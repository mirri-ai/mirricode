//! Agent profile `.md` file parsing and serialization — pure functions, zero IO.
//!
//! This is the rust port of the TS `@mirri/agent-profile` package's
//! `.md` handling (`parseFrontmatter` / `parseAgentFileText` /
//! `serializeAgentFile`), with field-by-field parity. A profile file is a
//! YAML frontmatter block (metadata) followed by a Markdown body (the system
//! prompt template). The legacy `.yaml`/`.yml` nested-fallback parsing from
//! the TS package is intentionally out of scope; the legacy rust `.toml`
//! profiles live in [`crate::profile::AgentProfile`] and are unrelated.
//!
//! # Supported YAML subset (adjudicated, R2-1)
//!
//! The workspace keeps a zero-YAML-crate dependency policy, so this module
//! implements a hand-written minimal frontmatter parser. The supported
//! subset is exactly what [`AgentFileDef`] consumes:
//!
//! - a flat top-level mapping (`key: value` lines, keys at column 0, bare
//!   `[A-Za-z0-9_-]+` keys only, no duplicates),
//! - string scalars — unquoted, single-quoted (`'…'` with `''` escaping), or
//!   double-quoted (`"…"` with `\\ \" \n \t \r \0 \uXXXX` escaping),
//! - booleans (`true/True/TRUE`, `false/False/FALSE`),
//! - null (`null/Null/NULL/~`, or an empty value),
//! - numbers (recognized so that they behave as *non-strings*, matching the
//!   TS `typeof` checks — e.g. `description: 123` is a parse error),
//! - block-style string lists (`key:` followed by `- item` lines; the dash
//!   may sit at any indentation, including column 0 like js-yaml accepts) —
//!   a value line that is only a trailing comment (`tools: # note`) binds
//!   the following block list exactly like an empty value does (js-yaml
//!   parity; R3-7 alignment),
//! - the empty flow sequence `[]`,
//! - blank lines, full-line comments and trailing `# comments`,
//! - CRLF line endings.
//!
//! Anything else is a **hard parse error** — nested mappings, block scalars
//! (`|` / `>`), flow collections (`{…}`, non-empty `[…]`), anchors and
//! aliases, tags, multi-document markers, folded multi-line plain scalars,
//! non-string list items, duplicate keys. No guessing: the error carries the
//! file path and a reason (the future discovery layer skip-collects them).
//!
//! # Known divergences from js-yaml (all deliberate, per adjudication)
//!
//! - js-yaml *accepts* nested mappings, block scalars, flow collections,
//!   anchors, tags and folded plain scalars; this parser rejects them.
//! - js-yaml accepts keys with spaces or quoted keys; this parser rejects
//!   them (bare keys only).
//! - js-yaml silently drops non-string list items in the TS `parseStringList`
//!   filters; this parser rejects them at frontmatter level (except `null`
//!   and empty items, which are dropped exactly like the TS filters do).
//! - Unquoted scalars containing `: ` are rejected (js-yaml also errors on
//!   these; quoted strings must be used instead).
//!
//! Unknown *keys* (e.g. legacy `extends`, `override`, `promptVars`,
//! `capabilities`) are silently ignored, matching the TS behavior — only
//! out-of-subset *syntax* is fatal.

use thiserror::Error;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Frontmatter text-level error (fence problems, out-of-subset YAML syntax).
///
/// Mirrors the TS `FrontmatterError`. Carries only a message; the file path is
/// attached by the [`AgentFileParseError`] wrapper in [`parse_agent_file_text`].
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct FrontmatterError {
    /// Human-readable reason (no file path — the caller adds context).
    pub message: String,
}

impl FrontmatterError {
    pub(super) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Agent profile file parse error.
///
/// Mirrors the TS `AgentFileParseError`. `reason` holds the full
/// human-readable message in the exact TS message format (which embeds the
/// path); `path` is kept as a structured field so the discovery layer can
/// skip-and-collect failures without re-parsing the message.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{reason}")]
pub struct AgentFileParseError {
    /// Path of the file that failed to parse.
    pub path: String,
    /// Full message in the TS format, e.g. `Missing frontmatter in <path>`.
    pub reason: String,
}

// ---------------------------------------------------------------------------
// Frontmatter value model
// ---------------------------------------------------------------------------

/// A parsed frontmatter scalar (or string list).
///
/// The variants model the JS `typeof` lattice that the TS field helpers
/// (`nonEmptyString`, `parseStringList`, …) branch on:
/// [`FrontmatterValue::Null`] and absent behave as `null`/`undefined`
/// (missing), [`FrontmatterValue::Bool`] and [`FrontmatterValue::Number`] as
/// non-string values, [`FrontmatterValue::Str`] as a string, and
/// [`FrontmatterValue::List`] as an array.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrontmatterValue {
    /// YAML `null` — behaves as a missing value in every field helper.
    Null,
    /// A YAML boolean.
    Bool(bool),
    /// A YAML number (kept as raw text; behaves as a non-string).
    Number(String),
    /// A string scalar.
    Str(String),
    /// A block-style string list.
    List(Vec<String>),
}

/// An insertion-ordered flat mapping of frontmatter keys to values.
///
/// Order is preserved so that [`serialize_frontmatter`] can round-trip a
/// parsed document.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FrontmatterMap {
    entries: Vec<(String, FrontmatterValue)>,
}

impl FrontmatterMap {
    /// Creates an empty map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the value for `key` (later insertions win).
    pub fn get(&self, key: &str) -> Option<&FrontmatterValue> {
        self.entries
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }

    /// Returns whether `key` is present.
    pub fn contains(&self, key: &str) -> bool {
        self.entries.iter().any(|(k, _)| k == key)
    }

    /// Inserts `key: value`, replacing an existing entry in place.
    pub fn insert(&mut self, key: &str, value: FrontmatterValue) {
        if let Some(slot) = self.entries.iter_mut().find(|(k, _)| k == key) {
            slot.1 = value;
        } else {
            self.entries.push((key.to_string(), value));
        }
    }

    /// Inserts a string scalar.
    pub fn insert_str(&mut self, key: &str, value: &str) {
        self.insert(key, FrontmatterValue::Str(value.to_string()));
    }

    /// Inserts a string list.
    pub fn insert_list(&mut self, key: &str, items: Vec<String>) {
        self.insert(key, FrontmatterValue::List(items));
    }

    /// Iterates the entries in insertion order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &FrontmatterValue)> {
        self.entries.iter().map(|(k, v)| (k.as_str(), v))
    }

    /// Returns whether the mapping has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns the number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Appends `item` to the list under `key`, upgrading a `null` (from a
    /// bare `key:` line) to a list — the block-list parse path.
    fn push_list_item(&mut self, key: &str, item: String) {
        if let Some(slot) = self.entries.iter_mut().find(|(k, _)| k == key) {
            if matches!(slot.1, FrontmatterValue::Null) {
                slot.1 = FrontmatterValue::List(vec![item]);
            } else if let FrontmatterValue::List(items) = &mut slot.1 {
                items.push(item);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Frontmatter split / join (TS frontmatter.ts)
// ---------------------------------------------------------------------------

/// A Markdown document split into its frontmatter block and body.
///
/// `data` is `None` when the document has no leading `---` fence (TS `null`),
/// and an empty map for `---\n---`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedFrontmatter {
    /// The parsed frontmatter mapping, or `None` when there is no fence.
    pub data: Option<FrontmatterMap>,
    /// Everything after the closing fence (line endings normalized to LF).
    pub body: String,
}

const FENCE: &str = "---";

// R4-1: the write channel's line-level rewriter (`profile/write.rs`) shares
// these two internals so a rewritten key serializes exactly like a
// full-file serialization would, and a "key line" means the same thing the
// parser means.
pub(super) const FRONTMATTER_FENCE: &str = FENCE;

/// Splits a Markdown document into frontmatter and body.
///
/// Mirrors the TS `parseFrontmatter`: a document without a leading `---`
/// fence parses as all body with `data: None`; an unterminated fence is a
/// [`FrontmatterError`]; `---\n---` yields an empty mapping.
pub fn parse_frontmatter(text: &str) -> Result<ParsedFrontmatter, FrontmatterError> {
    let lines: Vec<&str> = text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    if lines.first().map(|line| line.trim()) != Some(FENCE) {
        return Ok(ParsedFrontmatter {
            data: None,
            body: text.to_string(),
        });
    }

    let Some(close) = lines
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, line)| line.trim() == FENCE)
        .map(|(index, _)| index)
    else {
        return Err(FrontmatterError::new("Missing closing frontmatter fence"));
    };

    let yaml_text = lines[1..close].join("\n").trim().to_string();
    let body = lines[close + 1..].join("\n");
    if yaml_text.is_empty() {
        return Ok(ParsedFrontmatter {
            data: Some(FrontmatterMap::default()),
            body,
        });
    }
    let data = parse_frontmatter_map(&yaml_text)?;
    Ok(ParsedFrontmatter {
        data: Some(data),
        body,
    })
}

