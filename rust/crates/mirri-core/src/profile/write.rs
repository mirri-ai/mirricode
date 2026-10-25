//! Line-level surgical rewrite for agent `.md` profile files — the R4-1
//! write channel's round-trip-fidelity core. Pure functions, zero IO.
//!
//! [`ProfileManager::upsert_profile`](super::ProfileManager::upsert_profile)
//! persists profiles as `.md` files under the user agents root. Rewriting an
//! existing file through the map-level
//! [`serialize_frontmatter`](super::agent_md::serialize_frontmatter) would
//! lose everything the parser does not model — comments, blank lines,
//! unknown keys, the file's own quoting style, its inline-list spellings.
//! This module instead rewrites **only the line span of each modified key**
//! ([`rewrite_agent_md`]); every other byte of the file — unknown keys,
//! comments, blank lines, the Markdown body — passes through verbatim.
//!
//! # How a key's line span is found (the surgical boundary)
//!
//! A key line is a column-0 `key:` line with a bare `[A-Za-z0-9_-]` key (the
//! same shape [`parse_frontmatter`](super::agent_md::parse_frontmatter)
//! accepts). Its span is the key line plus the block-list items bound to it
//! — list-item lines plus the blank/comment lines interleaved between them
//! — minus any **trailing** blank/comment lines, which belong to the next
//! entry as separators. Consequence, stated honestly: a comment that sits
//! *between* a key and its list items is rewritten away with the key (it is
//! part of that field's block); a comment above or below the block is kept
//! byte-for-byte.
//!
//! New values are serialized with [`dump_entry`](super::agent_md::dump_entry)
//! — exactly what a full [`serialize_agent_file`] emission produces — using
//! the file's own line ending (`\r\n` when the first line carries it), so a
//! rewritten key cannot introduce mixed endings.
//!
//! Setting a key that has a snake_case alias (`defaultModel` /
//! `default_model`, `disallowedTools` / `disallowed_tools`,
//! `capabilitiesRequired` / `capabilities_required`) removes the alias span
//! first: a field never ends up with two spellings. No override-declaration
//! key is ever emitted (adjudication D14 — same-name files override by
//! virtue of existing; there is no `override: true`).

use super::AgentProfile;
use super::agent_md::{
    AgentFileDef, FRONTMATTER_FENCE, FrontmatterError, FrontmatterValue, dump_entry, split_key_line,
};

/// The body a profile without an inline system prompt **used to be**
/// persisted with — the TS `serializeAgentMd` placeholder (`'You are a
/// helpful AI assistant.'`). Retained for the discovery-layer placeholder
/// migration (ADR-010 ④): a file whose body is exactly this text is
/// rewritten as the composed base-reference shape.
pub const DEFAULT_PROMPT_PLACEHOLDER: &str = "You are a helpful AI assistant.";

/// The base-prompt reference token a composed profile body opens with
/// (the TS `builtinOverrideBody` convention — `agentProfileManagementService.ts`).
pub const BASE_PROMPT_REF: &str = "${base_prompt}";

/// The composed-body separator between the reference and the role text.
const BASE_PROMPT_SEPARATOR: &str = "\n\n";

/// A profile body composed onto the shared base: the bare reference when no
/// role text rides, else the reference then the role text (ADR-010 ①).
pub fn composed_body(role_text: Option<&str>) -> String {
    match role_text.map(str::trim).filter(|text| !text.is_empty()) {
        None => BASE_PROMPT_REF.to_string(),
        Some(text) => format!("{BASE_PROMPT_REF}{BASE_PROMPT_SEPARATOR}{text}"),
    }
}

/// Whether a file body carries the base reference — the single composition
/// discriminator (composed vs full-replacement, ADR-010 ① 追记
/// 2026-10-22): composed = the two shapes the write channels produce —
/// the body **starts with** `${base_prompt}\n\n`, or **is exactly** the
/// bare reference (after trim). A mid-text mention is a full replacement:
/// the token still expands at runtime (a template variable, the TS
/// `renderPromptTemplate` includes-based binding), but the body is the
/// user's own full prompt, never rewritten as composed.
pub fn is_composed_body(body: &str) -> bool {
    body.starts_with(&format!("{BASE_PROMPT_REF}{BASE_PROMPT_SEPARATOR}"))
        || body.trim() == BASE_PROMPT_REF
}

/// Strip the leading `${base_prompt}` prefix — the editable role text
/// projection: the `${base_prompt}\n\n` form yields the role text, any
/// other body **starting with** the bare reference yields the empty
/// string (the TS `stripBuiltinBasePromptRef` shape: `${base_prompt}\nRole`
/// strips to `''`, so the wire row emits no template key — no leading
/// newline leaks into the editable view), and a body without the prefix
/// passes through verbatim (a hand-written template that merely mentions
/// the token is not stripped).
pub fn strip_base_prompt_ref(body: &str) -> &str {
    if let Some(role) = body.strip_prefix(&format!("{BASE_PROMPT_REF}{BASE_PROMPT_SEPARATOR}")) {
        role
    } else if body.starts_with(BASE_PROMPT_REF) {
        ""
    } else {
        body
    }
}

/// A value the write channel stores into one frontmatter key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldValue {
    /// A string scalar.
    Str(String),
    /// A boolean scalar (`enabled: false`).
    Bool(bool),
    /// A block-style string list.
    List(Vec<String>),
}

impl From<FieldValue> for FrontmatterValue {
    fn from(value: FieldValue) -> Self {
        match value {
            FieldValue::Str(value) => FrontmatterValue::Str(value),
            FieldValue::Bool(value) => FrontmatterValue::Bool(value),
            FieldValue::List(items) => FrontmatterValue::List(items),
        }
    }
}

/// One frontmatter edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldEdit {
    /// Replace the key's line span — or insert it just before the closing
    /// fence when absent — with the freshly serialized value. A same-meaning
    /// alias key is removed first, and duplicate spans of either spelling
    /// are collapsed into the replacement.
    Set {
        /// The frontmatter key (the canonical camelCase spelling).
        key: &'static str,
        /// The value to store.
        value: FieldValue,
    },
    /// Remove the key's line span (the key line plus its bound list items).
    Remove {
        /// The frontmatter key (either spelling; the alias goes too).
        key: &'static str,
    },
}