/// Renders a frontmatter map + body back into `---\n…\n---\n<body>` form.
///
/// Mirrors the TS `serializeFrontmatter` (including the trailing-newline
/// behavior: an empty body still ends `---\n`, a non-empty body is appended
/// verbatim after the closing fence).
pub fn serialize_frontmatter(data: &FrontmatterMap, body: &str) -> String {
    let mut lines: Vec<String> = Vec::new();
    for (key, value) in data.iter() {
        dump_entry(key, value, &mut lines);
    }
    let yaml_text = lines.join("\n");
    if body.is_empty() {
        format!("{FENCE}\n{yaml_text}\n{FENCE}\n")
    } else {
        format!("{FENCE}\n{yaml_text}\n{FENCE}\n{body}")
    }
}

// ---------------------------------------------------------------------------
// Agent file model (TS schema.ts)
// ---------------------------------------------------------------------------

/// Where an agent file was discovered from (TS `AgentFileSource`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentFileSource {
    /// Bundled with a plugin.
    Plugin,
    /// In the project's agent directory.
    Project,
    /// In the user's global agent directory.
    User,
    /// From an extra directory added at runtime.
    Extra,
    /// Explicitly loaded by path.
    Explicit,
}

/// The parsed definition of a single agent profile `.md` file
/// (TS `AgentFileDef`).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AgentFileDef {
    /// Required, kebab-case identifier, e.g. `"code-reviewer"`.
    pub name: String,
    /// Required, one-line description for UI display and LLM selection.
    pub description: String,
    /// Optional, LLM selection hint — when to use this profile.
    pub when_to_use: Option<String>,
    /// Optional, default model alias, e.g. `"claude-sonnet"`.
    pub default_model: Option<String>,
    /// Optional, tool allowlist (omitted = all tools available).
    pub tools: Option<Vec<String>>,
    /// Optional, tool denylist (omitted = no extra denials).
    pub disallowed_tools: Option<Vec<String>>,
    /// Optional, subagent profile names this agent may dispatch (omitted = all).
    pub subagents: Option<Vec<String>>,
    /// Optional, capability tags, e.g. `["code.explore", "code.read"]`.
    pub capabilities_required: Option<Vec<String>>,
    /// Optional, `false` = disabled (omitted = enabled). Persisted only when
    /// `Some(false)`; a string `"false"` is ignored.
    pub enabled: Option<bool>,
    /// System prompt template body — the Markdown body of the `.md` file.
    pub prompt: String,
    /// File path the definition was parsed from (only set during discovery).
    pub file_path: Option<String>,
    /// Discovery source of the file.
    pub source: Option<AgentFileSource>,
}

// ---------------------------------------------------------------------------
// Agent file parse / serialize (TS parse.ts / serialize.ts)
// ---------------------------------------------------------------------------

/// Maximum agent profile name length, in bytes (TS `AGENT_NAME_MAX_LENGTH`;
/// both counts coincide — the kebab pattern already restricts names to
/// ASCII). The name becomes a filename on write, so the cap keeps both
/// engines far from filesystem limits (`ENAMETOOLONG`) and turns an over-long
/// name into a clean parse error instead of a 500. The route-level create
/// guard reuses the same bound.
pub const AGENT_NAME_MAX_LENGTH: usize = 64;

/// Parses an agent profile `.md` file (frontmatter + Markdown body) into an
/// [`AgentFileDef`]. Pure function — zero IO.
///
/// Mirrors the TS `parseAgentFileText` for `.md` inputs: the name is taken
/// from the frontmatter or derived from the file path stem; `description` is
/// required; the trimmed Markdown body is the prompt; camelCase keys win over
/// their snake_case aliases (`defaultModel`/`default_model`,
/// `disallowedTools`/`disallowed_tools`,
/// `capabilitiesRequired`/`capabilities_required`).
pub fn parse_agent_file_text(path: &str, text: &str) -> Result<AgentFileDef, AgentFileParseError> {
    parse_agent_file(path, text, None)
}

/// Like [`parse_agent_file_text`], but stamps the discovery source onto the
/// result (TS `ParseAgentFileOptions.source`).
pub fn parse_agent_file_text_with_source(
    path: &str,
    text: &str,
    source: AgentFileSource,
) -> Result<AgentFileDef, AgentFileParseError> {
    parse_agent_file(path, text, Some(source))
}

fn parse_agent_file(
    path: &str,
    text: &str,
    source: Option<AgentFileSource>,
) -> Result<AgentFileDef, AgentFileParseError> {
    let fail = |reason: String| AgentFileParseError {
        path: path.to_string(),
        reason,
    };

    let parsed = parse_frontmatter(text)
        .map_err(|error| fail(format!("Invalid frontmatter in {path}: {}", error.message)))?;
    let data = parsed
        .data
        .ok_or_else(|| fail(format!("Missing frontmatter in {path}")))?;
    // `data` is always a flat mapping here, so the TS non-mapping check is
    // structurally unreachable — out-of-subset shapes already error above.

    // Name: explicit field, or derived from the filename (TS parity: a
    // non-string name errors before the derive, a null/empty one falls
    // through to the derive).
    if let Some(
        FrontmatterValue::Bool(_) | FrontmatterValue::Number(_) | FrontmatterValue::List(_),
    ) = data.get("name")
    {
        return Err(fail(format!(
            "Frontmatter field \"name\" in {path} must be a non-empty string"
        )));
    }
    let name = non_empty_string(data.get("name"))
        .or_else(|| derive_name_from_path(path))
        .ok_or_else(|| {
            fail(format!(
                "Missing required frontmatter field \"name\" in {path}"
            ))
        })?;
    if !is_kebab_case(&name) {
        return Err(fail(format!(
            "Invalid agent name \"{name}\" in {path}: expected kebab-case (e.g. \"code-reviewer\")"
        )));
    }
    if name.len() > AGENT_NAME_MAX_LENGTH {
        return Err(fail(format!(
            "Invalid agent name \"{name}\" in {path}: must be at most {AGENT_NAME_MAX_LENGTH} characters"
        )));
    }

    // Description: required non-empty string.
    let description = required_non_empty_string(data.get("description"), "description", path)?;

    // Prompt: the trimmed Markdown body (.md only — the legacy .yaml prompt
    // fields are out of scope).
    let body = parsed.body.trim();
    if body.is_empty() {
        return Err(fail(format!("Missing prompt body in {path}")));
    }

    // Optional fields. Aliases resolve like the TS `primary ?? alias`:
    // an absent or null primary falls through to the alias.
    let when_to_use = non_empty_string(data.get("whenToUse"));
    let default_model = non_empty_string(field_or_alias(&data, "defaultModel", "default_model"));
    let tools = parse_string_list(data.get("tools"));
    let disallowed_tools =
        parse_string_list(field_or_alias(&data, "disallowedTools", "disallowed_tools"));
    let subagents = parse_subagents(data.get("subagents"));
    let capabilities_required = parse_string_list(field_or_alias(
        &data,
        "capabilitiesRequired",
        "capabilities_required",
    ));
    let enabled = match data.get("enabled") {
        Some(FrontmatterValue::Bool(value)) => Some(*value),
        _ => None,
    };

    Ok(AgentFileDef {
        name,
        description,
        when_to_use,
        default_model,
        tools,
        disallowed_tools,
        subagents,
        capabilities_required,
        enabled,
        prompt: body.to_string(),
        file_path: Some(path.to_string()),
        source,
    })
}