/// The rewrite plan for one agent `.md` file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentMdRewrite {
    /// The frontmatter edits, applied in order.
    pub field_edits: Vec<FieldEdit>,
    /// When `Some`, replaces the whole Markdown body after the closing
    /// fence; when `None`, the body passes through verbatim.
    pub body: Option<String>,
}

/// (canonical key, snake_case alias) pairs — the `.md` reader resolves
/// `primary ?? alias`, so a write through the canonical spelling removes
/// the alias first: a field never ends up with two spellings.
const KEY_ALIASES: &[(&str, &str)] = &[
    ("defaultModel", "default_model"),
    ("disallowedTools", "disallowed_tools"),
    ("capabilitiesRequired", "capabilities_required"),
];

/// The key plus its alias spelling, when one exists.
fn key_variants(key: &str) -> Vec<&str> {
    let mut variants = vec![key];
    if let Some((_, alias)) = KEY_ALIASES.iter().find(|(canonical, _)| *canonical == key) {
        variants.push(alias);
    }
    variants
}

/// Apply `rewrite` to an agent `.md` file's text, byte-preserving everything
/// outside the edited key spans (and the body, when [`AgentMdRewrite::body`]
/// is `None`).
///
/// Errors when the text has no leading frontmatter fence or no closing
/// fence — a file that cannot carry frontmatter has nothing to surgically
/// edit (the caller falls back to a full serialization).
pub fn rewrite_agent_md(text: &str, rewrite: &AgentMdRewrite) -> Result<String, FrontmatterError> {
    let mut lines = split_lines_keep_ends(text);
    // Validate the frontmatter structure upfront (not lazily per edit), so
    // the error contract holds for an empty rewrite too.
    find_close_fence(&lines)?;
    for edit in &rewrite.field_edits {
        apply_field_edit(&mut lines, edit)?;
    }
    match &rewrite.body {
        // The body replacement is the whole text after the closing fence:
        // keep everything through the fence (guaranteeing its terminator)
        // and append the new body verbatim.
        Some(body) => {
            let close = find_close_fence(&lines)?;
            let needs_terminator = !ends_with_newline(&lines[close]);
            let newline = newline_of(&lines).to_string();
            if needs_terminator {
                lines[close].push_str(&newline);
            }
            lines.truncate(close + 1);
            Ok(lines.concat() + body)
        }
        None => Ok(lines.concat()),
    }
}

/// The surgical edit set for rewriting the **parsed** file `existing`
/// into `profile` — the `.md`-persisted field surface (the unified
/// [`AgentFileDef`] schema).
///
/// Fields the `.md` format cannot carry (provider, loop controls,
/// permission mode, thinking effort, prompt vars, …) are not represented —
/// the map-level [`From<AgentFileDef>`](super::AgentProfile) conversion
/// defines the round-trippable set, and this is its exact inverse.
///
/// Diff-narrowed (T23-m1): a known key whose parsed value already equals
/// the target value is left entirely alone — a one-field change rewrites
/// only that key's line span, so the file's own quote style, its trailing
/// comments, its alias spelling (`default_model` vs `defaultModel`) and
/// its inline-list shapes survive verbatim. A key the profile no longer
/// carries is still [`FieldEdit::Remove`]d (a no-op when absent), and a
/// `None`/`Some(true)` `enabled` **removes** the key (the flag persists
/// only when `Some(false)`, the TS semantics) unless the file already
/// carries exactly that. A profile without an inline system prompt — or
/// with a blank one — leaves the file's Markdown body untouched (the
/// merge layer fills it from the existing definition before the write
/// channel sees it; writing a blank body would produce a file the parser
/// rejects).
///
/// `prompt_cleared` marks an **explicit** prompt clear (the update
/// channel's empty-string template, M2): only then does an existing
/// composed body fall back to the bare base reference. A `None` prompt
/// WITHOUT the flag (a description-only update of a profile whose
/// registered definition carries no inline prompt — e.g. a disabled
/// composed override the dispatcher merges from the builtin) preserves
/// the file's composed body verbatim.
pub fn agent_md_rewrite_for_existing(
    existing: &AgentFileDef,
    profile: &AgentProfile,
    prompt_cleared: bool,
) -> AgentMdRewrite {
    let mut field_edits = Vec::new();
    set_str_if_changed(&mut field_edits, "name", &profile.name, &existing.name);
    set_str_if_changed(
        &mut field_edits,
        "description",
        &profile.description.clone().unwrap_or_default(),
        &existing.description,
    );
    set_optional_str_if_changed(
        &mut field_edits,
        "whenToUse",
        &profile.when_to_use,
        &existing.when_to_use,
    );
    set_optional_str_if_changed(
        &mut field_edits,
        "defaultModel",
        &effective_model(profile),
        &existing.default_model,
    );
    set_optional_list_if_changed(&mut field_edits, "tools", &profile.tools, &existing.tools);
    set_optional_list_if_changed(
        &mut field_edits,
        "disallowedTools",
        &profile.disallowed_tools,
        &existing.disallowed_tools,
    );
    set_optional_list_if_changed(
        &mut field_edits,
        "subagents",
        &profile.subagents,
        &existing.subagents,
    );
    set_optional_list_if_changed(
        &mut field_edits,
        "capabilitiesRequired",
        &profile.capabilities_required,
        &existing.capabilities_required,
    );
    // The flag persists only when `Some(false)` (the TS semantics): an
    // absent or explicitly-true flag removes the key — and a file already
    // carrying `enabled: false` is left alone like every other unchanged
    // value.
    if profile.enabled == Some(false) {
        if existing.enabled != Some(false) {
            field_edits.push(FieldEdit::Set {
                key: "enabled",
                value: FieldValue::Bool(false),
            });
        }
    } else {
        field_edits.push(FieldEdit::Remove { key: "enabled" });
    }
    AgentMdRewrite {
        field_edits,
        body: replacement_body(existing, profile, prompt_cleared),
    }
}