/// Serializes an [`AgentFileDef`] back into `.md` form (frontmatter + prompt
/// as the Markdown body). Mirrors the TS `serializeAgentFile`: fields are
/// emitted in the TS order, optional fields only when present, `enabled` only
/// when `Some(false)`, and the prompt never appears as a frontmatter key.
/// Round-trip (parse → serialize → parse) preserves all fields.
pub fn serialize_agent_file(def: &AgentFileDef) -> String {
    let mut data = FrontmatterMap::new();
    data.insert_str("name", &def.name);
    data.insert_str("description", &def.description);
    if let Some(when_to_use) = &def.when_to_use {
        data.insert_str("whenToUse", when_to_use);
    }
    if let Some(default_model) = &def.default_model {
        data.insert_str("defaultModel", default_model);
    }
    if let Some(tools) = &def.tools {
        data.insert_list("tools", tools.clone());
    }
    if let Some(disallowed_tools) = &def.disallowed_tools {
        data.insert_list("disallowedTools", disallowed_tools.clone());
    }
    if let Some(subagents) = &def.subagents {
        data.insert_list("subagents", subagents.clone());
    }
    if let Some(capabilities_required) = &def.capabilities_required {
        data.insert_list("capabilitiesRequired", capabilities_required.clone());
    }
    if def.enabled == Some(false) {
        data.insert("enabled", FrontmatterValue::Bool(false));
    }
    serialize_frontmatter(&data, &def.prompt)
}

// ---------------------------------------------------------------------------
// Frontmatter subset parser (no YAML crate — adjudicated R2-1)
// ---------------------------------------------------------------------------

/// Parses the trimmed text between the fences into a flat mapping.
///
/// Enforces the supported subset (see the module docs); anything else is a
/// hard [`FrontmatterError`].
fn parse_frontmatter_map(yaml_text: &str) -> Result<FrontmatterMap, FrontmatterError> {
    let mut map = FrontmatterMap::default();
    // Key whose value is being collected as a block list (`key:` + `- item`).
    let mut pending_list: Option<String> = None;

    for raw in yaml_text.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // Block list item (`- item`). js-yaml allows the dash at any
        // indentation under the key (including column 0); a dash without a
        // list key in progress would be a top-level sequence / mixed
        // document, which is out of subset.
        if trimmed == "-" || trimmed.starts_with("- ") {
            let Some(key) = pending_list.clone() else {
                return Err(FrontmatterError::new(format!(
                    "unexpected list item `{trimmed}` (no list key in progress; top-level sequences are not supported)"
                )));
            };
            let item = trimmed[1..].trim_start();
            if item.is_empty() {
                continue; // null item — dropped, like the TS filters do
            }
            let is_quoted = item.starts_with('\'') || item.starts_with('"');
            if !is_quoted && split_key_line(item).is_some() {
                return Err(FrontmatterError::new(
                    "mapping entries inside lists are not supported (list items must be string scalars)",
                ));
            }
            match parse_scalar(item)? {
                FrontmatterValue::Str(value) if !value.is_empty() => {
                    map.push_list_item(&key, value);
                }
                FrontmatterValue::Str(_) | FrontmatterValue::Null => {} // empty item — dropped
                _ => {
                    return Err(FrontmatterError::new(
                        "list items must be string scalars (numbers and booleans are not supported)",
                    ));
                }
            }
            continue;
        }

        let indent = line.len() - line.trim_start().len();
        if indent > 0 {
            return Err(FrontmatterError::new(format!(
                "indented mapping entries are not supported (agent frontmatter must be a flat mapping): `{line}`"
            )));
        }

        if trimmed == "..." {
            return Err(FrontmatterError::new(
                "multi-document streams are not supported",
            ));
        }

        let Some((key, rest)) = split_key_line(line) else {
            return Err(FrontmatterError::new(format!(
                "expected a `key: value` mapping entry, found `{trimmed}`"
            )));
        };
        let key = key.trim();
        if key.is_empty()
            || !key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(FrontmatterError::new(format!(
                "unsupported mapping key `{key}` (bare [A-Za-z0-9_-] keys only)"
            )));
        }
        if map.contains(key) {
            return Err(FrontmatterError::new(format!(
                "duplicated mapping key `{key}`"
            )));
        }

        let rest = rest.trim();
        if rest.is_empty() || rest.starts_with('#') {
            // `key:` — a null until a block list upgrades it. A value that
            // is only a trailing comment (`tools: # note`) binds the same
            // way (R3-7 alignment): js-yaml sees a null value there and
            // binds the following block list to the key, so a comment-only
            // line must arm `pending_list` exactly like an empty one.
            map.insert(key, FrontmatterValue::Null);
            pending_list = Some(key.to_string());
        } else {
            let value = parse_scalar(rest)?;
            map.insert(key, value);
            pending_list = None;
        }
    }
    Ok(map)
}

/// Splits a `key: value` line at the first colon that is at end-of-line or
/// followed by a space/tab. Returns `(key, rest)`.
pub(super) fn split_key_line(line: &str) -> Option<(&str, &str)> {
    let bytes = line.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b':' {
            let at_end = index + 1 == bytes.len();
            let followed_by_space =
                !at_end && (bytes[index + 1] == b' ' || bytes[index + 1] == b'\t');
            if at_end || followed_by_space {
                let rest = if at_end { "" } else { &line[index + 1..] };
                return Some((&line[..index], rest));
            }
        }
    }
    None
}

/// Parses a non-empty scalar value (the text after `key: ` or `- `).
fn parse_scalar(text: &str) -> Result<FrontmatterValue, FrontmatterError> {
    debug_assert!(!text.is_empty());
    match text.as_bytes()[0] {
        b'\'' => return parse_single_quoted(text),
        b'"' => return parse_double_quoted(text),
        b'|' | b'>' => {
            return Err(FrontmatterError::new(
                "block scalars (`|`, `>`) are not supported in agent frontmatter",
            ));
        }
        b'{' => {
            return Err(FrontmatterError::new(
                "flow mappings (`{ ... }`) are not supported in agent frontmatter",
            ));
        }
        b'[' => {
            if text == "[]" {
                return Ok(FrontmatterValue::List(Vec::new()));
            }
            return Err(FrontmatterError::new(
                "flow sequences (`[ ... ]`) are not supported in agent frontmatter",
            ));
        }
        b'&' | b'*' => {
            return Err(FrontmatterError::new(
                "anchors and aliases are not supported in agent frontmatter",
            ));
        }
        b'!' => {
            return Err(FrontmatterError::new(
                "tags are not supported in agent frontmatter",
            ));
        }
        b'-' if text.starts_with("- ") => {
            return Err(FrontmatterError::new(
                "inline block sequences are not supported in agent frontmatter",
            ));
        }
        _ => {}
    }
    if text.starts_with('#') {
        // The value is only a comment → YAML null.
        return Ok(FrontmatterValue::Null);
    }
    let cleaned = strip_inline_comment(text);
    if cleaned.contains(": ") || cleaned.ends_with(':') {
        return Err(FrontmatterError::new(format!(
            "unquoted scalar `{cleaned}` contains a `: ` and must be quoted"
        )));
    }
    Ok(match cleaned {
        "true" | "True" | "TRUE" => FrontmatterValue::Bool(true),
        "false" | "False" | "FALSE" => FrontmatterValue::Bool(false),
        "null" | "Null" | "NULL" | "~" => FrontmatterValue::Null,
        value if looks_like_number(value) => FrontmatterValue::Number(value.to_string()),
        value => FrontmatterValue::Str(value.to_string()),
    })
}