/// The body replacement: the non-empty inline prompt — composed onto the
/// base reference when it is plain text (SDR-014 ①, the B1 regression:
/// editing the prompt of an existing override must keep it composed) and
/// passed through verbatim when already composed — and only when it
/// differs from the parsed body. An absent or blank prompt never rewrites
/// the body into a blank — writing an empty one would produce a file the
/// parser rejects. An **explicit** clear (`prompt_cleared`, the update
/// channel's empty-string template) rewrites a composed body to the bare
/// reference, so it still lands on the base reference instead of leaving
/// stale role text behind (SDR-014 ②); an absent prompt WITHOUT the flag
/// (a field-only update merged from a prompt-less registered definition)
/// leaves even a composed body alone (M2: the file's role text is the
/// user's, not the dispatcher's to destroy).
fn replacement_body(
    existing: &AgentFileDef,
    profile: &AgentProfile,
    prompt_cleared: bool,
) -> Option<String> {
    match profile
        .system_prompt
        .as_deref()
        .map(str::trim)
        .filter(|prompt| !prompt.is_empty())
    {
        Some(prompt) => {
            // A composed (prefix-recognized) prompt passes through
            // verbatim — no double wrap. A full-replacement body (a
            // mid-text token mention included) ALSO passes through
            // verbatim: wrapping it would silently turn the user's own
            // whole prompt into a trailing role paragraph (M1 — only the
            // plain-text-to-composed wrap applies, never the reverse).
            let body = if is_composed_body(prompt) || prompt.contains(BASE_PROMPT_REF) {
                prompt.to_string()
            } else {
                composed_body(Some(prompt))
            };
            (body != existing.prompt).then_some(body)
        }
        None => {
            // An explicit clear (the dispatcher dropped the inline prompt
            // on a `""` template): the file lands on the bare reference —
            // composed bodies drop their role text, replacement bodies
            // drop the whole prompt (SDR-014 ②④: both channels clear to
            // the shared base). A prompt-less update WITHOUT the flag
            // keeps whatever body is there (M2).
            let bare = composed_body(None);
            (prompt_cleared && existing.prompt != bare).then_some(bare)
        }
    }
}

/// Push a `Set` for a required scalar key — unless the file already
/// carries exactly that value (T23-m1: unchanged keys are not rewritten,
/// so their spelling survives verbatim).
fn set_str_if_changed(edits: &mut Vec<FieldEdit>, key: &'static str, value: &str, existing: &str) {
    if value != existing {
        edits.push(FieldEdit::Set {
            key,
            value: FieldValue::Str(value.to_string()),
        });
    }
}

/// Push a `Set` (or `Remove`, when the profile drops the field) for an
/// optional scalar key — unless the parsed value already matches.
fn set_optional_str_if_changed(
    edits: &mut Vec<FieldEdit>,
    key: &'static str,
    value: &Option<String>,
    existing: &Option<String>,
) {
    if value != existing {
        edits.push(match value {
            Some(value) => FieldEdit::Set {
                key,
                value: FieldValue::Str(value.clone()),
            },
            None => FieldEdit::Remove { key },
        });
    }
}

/// Push a `Set` (or `Remove`, when the profile drops the field) for an
/// optional list key — unless the parsed value already matches.
fn set_optional_list_if_changed(
    edits: &mut Vec<FieldEdit>,
    key: &'static str,
    value: &Option<Vec<String>>,
    existing: &Option<Vec<String>>,
) {
    if value != existing {
        edits.push(match value {
            Some(items) => FieldEdit::Set {
                key,
                value: FieldValue::List(items.clone()),
            },
            None => FieldEdit::Remove { key },
        });
    }
}

/// The full-file [`AgentFileDef`] an [`AgentProfile`] serializes to when it
/// has no existing `.md` (the new-file path — there are no original bytes
/// to preserve). The prompt always lands in the composed base-reference
/// shape (SDR-014 ②): an already-composed prompt passes through, a plain
/// one wraps onto the reference, and a prompt-less profile composes its
/// own role text (builtin task agents carry it in `prompt_vars`; the rest
/// compose the bare reference).
pub fn agent_file_def_for_profile(profile: &AgentProfile) -> AgentFileDef {
    AgentFileDef {
        name: profile.name.clone(),
        description: profile.description.clone().unwrap_or_default(),
        when_to_use: profile.when_to_use.clone(),
        default_model: effective_model(profile),
        tools: profile.tools.clone(),
        disallowed_tools: profile.disallowed_tools.clone(),
        subagents: profile.subagents.clone(),
        capabilities_required: profile.capabilities_required.clone(),
        enabled: profile.enabled,
        prompt: match profile.system_prompt.as_deref().map(str::trim) {
            // An already-composed prompt passes through verbatim (no double wrap).
            Some(prompt) if is_composed_body(prompt) => prompt.to_string(),
            // A full-replacement body (a mid-text token mention included)
            // passes through verbatim too — the discriminator is
            // write-channel shape, never a license to re-wrap the user's
            // own whole prompt (M1).
            Some(prompt) if prompt.contains(BASE_PROMPT_REF) => prompt.to_string(),
            // A plain inline prompt wraps onto the base reference (ADR-010 ①).
            Some(prompt) if !prompt.is_empty() => composed_body(Some(prompt)),
            // No inline prompt: the composed shape of the profile's own role text
            // (builtin task agents carry it in prompt_vars; the default agent and
            // prompt-less custom profiles compose the bare reference).
            _ => composed_body(
                profile
                    .prompt_vars
                    .as_ref()
                    .and_then(|vars| vars.get("role_additional"))
                    .map(String::as_str),
            ),
        },
        file_path: None,
        source: None,
    }
}

/// The single model notion the `.md` schema persists: the explicit
/// `default_model` when set, else the synced `model` (the
/// `From<AgentFileDef>` conversion keeps both in lockstep for file-sourced
/// profiles).
fn effective_model(profile: &AgentProfile) -> Option<String> {
    profile
        .default_model
        .clone()
        .filter(|model| !model.is_empty())
        .or_else(|| (!profile.model.is_empty()).then(|| profile.model.clone()))
}

// ---------------------------------------------------------------------------
// Line-level machinery
// ---------------------------------------------------------------------------