fn parse_single_quoted(text: &str) -> Result<FrontmatterValue, FrontmatterError> {
    let mut out = String::new();
    let mut chars = text[1..].chars().peekable();
    loop {
        match chars.next() {
            None => {
                return Err(FrontmatterError::new("unterminated single-quoted string"));
            }
            Some('\'') => {
                if chars.peek() == Some(&'\'') {
                    chars.next();
                    out.push('\'');
                } else {
                    break;
                }
            }
            Some(c) => out.push(c),
        }
    }
    let rest: String = chars.collect();
    check_trailing_after_quote(&rest)?;
    Ok(FrontmatterValue::Str(out))
}

fn parse_double_quoted(text: &str) -> Result<FrontmatterValue, FrontmatterError> {
    let mut out = String::new();
    let mut chars = text[1..].chars().peekable();
    loop {
        match chars.next() {
            None => {
                return Err(FrontmatterError::new("unterminated double-quoted string"));
            }
            Some('"') => break,
            Some('\\') => match chars.next() {
                Some('\\') => out.push('\\'),
                Some('"') => out.push('"'),
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some('r') => out.push('\r'),
                Some('0') => out.push('\0'),
                Some('u') => {
                    let mut code = String::new();
                    for _ in 0..4 {
                        match chars.next() {
                            Some(hex) if hex.is_ascii_hexdigit() => code.push(hex),
                            _ => {
                                return Err(FrontmatterError::new(
                                    "invalid \\u escape (expected 4 hex digits)",
                                ));
                            }
                        }
                    }
                    let value = u32::from_str_radix(&code, 16).unwrap_or(0);
                    match char::from_u32(value) {
                        Some(c) => out.push(c),
                        None => {
                            return Err(FrontmatterError::new(
                                "invalid \\u escape (not a Unicode scalar value)",
                            ));
                        }
                    }
                }
                _ => {
                    return Err(FrontmatterError::new(
                        "unsupported escape sequence in a double-quoted string",
                    ));
                }
            },
            Some(c) => out.push(c),
        }
    }
    let rest: String = chars.collect();
    check_trailing_after_quote(&rest)?;
    Ok(FrontmatterValue::Str(out))
}

/// After a closing quote only trailing whitespace or a `# comment` (preceded
/// by whitespace) may remain — anything else is out of subset.
fn check_trailing_after_quote(rest: &str) -> Result<(), FrontmatterError> {
    if rest.trim().is_empty() {
        return Ok(());
    }
    if rest.starts_with([' ', '\t']) && rest.trim_start().starts_with('#') {
        return Ok(());
    }
    Err(FrontmatterError::new(
        "unexpected content after a closing quote",
    ))
}

/// Strips a trailing `# comment` from an unquoted scalar (a `#` at the start
/// or preceded by whitespace begins a comment, as in YAML).
fn strip_inline_comment(text: &str) -> &str {
    let bytes = text.as_bytes();
    for index in 1..bytes.len() {
        if bytes[index] == b'#' && (bytes[index - 1] == b' ' || bytes[index - 1] == b'\t') {
            return text[..index].trim_end();
        }
    }
    text
}

/// Recognizes the number forms js-yaml resolves to numbers (so they behave as
/// non-strings, like the TS `typeof` checks). Decimal (with optional sign,
/// dot, exponent), `0x` hex; js-yaml keeps `1_000` a string, so underscores
/// are *not* accepted.
fn looks_like_number(text: &str) -> bool {
    let body = text.strip_prefix(['-', '+']).unwrap_or(text);
    if body.is_empty() {
        return false;
    }
    if let Some(hex) = body.strip_prefix("0x") {
        return !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit());
    }
    let (mantissa, exponent) = match body.split_once(['e', 'E']) {
        Some((m, e)) => (m, Some(e)),
        None => (body, None),
    };
    if mantissa.is_empty() {
        return false;
    }
    let mut seen_digit = false;
    let mut seen_dot = false;
    for byte in mantissa.bytes() {
        match byte {
            b'0'..=b'9' => seen_digit = true,
            b'.' => {
                if seen_dot {
                    return false;
                }
                seen_dot = true;
            }
            _ => return false,
        }
    }
    if let Some(exponent) = exponent {
        let digits = exponent.strip_prefix(['-', '+']).unwrap_or(exponent);
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
    }
    seen_digit
}

// ---------------------------------------------------------------------------
// TS field helpers (parse.ts)
// ---------------------------------------------------------------------------

/// TS `deriveNameFromPath`: the last `/`/`\` segment minus its last
/// extension, `None` when that leaves nothing.
fn derive_name_from_path(path: &str) -> Option<String> {
    let base = path.rsplit(['/', '\\']).next().unwrap_or("");
    let name = match base.rfind('.') {
        Some(index) => &base[..index],
        None => base,
    };
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// TS `AGENT_NAME_PATTERN` (`^[a-z0-9]+(?:-[a-z0-9]+)*$`), hand-rolled to
/// avoid a static regex: non-empty ASCII lowercase-alnum segments separated by
/// single dashes. The route-level create guard reuses the same semantics.
pub fn is_kebab_case(name: &str) -> bool {
    let mut segment_len = 0usize;
    for byte in name.bytes() {
        match byte {
            b'a'..=b'z' | b'0'..=b'9' => segment_len += 1,
            b'-' => {
                if segment_len == 0 {
                    return false; // leading or double dash
                }
                segment_len = 0;
            }
            _ => return false, // uppercase, underscore, or non-ASCII
        }
    }
    segment_len > 0 // rejects the trailing-dash case too
}

/// TS `nonEmptyString`: a string whose trim is non-empty, returned trimmed;
/// anything else (null, bool, number, list, absent) is missing.
fn non_empty_string(value: Option<&FrontmatterValue>) -> Option<String> {
    match value {
        Some(FrontmatterValue::Str(value)) if !value.trim().is_empty() => {
            Some(value.trim().to_string())
        }
        _ => None,
    }
}

/// TS `requiredNonEmptyString`.
fn required_non_empty_string(
    value: Option<&FrontmatterValue>,
    field: &str,
    path: &str,
) -> Result<String, AgentFileParseError> {
    if let Some(
        FrontmatterValue::Bool(_) | FrontmatterValue::Number(_) | FrontmatterValue::List(_),
    ) = value
    {
        return Err(AgentFileParseError {
            path: path.to_string(),
            reason: format!("Frontmatter field \"{field}\" in {path} must be a non-empty string"),
        });
    }
    non_empty_string(value).ok_or_else(|| AgentFileParseError {
        path: path.to_string(),
        reason: format!("Missing required frontmatter field \"{field}\" in {path}"),
    })
}

/// TS `parseStringList`: absent/null → missing; a string is comma-split,
/// trimmed and empty-dropped; a list keeps its non-blank items; other types
/// (bool, number) are missing.
fn parse_string_list(value: Option<&FrontmatterValue>) -> Option<Vec<String>> {
    match value {
        None | Some(FrontmatterValue::Null) => None,
        Some(FrontmatterValue::Str(value)) => Some(
            value
                .split(',')
                .map(|item| item.trim())
                .filter(|item| !item.is_empty())
                .map(str::to_string)
                .collect(),
        ),
        Some(FrontmatterValue::List(items)) => Some(
            items
                .iter()
                .filter(|item| !item.trim().is_empty())
                .cloned()
                .collect(),
        ),
        Some(FrontmatterValue::Bool(_)) | Some(FrontmatterValue::Number(_)) => None,
    }
}

/// TS `parseSubagents` minus the legacy `.yaml` Record fallback (out of
/// scope — a nested mapping is a hard parse error for `.md`): only block
/// lists produce a value; a comma-separated string does *not* split here.
fn parse_subagents(value: Option<&FrontmatterValue>) -> Option<Vec<String>> {
    match value {
        Some(FrontmatterValue::List(items)) => Some(
            items
                .iter()
                .filter(|item| !item.trim().is_empty())
                .cloned()
                .collect(),
        ),
        _ => None,
    }
}

/// TS `record[primary] ?? record[alias]`: an absent or null primary falls
/// through to the alias.
fn field_or_alias<'a>(
    map: &'a FrontmatterMap,
    primary: &str,
    alias: &str,
) -> Option<&'a FrontmatterValue> {
    match map.get(primary) {
        None | Some(FrontmatterValue::Null) => map.get(alias),
        Some(value) => Some(value),
    }
}