/// A frontmatter key's line span: the `key: …` line plus the block-list
/// items bound to it. `end` is exclusive.
struct KeySpan {
    key: String,
    start: usize,
    end: usize,
}

/// Apply one field edit to the line vector.
fn apply_field_edit(lines: &mut Vec<String>, edit: &FieldEdit) -> Result<(), FrontmatterError> {
    let newline = newline_of(lines).to_string();
    let close = find_close_fence(lines)?;
    let (key, value) = match edit {
        FieldEdit::Set { key, value } => (*key, Some(value)),
        FieldEdit::Remove { key } => (*key, None),
    };
    let variants = key_variants(key);
    let spans: Vec<KeySpan> = key_spans(lines, 1..close)
        .into_iter()
        .filter(|span| variants.contains(&span.key.as_str()))
        .collect();

    match value {
        Some(value) => {
            // Serialize the fresh key line(s) exactly like a full-file
            // emission would, with the file's own line ending.
            let mut dumped: Vec<String> = Vec::new();
            dump_entry(key, &value.clone().into(), &mut dumped);
            let rendered: Vec<String> = dumped
                .iter()
                .map(|line| format!("{line}{newline}"))
                .collect();
            if let Some(first) = spans.first() {
                let (start, end) = (first.start, first.end);
                // Drain the later duplicate spans first (descending, so the
                // indices stay valid), then replace the first.
                for span in spans[1..].iter().rev() {
                    lines.drain(span.start..span.end);
                }
                lines.splice(start..end, rendered);
            } else {
                // Absent: insert just before the closing fence.
                lines.splice(close..close, rendered);
            }
        }
        None => {
            for span in spans.iter().rev() {
                lines.drain(span.start..span.end);
            }
        }
    }
    Ok(())
}

/// Scan the frontmatter region for key spans (see the module docs for the
/// surgical boundary rules).
fn key_spans(lines: &[String], region: std::ops::Range<usize>) -> Vec<KeySpan> {
    let mut spans = Vec::new();
    let mut index = region.start;
    while index < region.end {
        let content = line_content(&lines[index]);
        // Indented lines are list-item/continuation territory, never keys.
        if content.starts_with([' ', '\t']) {
            index += 1;
            continue;
        }
        let Some((key, _)) = split_key_line(content) else {
            index += 1;
            continue;
        };
        if !is_bare_key(key) {
            index += 1;
            continue;
        }
        // Extend over the block list bound to the key: list items plus the
        // blank/comment lines interleaved between them. Trailing blank and
        // comment lines are handed back to the next key (they separate
        // entries, they do not belong to this one).
        let mut end = index + 1;
        while end < region.end && is_listish(line_content(&lines[end])) {
            end += 1;
        }
        while end > index + 1 && !is_list_item(line_content(&lines[end - 1])) {
            end -= 1;
        }
        spans.push(KeySpan {
            key: key.trim().to_string(),
            start: index,
            end,
        });
        index = end;
    }
    spans
}