// ---------------------------------------------------------------------------
// Serialization dump (TS serializeFrontmatter / js-yaml dump subset)
// ---------------------------------------------------------------------------

pub(super) fn dump_entry(key: &str, value: &FrontmatterValue, out: &mut Vec<String>) {
    match value {
        FrontmatterValue::List(items) if items.is_empty() => {
            out.push(format!("{key}: []"));
        }
        FrontmatterValue::List(items) => {
            out.push(format!("{key}:"));
            for item in items {
                out.push(format!("  - {}", dump_string(item)));
            }
        }
        FrontmatterValue::Null => out.push(format!("{key}: null")),
        FrontmatterValue::Bool(value) => out.push(format!("{key}: {value}")),
        FrontmatterValue::Number(value) => out.push(format!("{key}: {value}")),
        FrontmatterValue::Str(value) => out.push(format!("{key}: {}", dump_string(value))),
    }
}

/// Renders a string scalar, quoting exactly as needed for a round-trip
/// through [`parse_frontmatter`] (conservative, not byte-identical to js-yaml:
/// e.g. `-foo` gets quoted where js-yaml would keep it plain).
fn dump_string(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    if value.bytes().any(|b| b.is_ascii_control()) {
        return double_quote_dump(value);
    }
    let needs_quote = value.trim() != value
        || value.starts_with([
            '-', '?', ':', ',', '[', ']', '{', '}', '#', '&', '*', '!', '|', '>', '\'', '"', '%',
            '@', '`',
        ])
        || value.ends_with(':')
        || value.contains(": ")
        || value.contains(" #")
        || matches!(
            value,
            "true" | "True" | "TRUE" | "false" | "False" | "FALSE" | "null" | "Null" | "NULL" | "~"
        )
        || looks_like_number(value)
        || value == "---"
        || value == "...";
    if needs_quote {
        format!("'{}'", value.replace('\'', "''"))
    } else {
        value.to_string()
    }
}

fn double_quote_dump(value: &str) -> String {
    let mut out = String::from("\"");
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// ---------------------------------------------------------------------------
// Tests — ports of packages/agent-profile/test/{frontmatter,parse,serialize}.test.ts
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // === TS frontmatter.test.ts ports (4 parse + 3 serialize) ============

    /// TS: parseFrontmatter > "should parse frontmatter and body when given a
    /// valid markdown document"
    #[test]
    fn should_parse_frontmatter_and_body_when_given_a_valid_markdown_document() {
        let text = "---\nname: reviewer\ndescription: Test\n---\nYou are a reviewer.";
        let result = parse_frontmatter(text).expect("frontmatter parses");

        let mut expected = FrontmatterMap::new();
        expected.insert_str("name", "reviewer");
        expected.insert_str("description", "Test");
        assert_eq!(result.data, Some(expected));
        assert_eq!(result.body, "You are a reviewer.");
    }

    /// TS: parseFrontmatter > "should return null data and full text as body
    /// when document has no frontmatter fence"
    #[test]
    fn should_return_none_data_and_full_text_as_body_when_document_has_no_frontmatter_fence() {
        let text = "Just some text.";
        let result = parse_frontmatter(text).expect("no fence is not an error");

        assert_eq!(result.data, None);
        assert_eq!(result.body, "Just some text.");
    }

    /// TS: parseFrontmatter > "should throw FrontmatterError when closing
    /// fence is missing"
    #[test]
    fn should_error_when_closing_frontmatter_fence_is_missing() {
        let text = "---\nname: reviewer\n";
        let result = parse_frontmatter(text);

        assert_eq!(
            result.err(),
            Some(FrontmatterError {
                message: "Missing closing frontmatter fence".to_string()
            })
        );
    }

    /// TS: parseFrontmatter > "should return empty object data when
    /// frontmatter is empty"
    #[test]
    fn should_return_empty_map_data_when_frontmatter_is_empty() {
        let text = "---\n---\nBody here.";
        let result = parse_frontmatter(text).expect("empty frontmatter parses");

        assert_eq!(result.data, Some(FrontmatterMap::default()));
        assert_eq!(result.body, "Body here.");
    }

    /// TS: serializeFrontmatter > "should produce frontmatter and body when
    /// given data and non-empty body"
    #[test]
    fn should_produce_frontmatter_and_body_when_serializing_non_empty_body() {
        let mut data = FrontmatterMap::new();
        data.insert_str("name", "reviewer");
        let text = serialize_frontmatter(&data, "You are a reviewer.");

        assert!(text.contains(FENCE));
        assert!(text.contains("name: reviewer"));
        assert!(text.contains("You are a reviewer."));
    }

    /// TS: serializeFrontmatter > "should produce closing fence only when
    /// body is empty"
    #[test]
    fn should_produce_closing_fence_only_when_serializing_empty_body() {
        let mut data = FrontmatterMap::new();
        data.insert_str("name", "reviewer");
        let text = serialize_frontmatter(&data, "");

        let trimmed_end = text.trim_end();
        assert!(trimmed_end.starts_with("---\n"));
        assert!(trimmed_end.ends_with("---"));
        assert!(!text.contains("You are"));
    }

    /// TS: serializeFrontmatter > "should round-trip: parse → serialize →
    /// parse preserves data and body"
    #[test]
    fn should_round_trip_when_parsing_then_serializing_then_parsing_frontmatter() {
        let original = "---\nname: reviewer\ndescription: Test\n---\nYou are a reviewer.";
        let parsed = parse_frontmatter(original).expect("original parses");
        let data = parsed.data.clone().expect("frontmatter present");
        let serialized = serialize_frontmatter(&data, &parsed.body);
        let reparsed = parse_frontmatter(&serialized).expect("serialized form parses");

        assert_eq!(reparsed.data, parsed.data);
        assert_eq!(reparsed.body, parsed.body);
    }

    // === TS parse.test.ts ports ===========================================
    //
    // Skipped TS cases (all `.yaml`/`.yml`-only, legacy nested fallback out
    // of scope per the task adjudication; the rust legacy format is `.toml`):
    //   - "should parse .yaml file with prompt field as prompt body"
    //   - "should map legacy systemPromptTemplate field to prompt when
    //     parsing .yaml"
    //   - "should use promptVars.roleAdditional as prompt when neither prompt
    //     nor systemPromptTemplate is set"
    //   - "should fall back to default prompt when nothing is set"
    //   - "should convert legacy subagents Record to string array when
    //     parsing .yaml" (a nested mapping in `.md` is a hard parse error)
    //   - "should parse .yml file same as .yaml"
    // The ".yaml legacy fields ignored" case is ported below, flattened to
    // the `.md` subset (nested `promptVars:` cannot appear in `.md`).

    /// TS: parseAgentFileText > "should parse frontmatter and body when given
    /// a valid .md file"
    #[test]
    fn should_parse_frontmatter_and_body_when_given_a_valid_md_file() {
        let text = "---\nname: code-reviewer\ndescription: Code review specialist\n---\nYou are a code reviewer.";
        let result =
            parse_agent_file_text("/agents/code-reviewer.md", text).expect("valid .md parses");

        assert_eq!(result.name, "code-reviewer");
        assert_eq!(result.description, "Code review specialist");
        assert_eq!(result.prompt, "You are a code reviewer.");
    }

    /// TS: parseAgentFileText > "should derive name from filename when
    /// frontmatter name is missing"
    #[test]
    fn should_derive_name_from_filename_when_frontmatter_name_is_missing() {
        let text = "---\ndescription: Test agent\n---\nYou are a test agent.";
        let result =
            parse_agent_file_text("/agents/my-agent.md", text).expect("name derives from path");

        assert_eq!(result.name, "my-agent");
    }

    /// TS: parseAgentFileText > "should throw AgentFileParseError when name
    /// is not kebab-case" (exact TS message format)
    #[test]
    fn should_error_when_name_is_not_kebab_case() {
        let text = "---\nname: Bad_Name\ndescription: Test\n---\nBody.";
        let error =
            parse_agent_file_text("/agents/bad.md", text).expect_err("non-kebab name is rejected");

        assert_eq!(
            error.reason,
            "Invalid agent name \"Bad_Name\" in /agents/bad.md: expected kebab-case (e.g. \"code-reviewer\")"
        );
        assert_eq!(error.path, "/agents/bad.md");
    }

    /// TS: parseAgentFileText > "should accept a name that is exactly
    /// AGENT_NAME_MAX_LENGTH characters"
    #[test]
    fn should_accept_a_name_that_is_exactly_agent_name_max_length() {
        let name = "a".repeat(64);
        let text = format!("---\nname: {name}\ndescription: Test\n---\nBody.");
        let result = parse_agent_file_text("/agents/long.md", &text).expect("64-char name parses");

        assert_eq!(result.name, name);
    }

    /// TS: parseAgentFileText > "should throw AgentFileParseError when name
    /// exceeds AGENT_NAME_MAX_LENGTH" (exact TS message format)
    #[test]
    fn should_error_when_name_exceeds_agent_name_max_length() {
        let name = "a".repeat(65);
        let text = format!("---\nname: {name}\ndescription: Test\n---\nBody.");
        let error = parse_agent_file_text("/agents/long.md", &text)
            .expect_err("over-long name is rejected");

        assert_eq!(
            error.reason,
            format!(
                "Invalid agent name \"{name}\" in /agents/long.md: \
                 must be at most {AGENT_NAME_MAX_LENGTH} characters"
            )
        );
    }

    /// TS: parseAgentFileText > "should throw AgentFileParseError when
    /// description is missing" (exact TS message format)
    #[test]
    fn should_error_when_description_is_missing() {
        let text = "---\nname: reviewer\n---\nYou are a reviewer.";
        let error = parse_agent_file_text("/agents/reviewer.md", text)
            .expect_err("missing description is rejected");

        assert_eq!(
            error.reason,
            "Missing required frontmatter field \"description\" in /agents/reviewer.md"
        );
    }

    /// TS: parseAgentFileText > "should throw AgentFileParseError when
    /// frontmatter is missing entirely" (exact TS message format)
    #[test]
    fn should_error_when_frontmatter_is_missing_entirely() {
        let text = "Just some text without frontmatter.";
        let error = parse_agent_file_text("/agents/no-fm.md", text)
            .expect_err("missing frontmatter is rejected");

        assert_eq!(error.reason, "Missing frontmatter in /agents/no-fm.md");
    }

    /// TS: parseAgentFileText > "should ignore legacy fields (extends,
    /// override, promptVars, capabilities) in .yaml" — adapted to `.md`:
    /// flat legacy keys are silently ignored (unknown keys never error);
    /// the nested `promptVars:` mapping from the `.yaml` test cannot appear
    /// in `.md` (out-of-subset syntax), and in `.md` the prompt is the body,
    /// so the `prompt` key is ignored too.
    #[test]
    fn should_ignore_legacy_flat_fields_when_parsing_md_file() {
        let text = "---\nname: reviewer\ndescription: Test\nextends: agent\noverride: true\ncapabilities:\n  - code.read\nprompt: You are not the prompt.\n---\nYou are a reviewer.";
        let result =
            parse_agent_file_text("/agents/reviewer.md", text).expect("legacy keys are ignored");

        assert_eq!(
            result,
            AgentFileDef {
                name: "reviewer".to_string(),
                description: "Test".to_string(),
                when_to_use: None,
                default_model: None,
                tools: None,
                disallowed_tools: None,
                subagents: None,
                capabilities_required: None,
                enabled: None,
                prompt: "You are a reviewer.".to_string(),
                file_path: Some("/agents/reviewer.md".to_string()),
                source: None,
            }
        );
    }

    /// TS: parseAgentFileText > "should parse tools and capabilitiesRequired
    /// fields from .md frontmatter"
    #[test]
    fn should_parse_tools_and_capabilities_required_when_given_block_lists() {
        let text = "---\nname: reviewer\ndescription: Test\ntools:\n  - Read\n  - Grep\ncapabilitiesRequired:\n  - code.read\n---\nYou are a reviewer.";
        let result = parse_agent_file_text("/agents/reviewer.md", text).expect("block lists parse");

        assert_eq!(
            result.tools,
            Some(vec!["Read".to_string(), "Grep".to_string()])
        );
        assert_eq!(
            result.capabilities_required,
            Some(vec!["code.read".to_string()])
        );
    }

    /// Given a list key whose value line carries only a trailing comment
    /// (`tools: # note`), When the file parses, Then the following block
    /// list binds to the key exactly like js-yaml does — the comment-only
    /// value behaves as an empty one (the R3-7 cross-engine alignment;
    /// previously the rust subset parser bound `null` and then errored on
    /// the list items).
    #[test]
    fn should_bind_a_block_list_when_the_value_line_is_only_a_trailing_comment() {
        let text = "---\nname: reviewer\ndescription: Test\ntools: # curated set\n  - Read\n  - Grep\n---\nYou are a reviewer.";
        let result = parse_agent_file_text("/agents/reviewer.md", text)
            .expect("a comment-only value line must bind the block list");

        assert_eq!(
            result.tools,
            Some(vec!["Read".to_string(), "Grep".to_string()])
        );
    }

    /// Given a key whose value line is only a trailing comment and NO block
    /// list follows, When the file parses, Then the key binds null and the
    /// optional field stays `None` (js-yaml's null semantics).
    #[test]
    fn should_treat_a_comment_only_value_as_null_when_no_list_follows() {
        let text =
            "---\nname: reviewer\ndescription: Test\ntools: # none yet\n---\nYou are a reviewer.";
        let result =
            parse_agent_file_text("/agents/reviewer.md", text).expect("a null value parses");

        assert_eq!(result.tools, None);
    }

    /// TS: parseAgentFileText > "should parse disallowedTools from .md
    /// frontmatter when present"
    #[test]
    fn should_parse_disallowed_tools_when_present_in_frontmatter() {
        let text = "---\nname: reviewer\ndescription: Test\ndisallowedTools:\n  - Write\n  - Edit\n---\nYou are a reviewer.";
        let result =
            parse_agent_file_text("/agents/reviewer.md", text).expect("disallowedTools parses");

        assert_eq!(
            result.disallowed_tools,
            Some(vec!["Write".to_string(), "Edit".to_string()])
        );
    }

    /// TS: parseAgentFileText > "should parse the disallowed_tools snake_case
    /// alias when disallowedTools is absent"
    #[test]
    fn should_parse_disallowed_tools_alias_when_camel_case_key_is_absent() {
        let text = "---\nname: reviewer\ndescription: Test\ndisallowed_tools:\n  - Write\n---\nYou are a reviewer.";
        let result =
            parse_agent_file_text("/agents/reviewer.md", text).expect("snake_case alias parses");

        assert_eq!(result.disallowed_tools, Some(vec!["Write".to_string()]));
    }

    /// TS: parseAgentFileText > "should treat a missing disallowedTools key
    /// as undefined"
    #[test]
    fn should_treat_missing_disallowed_tools_key_as_none() {
        let text = "---\nname: reviewer\ndescription: Test\n---\nYou are a reviewer.";
        let result = parse_agent_file_text("/agents/reviewer.md", text).expect("parses");

        assert_eq!(result.disallowed_tools, None);
    }

    /// TS: parseAgentFileText > "should fill filePath and source from options
    /// when provided"
    #[test]
    fn should_fill_file_path_and_source_when_source_is_provided() {
        let text = "---\nname: reviewer\ndescription: Test\n---\nYou are a reviewer.";
        let result =
            parse_agent_file_text_with_source("/agents/reviewer.md", text, AgentFileSource::User)
                .expect("parses with source");

        assert_eq!(result.file_path, Some("/agents/reviewer.md".to_string()));
        assert_eq!(result.source, Some(AgentFileSource::User));
    }

    /// TS: parseAgentFileText > "should parse enabled: false from the
    /// frontmatter"
    #[test]
    fn should_parse_enabled_false_when_present_in_frontmatter() {
        let def = parse_agent_file_text(
            "reviewer.md",
            "---\nname: reviewer\ndescription: Reviews code\nenabled: false\n---\nYou review.",
        )
        .expect("enabled: false parses");

        assert_eq!(def.enabled, Some(false));
    }

    /// TS: parseAgentFileText > "should treat a missing enabled key as
    /// enabled"
    #[test]
    fn should_treat_missing_enabled_key_as_none() {
        let def = parse_agent_file_text(
            "reviewer.md",
            "---\nname: reviewer\ndescription: Reviews code\n---\nYou review.",
        )
        .expect("parses");

        assert_eq!(def.enabled, None);
    }

    /// TS: parseAgentFileText > "should ignore a non-boolean enabled value"
    #[test]
    fn should_ignore_non_boolean_enabled_value() {
        let def = parse_agent_file_text(
            "strict.md",
            "---\nname: strict\ndescription: d\nenabled: \"false\"\n---\np",
        )
        .expect("string enabled parses (and is ignored)");

        assert_eq!(def.enabled, None);
    }

    // === TS serialize.test.ts ports ========================================

    /// TS: serializeAgentFile > "should produce .md frontmatter format when
    /// given a complete AgentFileDef"
    #[test]
    fn should_produce_md_frontmatter_format_when_given_a_complete_agent_file_def() {
        let def = AgentFileDef {
            name: "code-reviewer".to_string(),
            description: "Code review specialist".to_string(),
            prompt: "You are a code reviewer.".to_string(),
            ..AgentFileDef::default()
        };
        let text = serialize_agent_file(&def);

        assert!(text.starts_with("---\n"));
        assert!(text.contains("name: code-reviewer"));
        assert!(text.contains("description: Code review specialist"));
        assert!(text.contains("You are a code reviewer."));
    }

    /// TS: serializeAgentFile > "should omit optional fields when they are
    /// undefined"
    #[test]
    fn should_omit_optional_fields_when_they_are_none() {
        let def = AgentFileDef {
            name: "simple".to_string(),
            description: "Simple agent".to_string(),
            prompt: "You are simple.".to_string(),
            ..AgentFileDef::default()
        };
        let text = serialize_agent_file(&def);

        assert!(!text.contains("whenToUse"));
        assert!(!text.contains("defaultModel"));
        assert!(!text.contains("tools"));
        assert!(!text.contains("subagents"));
        assert!(!text.contains("capabilitiesRequired"));
    }

    /// TS: serializeAgentFile > "should serialize all optional fields when
    /// present"
    #[test]
    fn should_serialize_all_optional_fields_when_present() {
        let def = AgentFileDef {
            name: "full".to_string(),
            description: "Full agent".to_string(),
            when_to_use: Some("Use when needed".to_string()),
            default_model: Some("claude-sonnet".to_string()),
            tools: Some(vec!["Read".to_string(), "Grep".to_string()]),
            subagents: Some(vec!["coder".to_string()]),
            capabilities_required: Some(vec!["code.read".to_string()]),
            prompt: "You are full.".to_string(),
            ..AgentFileDef::default()
        };
        let text = serialize_agent_file(&def);

        assert!(text.contains("whenToUse: Use when needed"));
        assert!(text.contains("defaultModel: claude-sonnet"));
        assert!(text.contains("Read"));
        assert!(text.contains("Grep"));
        assert!(text.contains("coder"));
        assert!(text.contains("code.read"));
    }

    /// TS: serializeAgentFile > "should round-trip: serialize → parse
    /// preserves all fields"
    #[test]
    fn should_round_trip_all_fields_when_serializing_then_parsing() {
        let original = AgentFileDef {
            name: "round-trip".to_string(),
            description: "Round trip test".to_string(),
            when_to_use: Some("Use for testing".to_string()),
            default_model: Some("test-model".to_string()),
            tools: Some(vec!["Read".to_string(), "Write".to_string()]),
            subagents: Some(vec!["coder".to_string(), "explore".to_string()]),
            capabilities_required: Some(vec!["code.read".to_string(), "code.write".to_string()]),
            enabled: Some(false),
            prompt: "You are a round-trip test agent.".to_string(),
            ..AgentFileDef::default()
        };
        let text = serialize_agent_file(&original);
        let reparsed =
            parse_agent_file_text("/agents/round-trip.md", &text).expect("serialized form parses");

        assert_eq!(reparsed.name, original.name);
        assert_eq!(reparsed.description, original.description);
        assert_eq!(reparsed.when_to_use, original.when_to_use);
        assert_eq!(reparsed.default_model, original.default_model);
        assert_eq!(reparsed.tools, original.tools);
        assert_eq!(reparsed.subagents, original.subagents);
        assert_eq!(
            reparsed.capabilities_required,
            original.capabilities_required
        );
        assert_eq!(reparsed.enabled, Some(false));
        assert_eq!(reparsed.prompt, original.prompt);
    }

    /// TS: serializeAgentFile > "should round-trip disallowedTools through
    /// serialize → parse"
    #[test]
    fn should_round_trip_disallowed_tools_when_serializing_then_parsing() {
        let original = AgentFileDef {
            name: "tool-guard".to_string(),
            description: "Guards against dangerous tools".to_string(),
            tools: Some(vec!["Read".to_string(), "Grep".to_string()]),
            disallowed_tools: Some(vec!["Write".to_string(), "Edit".to_string()]),
            prompt: "You guard against dangerous tools.".to_string(),
            ..AgentFileDef::default()
        };
        let text = serialize_agent_file(&original);
        let reparsed = parse_agent_file_text("/agents/tool-guard.md", &text).expect("parses back");

        assert_eq!(
            reparsed.disallowed_tools,
            Some(vec!["Write".to_string(), "Edit".to_string()])
        );
    }

    /// TS: serializeAgentFile > "should serialize prompt as body, not as
    /// frontmatter field"
    #[test]
    fn should_serialize_prompt_as_body_when_given_agent_file_def() {
        let def = AgentFileDef {
            name: "test".to_string(),
            description: "Test".to_string(),
            prompt: "You are a test agent.".to_string(),
            ..AgentFileDef::default()
        };
        let text = serialize_agent_file(&def);

        // The prompt should appear after the closing fence, not inside it.
        let close = text[3..].find(FENCE).expect("closing fence") + 3;
        let frontmatter = &text[3..close];
        assert!(!frontmatter.contains("prompt:"));
        let body = text[close + 3..].trim_start();
        assert_eq!(body, "You are a test agent.");
    }

    /// TS: serializeAgentFile > "should persist enabled only when false"
    #[test]
    fn should_persist_enabled_only_when_false() {
        let base = AgentFileDef {
            description: "d".to_string(),
            prompt: "p".to_string(),
            ..AgentFileDef::default()
        };

        let disabled = serialize_agent_file(&AgentFileDef {
            enabled: Some(false),
            ..base.clone()
        });
        assert!(disabled.contains("enabled: false"));

        let enabled = serialize_agent_file(&AgentFileDef {
            enabled: Some(true),
            ..base.clone()
        });
        assert!(!enabled.contains("enabled"));

        let omitted = serialize_agent_file(&base);
        assert!(!omitted.contains("enabled"));
    }

    // === Out-of-subset hard errors + TS-parity semantics ==================
    // (adjudicated: anything outside the supported subset is a hard parse
    // error carrying the path and a reason — the future discovery layer
    // skip-collects these; no .yaml fallback guessing.)

    #[test]
    fn should_error_when_frontmatter_uses_nested_mapping_syntax() {
        let text =
            "---\nname: nested\ndescription: d\npromptVars:\n  roleAdditional: extra\n---\nBody.";
        let error = parse_agent_file_text("/agents/nested.md", text)
            .expect_err("nested mappings are out of subset");

        assert_eq!(error.path, "/agents/nested.md");
        assert!(
            error
                .reason
                .contains("Invalid frontmatter in /agents/nested.md")
        );
        assert!(error.reason.contains("flat mapping"));
    }

    #[test]
    fn should_error_when_frontmatter_uses_block_scalar_syntax() {
        let text = "---\nname: block-scalar\ndescription: |\n  text\n---\nBody.";
        let error = parse_agent_file_text("/agents/block.md", text)
            .expect_err("block scalars are out of subset");

        assert!(error.reason.contains("block scalars"));
    }

    #[test]
    fn should_error_when_frontmatter_uses_flow_sequence_syntax() {
        let text = "---\nname: flow\ndescription: d\ntools: [Read, Grep]\n---\nBody.";
        let error = parse_agent_file_text("/agents/flow.md", text)
            .expect_err("non-empty flow sequences are out of subset");

        assert!(error.reason.contains("flow sequences"));
    }

    #[test]
    fn should_error_when_frontmatter_uses_anchor_or_alias_syntax() {
        let text = "---\nname: anchor\ndescription: &base text\n---\nBody.";
        let error = parse_agent_file_text("/agents/anchor.md", text)
            .expect_err("anchors are out of subset");

        assert!(error.reason.contains("anchors"));
    }

    #[test]
    fn should_error_when_frontmatter_uses_multi_document_marker() {
        let text = "---\nname: multi\ndescription: d\n...\n---\nBody.";
        let error = parse_agent_file_text("/agents/multi.md", text)
            .expect_err("multi-document streams are out of subset");

        assert!(error.reason.contains("multi-document"));
    }

    #[test]
    fn should_error_when_duplicate_mapping_keys_are_present() {
        let text = "---\nname: dup\ndescription: d\nname: dup\n---\nBody.";
        let error = parse_agent_file_text("/agents/dup.md", text)
            .expect_err("duplicate keys are rejected (js-yaml parity)");

        assert!(error.reason.contains("duplicated mapping key"));
    }

    /// TS `parseStringList` string branch: any string value where a list is
    /// expected is comma-split, trimmed, and empties dropped. Note the TS
    /// asymmetry: `subagents` does *not* comma-split (parseSubagents).
    #[test]
    fn should_parse_comma_separated_string_list_when_field_value_is_a_string() {
        let text = "---\nname: lists\ndescription: d\ntools: Read, Grep\ndisallowedTools: Write,,Edit ,\nsubagents: coder, explore\n---\nBody.";
        let def = parse_agent_file_text("/agents/lists.md", text).expect("parses");

        assert_eq!(
            def.tools,
            Some(vec!["Read".to_string(), "Grep".to_string()])
        );
        assert_eq!(
            def.disallowed_tools,
            Some(vec!["Write".to_string(), "Edit".to_string()])
        );
        assert_eq!(def.subagents, None);
    }

    /// CRLF tolerance (acceptance criterion): fences, keys and body all
    /// tolerate Windows line endings.
    #[test]
    fn should_parse_crlf_line_endings_when_given_windows_style_text() {
        let text = "---\r\nname: code-reviewer\r\ndescription: Code review specialist\r\n---\r\nYou are a code reviewer.";
        let result =
            parse_agent_file_text("/agents/code-reviewer.md", text).expect("CRLF text parses");

        assert_eq!(result.name, "code-reviewer");
        assert_eq!(result.description, "Code review specialist");
        assert_eq!(result.prompt, "You are a code reviewer.");
    }

    /// TS `body.trim()`: the prompt is the trimmed Markdown body.
    #[test]
    fn should_trim_prompt_body_when_parsing_md_file() {
        let text = "---\nname: trim-body\ndescription: d\n---\n\n\n  You are a trim test.  \n\n";
        let def = parse_agent_file_text("/agents/trim-body.md", text).expect("parses");

        assert_eq!(def.prompt, "You are a trim test.");
    }

    /// TS `nonEmptyString`: whitespace-only strings are treated as missing —
    /// for optional fields (None) and required fields (the "missing" error).
    #[test]
    fn should_treat_whitespace_only_string_field_as_missing() {
        let optional = "---\nname: ws\ndescription: d\nwhenToUse: \"   \"\n---\nBody.";
        let def = parse_agent_file_text("/agents/ws.md", optional).expect("parses");
        assert_eq!(def.when_to_use, None);

        let required = "---\nname: ws2\ndescription: \"   \"\n---\nBody.";
        let error = parse_agent_file_text("/agents/ws2.md", required)
            .expect_err("whitespace-only description is missing");
        assert_eq!(
            error.reason,
            "Missing required frontmatter field \"description\" in /agents/ws2.md"
        );
    }

    /// Quoted scalar forms (`'…'` with `''` escaping, `"…"` with escapes).
    #[test]
    fn should_parse_quoted_scalar_values_when_strings_are_quoted() {
        let text = "---\nname: \"code-reviewer\"\ndescription: 'A reviewer, primarily'\nwhenToUse: \"It's quoted\"\n---\nBody.";
        let def = parse_agent_file_text("/agents/quoted.md", text).expect("quoted scalars parse");

        assert_eq!(def.name, "code-reviewer");
        assert_eq!(def.description, "A reviewer, primarily");
        assert_eq!(def.when_to_use, Some("It's quoted".to_string()));
    }

    /// TS `parseAgentFileText` name type check: a non-string `name` errors
    /// with the exact TS message before the filename derive runs.
    #[test]
    fn should_error_when_name_field_is_not_a_string() {
        let text = "---\nname: true\ndescription: d\n---\nBody.";
        let error = parse_agent_file_text("/agents/bad-name.md", text)
            .expect_err("non-string name is rejected");

        assert_eq!(
            error.reason,
            "Frontmatter field \"name\" in /agents/bad-name.md must be a non-empty string"
        );
    }
}