/// A bare `[A-Za-z0-9_-]+` key (the parser's key grammar).
fn is_bare_key(key: &str) -> bool {
    let key = key.trim();
    !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// A block-list item line (`- item` at any indentation the parser accepts).
fn is_list_item(content: &str) -> bool {
    let trimmed = content.trim();
    trimmed == "-" || trimmed.starts_with("- ")
}

/// A line that may sit inside a key's block list: a list item, a blank
/// line, or a comment interleaved between items.
fn is_listish(content: &str) -> bool {
    let trimmed = content.trim();
    trimmed.is_empty() || trimmed.starts_with('#') || is_list_item(content)
}

/// The index of the closing fence line, having verified the leading one.
fn find_close_fence(lines: &[String]) -> Result<usize, FrontmatterError> {
    if lines.first().is_none_or(|line| !is_fence(line)) {
        return Err(FrontmatterError::new(
            "no leading frontmatter fence to rewrite",
        ));
    }
    (1..lines.len())
        .find(|&index| is_fence(&lines[index]))
        .ok_or_else(|| FrontmatterError::new("Missing closing frontmatter fence"))
}

fn is_fence(line: &str) -> bool {
    line_content(line).trim() == FRONTMATTER_FENCE
}

/// The file's line ending: `\r\n` when the first line carries it, else `\n`.
fn newline_of(lines: &[String]) -> &str {
    if lines.first().is_some_and(|line| line.ends_with("\r\n")) {
        "\r\n"
    } else {
        "\n"
    }
}

fn ends_with_newline(line: &str) -> bool {
    line.ends_with('\n')
}

/// The line content without its terminator (the last line of a file may
/// have none).
fn line_content(line: &str) -> &str {
    line.strip_suffix('\n')
        .map(|content| content.strip_suffix('\r').unwrap_or(content))
        .unwrap_or(line)
}

/// Split `text` into lines, each keeping its terminator so untouched lines
/// re-concatenate byte-for-byte.
fn split_lines_keep_ends(text: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    for character in text.chars() {
        current.push(character);
        if character == '\n' {
            lines.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    lines
}

// ---------------------------------------------------------------------------
// Tests — vector-level, byte-exact. The pure function is the R4-1 acceptance
// core: every case pins exact input → output strings, so comment placement,
// quoting style, blank lines, CRLF endings and list shapes are locked.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::parse_agent_file_text;
    use std::collections::HashMap;

    fn set(key: &'static str, value: FieldValue) -> FieldEdit {
        FieldEdit::Set { key, value }
    }

    fn remove(key: &'static str) -> FieldEdit {
        FieldEdit::Remove { key }
    }

    fn str_value(value: &str) -> FieldValue {
        FieldValue::Str(value.to_string())
    }

    fn rewrite(text: &str, edits: Vec<FieldEdit>) -> String {
        rewrite_agent_md(
            text,
            &AgentMdRewrite {
                field_edits: edits,
                body: None,
            },
        )
        .expect("the rewrite succeeds")
    }

    /// Given a file carrying `enabled: false`, unknown keys with comments,
    /// and a custom body, When one known field changes, Then every other
    /// byte is preserved (the AC1 fidelity core).
    #[test]
    fn should_preserve_every_untouched_byte_when_one_known_field_changes() {
        let original = "---\n\
             name: reviewer\n\
             description: Old text\n\
             # curated by hand — do not reformat\n\
             unknown_key: keep me\n\
             whenToUse: 'It''s quoted'\n\
             enabled: false\n\
             \n\
             # the tool allowlist\n\
             tools:\n\
               - Read\n\
               - Grep\n\
             ---\n\
             \n\
             You are a reviewer.\n\
             Keep this body byte-for-byte.\n";

        let rewritten = rewrite(original, vec![set("description", str_value("New text"))]);

        let expected = original.replace("description: Old text\n", "description: New text\n");
        assert_eq!(rewritten, expected);
    }

    /// Given a file with CRLF line endings, When a known field changes,
    /// Then the rewritten line carries CRLF too and the rest stays CRLF.
    #[test]
    fn should_keep_crlf_endings_when_rewriting_a_crlf_file() {
        let original =
            "---\r\nname: reviewer\r\ndescription: Old\r\ntools:\r\n  - Read\r\n---\r\nBody.\r\n";

        let rewritten = rewrite(original, vec![set("description", str_value("New"))]);

        assert_eq!(
            rewritten,
            "---\r\nname: reviewer\r\ndescription: New\r\ntools:\r\n  - Read\r\n---\r\nBody.\r\n"
        );
    }

    /// Given a key stored as a block list, When it is replaced with a new
    /// list, Then only the key's own span (key line + items) is rewritten —
    /// the following key and its preceding separator survive verbatim.
    #[test]
    fn should_replace_only_the_key_span_when_replacing_a_block_list() {
        let original =
            "---\nname: r\ndescription: d\n\ntools:\n  - Read\n\nwhenToUse: later\n---\nBody.";

        let rewritten = rewrite(
            original,
            vec![set("tools", FieldValue::List(vec!["A".into(), "B".into()]))],
        );

        assert_eq!(
            rewritten,
            "---\nname: r\ndescription: d\n\ntools:\n  - A\n  - B\n\nwhenToUse: later\n---\nBody."
        );
    }

    /// Given a key stored as a comma-separated string (the inline-list
    /// spelling the parser accepts), When it is replaced, Then the single
    /// line is replaced by the canonical block list.
    #[test]
    fn should_replace_an_inline_list_spelling_with_a_block_list() {
        let original = "---\nname: r\ndescription: d\ntools: Read, Grep\n---\nBody.";

        let rewritten = rewrite(
            original,
            vec![set(
                "tools",
                FieldValue::List(vec!["Read".into(), "Grep".into()]),
            )],
        );

        assert_eq!(
            rewritten,
            "---\nname: r\ndescription: d\ntools:\n  - Read\n  - Grep\n---\nBody."
        );
    }

    /// Given a scalar key, When it is replaced by a list, Then the new block
    /// list is emitted and the next key is not swallowed.
    #[test]
    fn should_replace_a_scalar_with_a_block_list_without_swallowing_the_next_key() {
        let original = "---\nname: r\ndescription: d\nwhenToUse: old\nenabled: false\n---\nBody.";

        let rewritten = rewrite(
            original,
            vec![set(
                "whenToUse",
                FieldValue::Str("whenever lists are replaced".to_string()),
            )],
        );

        assert_eq!(
            rewritten,
            "---\nname: r\ndescription: d\nwhenToUse: whenever lists are replaced\nenabled: false\n---\nBody."
        );
    }

    /// Given a key with a trailing comment on its own value line and no list
    /// following, When the key is replaced, Then the comment goes with the
    /// line (it is attached to the modified key, not an untouched one).
    #[test]
    fn should_rewrite_the_value_line_with_its_trailing_comment_attached() {
        let original = "---\nname: r\ndescription: d\ntools: # none yet\n---\nBody.";

        let rewritten = rewrite(
            original,
            vec![set("tools", FieldValue::List(vec!["Read".into()]))],
        );

        assert_eq!(
            rewritten,
            "---\nname: r\ndescription: d\ntools:\n  - Read\n---\nBody."
        );
    }

    /// Given a key between a key and its list items, When the key is
    /// replaced, Then the comment is rewritten away with the span (it is
    /// part of the field's block); comments above and below survive.
    #[test]
    fn should_rewrite_a_comment_inside_a_replaced_list_span_but_keep_outside_ones() {
        let original = "---\n# about tools\nname: r\ndescription: d\ntools:\n# inside the list\n  - Read\n# after the list\nwhenToUse: later\n---\nBody.";

        let rewritten = rewrite(
            original,
            vec![set("tools", FieldValue::List(vec!["Grep".into()]))],
        );

        assert_eq!(
            rewritten,
            "---\n# about tools\nname: r\ndescription: d\ntools:\n  - Grep\n# after the list\nwhenToUse: later\n---\nBody."
        );
    }

    /// Given the enable flow (remove `enabled`), When the key is removed,
    /// Then the whole key line disappears and the surrounding bytes are
    /// untouched.
    #[test]
    fn should_remove_the_key_line_when_the_edit_removes_the_key() {
        let original = "---\nname: r\ndescription: d\nenabled: false\nwhenToUse: later\n---\nBody.";

        let rewritten = rewrite(original, vec![remove("enabled")]);

        assert_eq!(
            rewritten,
            "---\nname: r\ndescription: d\nwhenToUse: later\n---\nBody."
        );
    }

    /// Given a removed key that owns a block list, When it is removed, Then
    /// the key line and its items all go, and the next key survives.
    #[test]
    fn should_remove_the_key_and_its_list_items_together() {
        let original = "---\nname: r\ndescription: d\ntools:\n  - Read\n\n  - Grep\nwhenToUse: later\n---\nBody.";

        let rewritten = rewrite(original, vec![remove("tools")]);

        assert_eq!(
            rewritten,
            "---\nname: r\ndescription: d\nwhenToUse: later\n---\nBody."
        );
    }

    /// Given a key that is absent, When it is set, Then it is inserted just
    /// before the closing fence (valid YAML, appended at the end).
    #[test]
    fn should_insert_a_missing_key_before_the_closing_fence() {
        let original = "---\nname: r\ndescription: d\n---\nBody.";

        let rewritten = rewrite(
            original,
            vec![set("defaultModel", str_value("claude-sonnet"))],
        );

        assert_eq!(
            rewritten,
            "---\nname: r\ndescription: d\ndefaultModel: claude-sonnet\n---\nBody."
        );
    }

    /// Given a snake_case alias spelling, When the canonical key is set,
    /// Then the alias span is removed so the field has one spelling only.
    #[test]
    fn should_remove_the_alias_spelling_when_the_canonical_key_is_set() {
        let original = "---\nname: r\ndescription: d\ndefault_model: old\n---\nBody.";

        let rewritten = rewrite(original, vec![set("defaultModel", str_value("new"))]);

        assert_eq!(
            rewritten,
            "---\nname: r\ndescription: d\ndefaultModel: new\n---\nBody."
        );
    }

    /// Given duplicate key spans (a hand-edited file the parser would
    /// reject, but the rewriter must not corrupt), When the key is set,
    /// Then the first span is replaced and the later duplicates removed.
    #[test]
    fn should_collapse_duplicate_key_spans_when_the_key_is_set() {
        let original = "---\nname: r\ndescription: first\ndescription: second\n---\nBody.";

        let rewritten = rewrite(original, vec![set("description", str_value("only"))]);

        assert_eq!(rewritten, "---\nname: r\ndescription: only\n---\nBody.");
    }

    /// Given a custom body, When the rewrite carries a body, Then everything
    /// after the closing fence is replaced — and the fence keeps a
    /// terminator even when the original ended bare.
    #[test]
    fn should_replace_the_body_after_the_closing_fence() {
        let original = "---\nname: r\ndescription: d\n---\nOld body.";
        let rewrite_plan = AgentMdRewrite {
            field_edits: vec![set("description", str_value("d"))],
            body: Some("New body.\n".to_string()),
        };

        let rewritten = rewrite_agent_md(original, &rewrite_plan).expect("the rewrite succeeds");

        assert_eq!(rewritten, "---\nname: r\ndescription: d\n---\nNew body.\n");
    }

    /// Given an empty rewrite, When it is applied, Then the output is
    /// byte-identical to the input (the identity property).
    #[test]
    fn should_return_the_text_unchanged_when_the_rewrite_is_empty() {
        let original = "---\r\nname: r\r\ndescription: d\r\n# comment\r\nunknown: 1\r\n---\r\n\r\nBody with\r\nCRLF.\r\n";

        let rewritten = rewrite_agent_md(original, &AgentMdRewrite::default())
            .expect("the empty rewrite succeeds");

        assert_eq!(rewritten, original);
    }

    /// Given a text without a leading fence, When it is rewritten, Then the
    /// error says so (the caller falls back to full serialization).
    #[test]
    fn should_error_when_the_text_has_no_leading_frontmatter_fence() {
        let error = rewrite_agent_md("Just a body.\n", &AgentMdRewrite::default())
            .expect_err("a fence-less text cannot be surgically rewritten");

        assert!(error.message.contains("frontmatter fence"));
    }

    /// Given a text whose fence never closes, When it is rewritten, Then the
    /// error says so.
    #[test]
    fn should_error_when_the_frontmatter_fence_never_closes() {
        let error = rewrite_agent_md("---\nname: r\n", &AgentMdRewrite::default())
            .expect_err("an unterminated fence cannot be surgically rewritten");

        assert!(error.message.contains("closing frontmatter fence"));
    }

    /// Given a rewritten file, When it is re-parsed, Then the edited values
    /// are what the parser sees and the unknown keys survive the parse
    /// (unknown keys never error).
    #[test]
    fn should_round_trip_the_edited_values_through_the_parser() {
        let original = "---\nname: reviewer\ndescription: Old\nunknown_key: keep\ntools:\n  - Read\n---\nBody.";
        let rewritten = rewrite(
            original,
            vec![
                set("description", str_value("New")),
                set(
                    "tools",
                    FieldValue::List(vec!["Grep".into(), "Glob".into()]),
                ),
                remove("enabled"),
            ],
        );

        let def = parse_agent_file_text("/agents/reviewer.md", &rewritten)
            .expect("the rewritten file parses");
        assert_eq!(def.description, "New");
        assert_eq!(
            def.tools,
            Some(vec!["Grep".to_string(), "Glob".to_string()])
        );
        assert_eq!(def.prompt, "Body.");
    }

    /// Given a quoted value style, When an untouched key keeps its quoted
    /// spelling, Then the bytes are preserved — only the rewritten key gets
    /// the canonical dump quoting.
    #[test]
    fn should_keep_the_files_own_quoting_for_untouched_keys() {
        let original = "---\nname: \"reviewer\"\ndescription: 'Old, with comma'\n---\nBody.";

        let rewritten = rewrite(original, vec![set("name", str_value("reviewer-2"))]);

        assert_eq!(
            rewritten,
            "---\nname: reviewer-2\ndescription: 'Old, with comma'\n---\nBody."
        );
    }

    // --- profile → edit set / def mapping --------------------------------

    /// Given a full mappable profile and an empty existing def, When the
    /// edit set is derived, Then every mappable field is Set (or removed
    /// when absent) exactly as the full serializer would emit it, and no
    /// override-declaration key appears (D14).
    #[test]
    fn should_derive_the_edit_set_from_a_full_profile() {
        let profile = AgentProfile {
            name: "reviewer".to_string(),
            model: "claude-sonnet-4".to_string(),
            description: Some("Reviews.".to_string()),
            when_to_use: Some("When reviewing.".to_string()),
            system_prompt: Some("You review.".to_string()),
            tools: Some(vec!["Read".to_string()]),
            disallowed_tools: Some(vec!["Bash".to_string()]),
            subagents: Some(vec!["coder".to_string()]),
            capabilities_required: Some(vec!["code.read".to_string()]),
            enabled: Some(false),
            ..Default::default()
        };

        let rewrite = agent_md_rewrite_for_existing(&AgentFileDef::default(), &profile, false);

        assert_eq!(
            rewrite.field_edits,
            vec![
                set("name", str_value("reviewer")),
                set("description", str_value("Reviews.")),
                set("whenToUse", str_value("When reviewing.")),
                set("defaultModel", str_value("claude-sonnet-4")),
                set("tools", FieldValue::List(vec!["Read".to_string()])),
                set(
                    "disallowedTools",
                    FieldValue::List(vec!["Bash".to_string()])
                ),
                set("subagents", FieldValue::List(vec!["coder".to_string()])),
                set(
                    "capabilitiesRequired",
                    FieldValue::List(vec!["code.read".to_string()])
                ),
                set("enabled", FieldValue::Bool(false)),
            ]
        );
        // SDR-014 ①: a plain inline prompt composes onto the base reference.
        assert_eq!(
            rewrite.body,
            Some("${base_prompt}\n\nYou review.".to_string())
        );
        // D14: no override-declaration key is ever emitted.
        assert!(
            !rewrite
                .field_edits
                .iter()
                .any(|edit| matches!(edit, FieldEdit::Set { key, .. } if *key == "override"))
        );
    }

    /// Given a profile with optional fields absent and an empty existing
    /// def, When the edit set is derived, Then those keys are Removed,
    /// `Some(true)`/`None` `enabled` removes the flag (it persists only
    /// when false), and the already-matching empty description emits no
    /// edit at all (T23-m1: unchanged values are never rewritten).
    #[test]
    fn should_derive_removals_for_absent_optional_fields() {
        let profile = AgentProfile {
            name: "minimal".to_string(),
            model: String::new(),
            description: None,
            enabled: Some(true),
            ..Default::default()
        };

        let rewrite = agent_md_rewrite_for_existing(&AgentFileDef::default(), &profile, false);

        assert_eq!(
            rewrite.field_edits,
            vec![set("name", str_value("minimal")), remove("enabled"),]
        );
        // No inline system prompt: the body is left untouched (the caller's
        // merge layer fills it from the existing definition).
        assert_eq!(rewrite.body, None);
    }

    /// Given an existing def whose values already match the profile except
    /// one, When the edit set is derived, Then only the changed key is Set
    /// — every already-matching key (including one stored under its
    /// snake_case alias) is skipped, so its spelling survives verbatim
    /// (T23-m1, the rewrite-narrowing acceptance core).
    #[test]
    fn should_only_edit_keys_whose_values_actually_changed() {
        let existing = AgentFileDef {
            name: "reviewer".to_string(),
            description: "Reviews.".to_string(),
            when_to_use: Some("When reviewing.".to_string()),
            default_model: Some("claude-sonnet-4".to_string()),
            tools: Some(vec!["Read".to_string()]),
            disallowed_tools: Some(vec!["Bash".to_string()]),
            subagents: Some(vec!["coder".to_string()]),
            capabilities_required: Some(vec!["code.read".to_string()]),
            enabled: Some(false),
            prompt: "${base_prompt}\n\nYou review.".to_string(),
            file_path: None,
            source: None,
        };
        let profile = AgentProfile {
            name: "reviewer".to_string(),
            model: String::new(),
            default_model: Some("claude-sonnet-4".to_string()),
            description: Some("Reviews, carefully.".to_string()),
            when_to_use: Some("When reviewing.".to_string()),
            system_prompt: Some("You review.".to_string()),
            tools: Some(vec!["Read".to_string()]),
            disallowed_tools: Some(vec!["Bash".to_string()]),
            subagents: Some(vec!["coder".to_string()]),
            capabilities_required: Some(vec!["code.read".to_string()]),
            enabled: Some(false),
            ..Default::default()
        };

        let rewrite = agent_md_rewrite_for_existing(&existing, &profile, false);

        assert_eq!(
            rewrite.field_edits,
            vec![set("description", str_value("Reviews, carefully."))]
        );
        assert_eq!(
            rewrite.body, None,
            "an unchanged prompt leaves the body alone"
        );
    }

    /// Given a profile carrying a blank inline system prompt, When the edit
    /// set is derived against a file with a body, Then the body is left
    /// untouched — writing an empty body would produce a file the parser
    /// rejects (the "invisible profile" shape).
    #[test]
    fn should_leave_the_body_untouched_when_the_profile_prompt_is_blank() {
        let existing = AgentFileDef {
            name: "bare".to_string(),
            description: "Bare.".to_string(),
            prompt: "Meaningful body.".to_string(),
            ..Default::default()
        };
        let blank = AgentProfile {
            name: "bare".to_string(),
            model: String::new(),
            description: Some("Bare.".to_string()),
            system_prompt: Some("   ".to_string()),
            ..Default::default()
        };

        let rewrite = agent_md_rewrite_for_existing(&existing, &blank, false);

        // No key is *set* — only the flag removal (a no-op: the file does
        // not carry an `enabled` key) — and the body is left untouched.
        assert_eq!(rewrite.field_edits, vec![remove("enabled")]);
        assert_eq!(rewrite.body, None);
    }

    /// Given a profile with no inline system prompt, When the full-file def
    /// is derived (the new-file path), Then the body is the composed base
    /// reference (SDR-014 ②, superseding the placeholder fallback): a bare
    /// reference for the prompt-less shape, and the builtin role text when
    /// the profile carries one.
    #[test]
    fn should_compose_the_base_reference_for_new_files_without_a_prompt() {
        let profile = AgentProfile {
            name: "agent".to_string(),
            ..Default::default()
        };
        let def = agent_file_def_for_profile(&profile);
        assert_eq!(def.prompt, "${base_prompt}");

        let coder = AgentProfile {
            name: "coder".to_string(),
            prompt_vars: Some(HashMap::from([(
                "role_additional".to_string(),
                "You are a coder.".to_string(),
            )])),
            ..Default::default()
        };
        let def = agent_file_def_for_profile(&coder);
        assert_eq!(def.prompt, "${base_prompt}\n\nYou are a coder.");
    }

    /// Given a profile carrying an inline system prompt, When it is serialized
    /// as a new file, Then the body wraps the prompt onto the base reference.
    #[test]
    fn should_wrap_an_inline_prompt_onto_the_base_reference() {
        let profile = AgentProfile {
            name: "reviewer".to_string(),
            system_prompt: Some("You review code.".to_string()),
            ..Default::default()
        };
        let def = agent_file_def_for_profile(&profile);
        assert_eq!(def.prompt, "${base_prompt}\n\nYou review code.");
    }

    /// Given an inline prompt that is already the composed shape, When
    /// serialized, Then the body passes through without double wrapping.
    #[test]
    fn should_not_double_wrap_an_already_composed_prompt() {
        let profile = AgentProfile {
            name: "reviewer".to_string(),
            system_prompt: Some("${base_prompt}\n\nYou review.".to_string()),
            ..Default::default()
        };
        let def = agent_file_def_for_profile(&profile);
        assert_eq!(def.prompt, "${base_prompt}\n\nYou review.");
    }

    /// Given an existing override file whose prompt is edited to a new
    /// plain text, When the surgical rewrite is derived, Then the written
    /// body is the composed shape — never bare text (the B1 regression:
    /// editing the prompt of an existing override must keep it composed).
    #[test]
    fn should_compose_the_body_when_editing_the_prompt_of_an_existing_file() {
        let existing = AgentFileDef {
            name: "coder".to_string(),
            description: "Codes.".to_string(),
            prompt: "${base_prompt}\n\nOld role text.".to_string(),
            ..Default::default()
        };
        let profile = AgentProfile {
            name: "coder".to_string(),
            description: Some("Codes.".to_string()),
            system_prompt: Some("New role text.".to_string()),
            ..Default::default()
        };

        let rewrite = agent_md_rewrite_for_existing(&existing, &profile, false);

        assert_eq!(
            rewrite.body,
            Some("${base_prompt}\n\nNew role text.".to_string())
        );
    }
    // ===== ADR-010 ① 追记: prefix-based composition discriminator (M1) =====

    /// Given a body merely mentioning the token mid-text, When the
    /// composition discriminator runs, Then it is a FULL REPLACEMENT —
    /// only the two write-channel shapes (leading `${base_prompt}\n\n`,
    /// or the bare reference) count as composed.
    #[test]
    fn should_classify_a_mid_text_mention_as_a_full_replacement() {
        assert!(is_composed_body("${base_prompt}\n\nRole text."));
        assert!(is_composed_body("${base_prompt}"));
        assert!(is_composed_body("  ${base_prompt}  "));
        assert!(
            !is_composed_body("Preamble. ${base_prompt} then more text."),
            "a mid-text mention is a replacement: the user's own full prompt"
        );
        assert!(
            !is_composed_body("${base_prompt}\nRole"),
            "the non-canonical single-newline separator is not a write-channel shape"
        );
    }

    /// Given a mid-mention replacement body, When the write channel
    /// rewrites the file, Then the body is preserved verbatim (never
    /// re-wrapped as composed) — the wire template projects the full text
    /// and the runtime expands the token as a template variable.
    #[test]
    fn should_preserve_a_mid_mention_replacement_body_verbatim_on_update() {
        let existing = AgentFileDef {
            name: "hand".to_string(),
            description: "Hand.".to_string(),
            prompt: "Head. ${base_prompt} Tail.".to_string(),
            ..Default::default()
        };
        let profile = AgentProfile {
            name: "hand".to_string(),
            description: Some("Hand, edited.".to_string()),
            system_prompt: Some("Head. ${base_prompt} Tail.".to_string()),
            ..Default::default()
        };

        let rewrite = agent_md_rewrite_for_existing(&existing, &profile, false);

        // The body is byte-identical — no rewrap, no strip.
        assert_eq!(rewrite.body, None);

        // And an explicit prompt clear (the dispatcher drops the inline
        // prompt, so the write layer sees None + the flag) falls back to
        // the bare reference.
        let cleared = AgentProfile {
            system_prompt: None,
            ..profile.clone()
        };
        let rewrite = agent_md_rewrite_for_existing(&existing, &cleared, true);
        assert_eq!(rewrite.body, Some("${base_prompt}".to_string()));
    }

    // ===== m3: strip-to-whitespace projects as empty =====

    /// Given a body whose post-strip remainder is whitespace-only (the
    /// non-canonical `${base_prompt}\nRole` separator), When the editable
    /// role text is projected, Then the result is the EMPTY string — no
    /// leading-newline text leaks into the wire template (the TS
    /// `stripBuiltinBasePromptRef` alignment).
    #[test]
    fn should_project_a_whitespace_remainder_as_empty_when_stripping() {
        assert_eq!(
            strip_base_prompt_ref("${base_prompt}\n\nRole text."),
            "Role text."
        );
        assert_eq!(strip_base_prompt_ref("${base_prompt}"), "");
        assert_eq!(
            strip_base_prompt_ref("${base_prompt}\nRole"),
            "",
            "a whitespace-only remainder projects as empty"
        );
        assert_eq!(
            strip_base_prompt_ref("plain body"),
            "plain body",
            "a body without the prefix passes through verbatim"
        );
    }

    // ===== M2: an absent prompt never destroys a composed body =====

    /// Given an existing composed body and a prompt-LESS update profile
    /// WITHOUT the explicit-clear flag, When the surgical rewrite is
    /// derived, Then the composed body (its role text) survives verbatim
    /// — only a deliberate empty-string template (the flag) falls back to
    /// the bare reference.
    #[test]
    fn should_preserve_the_composed_body_when_a_promptless_update_does_not_clear() {
        let existing = AgentFileDef {
            name: "sleeper".to_string(),
            description: "Old.".to_string(),
            enabled: Some(false),
            prompt: "${base_prompt}\n\nPreserved role text.".to_string(),
            ..Default::default()
        };
        // A description-only update merged from a registered definition
        // that carries no inline prompt (e.g. the builtin def).
        let profile = AgentProfile {
            name: "sleeper".to_string(),
            description: Some("New.".to_string()),
            system_prompt: None,
            enabled: Some(false),
            ..Default::default()
        };

        let rewrite = agent_md_rewrite_for_existing(&existing, &profile, false);

        assert_eq!(rewrite.body, None, "the composed body rides untouched");
        // The explicit clear DOES fall back to the bare reference.
        let rewrite = agent_md_rewrite_for_existing(&existing, &profile, true);
        assert_eq!(rewrite.body, Some("${base_prompt}".to_string()));
    }
}
