//! Model-specific prompts: the builtin rule table, the file identity format,
//! and the one path-sanitization rule for model names.
//!
//! Requirement D-M13 / architecture C9. The split of responsibilities is the
//! zero-IO contract (rust/AGENTS.md 契约 6): **this module is pure** — it
//! decides *what* a model's prompt says and *which file name* carries it, but
//! never touches the filesystem. The composition root (the embedded server)
//! owns reading and writing `model_prompts/`, exactly like the existing
//! `SYSTEM.md` channel.
//!
//! Two invariants drive the whole design:
//!
//! 1. **Identity is self-described.** A prompt file's owner is the `model:`
//!    field in its frontmatter, never its file name. Sanitization is lossy
//!    (`a/b` and `a-b` collide), so a name-derived identity would silently
//!    hand one model another model's prompt.
//! 2. **Lossy names carry a fingerprint.** [`prompt_file_stem`] appends a
//!    fingerprint whenever the mapping was lossy in any way — including
//!    lowercasing, which on macOS/Windows would otherwise fold `Mimo` and
//!    `mimo` onto one file.

/// The maximum size of a prompt file's body, in bytes.
///
/// A body over this limit is dropped whole rather than truncated: half a
/// directive is more dangerous than none, because the model will follow the
/// fragment.
pub const MAX_PROMPT_BODY_BYTES: usize = 16 * 1024;

/// One builtin rule: a match pattern over the raw model name, and the prompt
/// body materialized for a matching model.
pub struct ModelPromptRule {
    /// Pattern over the raw model name (M2). `*` matches any run of
    /// characters; matching is case-insensitive because the comparison runs
    /// on the lowercased form.
    pub pattern: &'static str,
    /// The prompt body written into a newly materialized file.
    pub body: &'static str,
}

/// The builtin rule table (architecture C9.1).
///
/// Ordered; the first matching rule wins. Deliberately small: a rule is a
/// claim about how a model behaves, and a wrong claim costs tokens and can
/// distort behavior. Entries are added only with evidence, never to "cover
/// more models".
pub const BUILTIN_MODEL_PROMPT_RULES: &[ModelPromptRule] = &[ModelPromptRule {
    pattern: "mimo-v2.6-*",
    body: "Keep your internal reasoning strictly within what the task requires. Do not \
produce long, performative <thinking> monologues. Go straight to acting: emit \
the task's next concrete step or the tool call it needs.\n\
\n\
Before a tool call, keep the reasoning short and focused on the immediate next \
action. Do not speculate beyond the available evidence and do not re-litigate \
your own conclusions.\n\
\n\
Plan at most three tool actions per round. When you already have enough \
evidence to answer, summarise and answer — do not keep expanding the \
investigation.\n",
}];

/// The builtin prompt body for `raw_model_name`, when a rule matches.
///
/// Matching is case-insensitive and anchored at the start (a pattern without
/// `*` must equal the whole name; a pattern ending in `*` is a prefix match).
pub fn builtin_prompt_for(raw_model_name: &str) -> Option<&'static str> {
    let name = raw_model_name.trim().to_ascii_lowercase();
    if name.is_empty() {
        return None;
    }
    BUILTIN_MODEL_PROMPT_RULES
        .iter()
        .find(|rule| glob_matches(&rule.pattern.to_ascii_lowercase(), &name))
        .map(|rule| rule.body)
}

/// A minimal glob: `*` matches any run (including empty), everything else is
/// literal. Sufficient for model-name prefixes, and small enough to verify by
/// inspection — a regex dependency would be heavier than the rule table it
/// serves.
fn glob_matches(pattern: &str, text: &str) -> bool {
    match pattern.split_once('*') {
        None => pattern == text,
        Some((prefix, rest)) => {
            // A prefix-only pattern (`mimo-*`) matches every name that starts
            // with it, including the bare prefix itself.
            if rest.is_empty() {
                return text.starts_with(prefix);
            }
            // General case (`a*b*c`): try every split point of the remaining
            // pattern against every suffix — the pattern table is tiny, so a
            // simple recursive walk is preferable to a matcher we would have
            // to trust.
            (0..=text.len())
                .filter(|&i| text.is_char_boundary(i))
                .any(|i| text[..i].starts_with(prefix) && glob_matches(rest, &text[i..]))
        }
    }
}

/// Split an already-fragmented name into the stem's path-safe characters.
///
/// Returns the sanitized stem plus whether the mapping was lossy — the flag
/// drives the fingerprint suffix in [`prompt_file_stem`].
fn sanitize_stem(raw: &str) -> (String, bool) {
    let mut lossy = false;

    // Step 1: lowercase (and trim surrounding whitespace). Counted as lossy
    // in both cases: on a case-insensitive filesystem `Mimo` and `mimo` would
    // otherwise share one file (US-17 requires they do not), and `con` vs
    // `con ` would otherwise fold onto the same stem.
    let lowered = raw.trim().to_lowercase();
    if lowered != raw {
        lossy = true;
    }

    // Step 2: keep [a-z0-9._-], fold every other character to `-`.
    let mut pushed = String::with_capacity(lowered.len());
    for ch in lowered.chars() {
        if ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '.' | '_' | '-') {
            pushed.push(ch);
        } else {
            pushed.push('-');
            lossy = true;
        }
    }

    // Step 3: collapse runs of `-`.
    let mut collapsed = String::with_capacity(pushed.len());
    let mut prev_dash = false;
    for ch in pushed.chars() {
        if ch == '-' {
            if prev_dash {
                lossy = true;
                continue;
            }
            prev_dash = true;
        } else {
            prev_dash = false;
        }
        collapsed.push(ch);
    }

    // Step 4: trim leading/trailing separators.
    let trimmed = collapsed.trim_matches(|c| c == '-' || c == '.' || c == '_');
    if trimmed.len() != collapsed.len() {
        lossy = true;
    }

    (trimmed.to_string(), lossy)
}

/// Windows reserved device names: a file with one of these stems cannot be
/// created on Windows regardless of directory.
const WINDOWS_RESERVED: [&str; 22] = [
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// The longest stem this rule produces, fingerprint included.
///
/// 91 + 1 + 8 = 100 characters, leaving room for `model_prompts/` and `.md`
/// well inside every platform's component limit.
const MAX_STEM_LEN: usize = 100;

/// The 8-hex-digit FNV-1a fingerprint of the raw (unprocessed) name.
fn fingerprint(raw: &str) -> String {
    const FNV_OFFSET_BASIS: u32 = 0x811c_9dc5;
    const FNV_PRIME: u32 = 0x0100_0193;
    let mut hash = FNV_OFFSET_BASIS;
    for byte in raw.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{hash:08x}")
}

/// The canonical file stem for a raw model name (architecture C9.3).
///
/// **The only rule in the model domain for "model name → file path".** Any
/// future consumer reuses this function rather than writing another one.
///
/// Steps, in order:
///
/// 1. Lowercase (lossy — see [`sanitize_stem`]).
/// 2. Keep `[a-z0-9._-]`; fold every other character to `-`.
/// 3. Collapse `-` runs.
/// 4. Trim leading/trailing `-`, `.`, `_`.
/// 5. Empty result → `model-<fingerprint>`, and stop (no second fingerprint).
/// 6. A Windows reserved device name gets a `model-` prefix (not counted as
///    lossy: the reserved set is closed and prefixing is collision-free).
/// 7. Truncate to leave room for the fingerprint (91 chars).
/// 8. **If and only if** steps 1–4 or 7 were lossy, append
///    `-<fingerprint>` of the raw name.
///
/// Step 8 is what makes the rule safe: sanitization is not injective, and
/// without the fingerprint two distinct names would silently share one file
/// while the filesystem reported no error at all.
///
/// Self-check invariant (pinned by the tests): two distinct raw names that
/// produce the same stem must both carry fingerprints, and those fingerprints
/// differ.
pub fn prompt_file_stem(raw_model_name: &str) -> String {
    let (stem, lossy) = sanitize_stem(raw_model_name);
    let print = fingerprint(raw_model_name);

    // Step 5: nothing survived — the fingerprint alone identifies the file.
    if stem.is_empty() {
        return format!("model-{print}");
    }

    // Step 6: reserved device name.
    let stem = if WINDOWS_RESERVED.contains(&stem.as_str()) {
        format!("model-{stem}")
    } else {
        stem
    };

    if !lossy {
        // Step 7 (no-op for a clean name): truncate only if it is long, and
        // treat that as lossy so the name stays identifiable.
        if stem.chars().count() <= MAX_STEM_LEN {
            return stem;
        }
        let truncated: String = stem.chars().take(MAX_STEM_LEN - 9).collect();
        return format!("{truncated}-{print}");
    }

    let budget = MAX_STEM_LEN - 9; // room for `-` + 8 hex digits
    let truncated: String = stem.chars().take(budget).collect();
    format!("{truncated}-{print}")
}

/// Render a prompt file: frontmatter naming the owner, then the body.
///
/// The `model` value is always double-quoted so names containing `:` or `#`
/// survive YAML's plain-scalar rules.
pub fn render_prompt_file(raw_model_name: &str, body: &str) -> String {
    let escaped = raw_model_name.replace('\\', "\\\\").replace('"', "\\\"");
    format!("---\nmodel: \"{escaped}\"\n---\n{body}")
}

/// Parse a prompt file into `(raw model name, body)`.
///
/// Returns `None` when there is no frontmatter or no `model` field — such a
/// file has no owner and must not be injected under a guessed name. The body
/// is everything after the closing `---` line, with one leading newline
/// removed (the separator [`render_prompt_file`] writes).
pub fn parse_prompt_file(text: &str) -> Option<(String, String)> {
    let rest = text.strip_prefix("---\n").or_else(|| {
        // Tolerate CRLF frontmatter: users edit these files in arbitrary
        // editors.
        text.strip_prefix("---\r\n")
    })?;
    let mut model: Option<String> = None;
    let mut offset = 0usize;
    for line in rest.split_inclusive('\n') {
        let stripped = line.trim_end_matches(['\r', '\n']);
        offset += line.len();
        if stripped == "---" {
            let body = rest[offset..].strip_prefix('\n').unwrap_or(&rest[offset..]);
            return model.map(|model| (model, body.to_string()));
        }
        if let Some(value) = stripped.strip_prefix("model:") {
            model = Some(unquote_yaml_scalar(value.trim()));
        }
    }
    None
}

/// Read a frontmatter scalar, accepting both `"quoted"` and bare forms.
fn unquote_yaml_scalar(value: &str) -> String {
    let unquoted = value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .map(|v| v.replace("\\\"", "\"").replace("\\\\", "\\"))
        .unwrap_or_else(|| value.to_string());
    unquoted.trim().to_string()
}

/// Whether a body may be injected: non-empty and within the size limit.
///
/// The caller reports the over-limit case to the user — dropping silently is
/// exactly the failure mode this requirement exists to remove.
pub fn prompt_body_injectable(body: &str) -> bool {
    let trimmed = body.trim();
    !trimmed.is_empty() && trimmed.len() <= MAX_PROMPT_BODY_BYTES
}

/// The model-prompt lookup a session carries, seeded at creation from the
/// `model_prompts/` directory (architecture C9.5).
///
/// Two levels, because the file layer and the agent layer key differently:
/// files belong to a **raw model name** (models share one file regardless of
/// which provider reaches them), while an agent's config holds a **model id**
/// — an alias key or a `provider/model` composite. The composition root
/// resolves the second into the first when it seeds the table; the engine
/// then answers with one lookup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelPromptTable {
    /// `raw model name → prompt body`.
    by_raw_name: std::collections::BTreeMap<String, String>,
    /// `model id (alias key or provider/model composite) → raw model name`.
    id_to_raw_name: std::collections::BTreeMap<String, String>,
}

impl ModelPromptTable {
    /// Build the table from the two resolved maps.
    pub fn new(
        by_raw_name: std::collections::BTreeMap<String, String>,
        id_to_raw_name: std::collections::BTreeMap<String, String>,
    ) -> Self {
        Self {
            by_raw_name,
            id_to_raw_name,
        }
    }

    /// The prompt body to append for `model_id`, or `None` when the model has
    /// no injectable prompt.
    pub fn body_for(&self, model_id: &str) -> Option<&str> {
        let raw = self.id_to_raw_name.get(model_id)?;
        self.by_raw_name.get(raw).map(String::as_str)
    }

    /// Whether the table carries anything at all (false short-circuits the
    /// caller into the byte-identical "feature absent" path).
    pub fn is_empty(&self) -> bool {
        self.by_raw_name.is_empty()
    }

    /// How many distinct raw model names carry a body — the diagnostic count.
    pub fn len(&self) -> usize {
        self.by_raw_name.len()
    }
}

/// Append a model prompt body to an assembled system prompt.
///
/// Returns `prompt` unchanged when `body` is `None`, so the "feature absent"
/// path stays byte-identical to the pre-feature prompt (D-M13.4). When a body
/// is present it is appended as a trailing section separated by a blank line —
/// nothing existing is replaced or reordered.
pub fn append_model_prompt(prompt: String, body: Option<&str>) -> String {
    let Some(body) = body else {
        return prompt;
    };
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return prompt;
    }
    if prompt.trim().is_empty() {
        return format!("{trimmed}\n");
    }
    format!("{}\n\n{trimmed}\n", prompt.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_match_the_first_builtin_rule_when_the_raw_name_has_the_family_prefix() {
        // Given the builtin table (mimo-v2.6-*)
        // When two family members resolve
        let pro = builtin_prompt_for("mimo-v2.6-pro");
        let flash = builtin_prompt_for("mimo-v2.6-flash");
        // Then both are covered by the same rule, with the same body
        assert!(pro.is_some());
        assert_eq!(pro, flash);
    }

    #[test]
    fn should_return_no_builtin_prompt_when_the_name_is_outside_every_rule() {
        // Given a model the table makes no claim about
        // Then nothing is materialized — guessing would burn tokens and could
        // distort a model that behaves fine
        assert!(builtin_prompt_for("deepseek-v3").is_none());
        assert!(builtin_prompt_for("mimo-v2.5-pro").is_none());
        assert!(builtin_prompt_for("").is_none());
        // A name that merely contains the prefix is not a family member
        assert!(builtin_prompt_for("my-mimo-v2.6-pro").is_none());
    }

    #[test]
    fn should_match_case_insensitively_when_the_rule_pattern_is_lowercase() {
        // Given a directory that spells the name in mixed case
        // Then the rule still applies
        assert!(builtin_prompt_for("MiMo-V2.6-Pro").is_some());
    }

    #[test]
    fn should_keep_a_clean_name_bare_when_nothing_was_lost() {
        // Given a name that needs no transformation
        // When the stem is derived
        // Then it stays human-readable with no fingerprint
        assert_eq!(prompt_file_stem("mimo-v2.6-pro"), "mimo-v2.6-pro");
        assert_eq!(prompt_file_stem("deepseek-v3.1"), "deepseek-v3.1");
    }

    #[test]
    fn should_fold_path_separators_into_one_dash_when_the_name_contains_a_slash() {
        // Given a namespaced model name (`/` is common and would otherwise
        // create a subdirectory)
        // When the stem is derived
        // Then the separator is folded, and a fingerprint records the loss
        let stem = prompt_file_stem("meta-llama/Llama-3-70B");
        assert!(stem.starts_with("meta-llama-llama-3-70b-"), "{stem}");
        assert_eq!(stem.len(), "meta-llama-llama-3-70b-".len() + 8);
    }

    #[test]
    fn should_fingerprint_a_name_only_differing_by_case_so_the_two_never_share_a_file() {
        // Given two names that differ only in case — on macOS and Windows a
        // case-insensitive filesystem would fold them onto one path
        // When both stems are derived
        let upper = prompt_file_stem("Mimo-Pro");
        let lower = prompt_file_stem("mimo-pro");
        // Then they differ (US-17: neither may overwrite the other)
        assert_ne!(upper, lower);
        assert_eq!(lower, "mimo-pro"); // the clean spelling stays clean
    }

    #[test]
    fn should_distinguish_a_folded_name_from_the_clean_name_it_folds_to() {
        // Given `a/b` (lossy) and `a-b` (clean) — the classic collision
        // When both stems are derived
        let lossy = prompt_file_stem("a/b");
        let clean = prompt_file_stem("a-b");
        // Then they never share a file
        assert_ne!(lossy, clean);
        assert_eq!(clean, "a-b");
        assert!(lossy.starts_with("a-b-"));
    }

    #[test]
    fn should_produce_a_model_prefixed_stem_when_nothing_survives_sanitization() {
        // Given a name made entirely of path-hostile characters
        // When the stem is derived
        // Then it is non-empty and carries exactly one fingerprint
        let stem = prompt_file_stem("///");
        assert!(stem.starts_with("model-"), "{stem}");
        assert_eq!(
            stem.matches('-').count(),
            1,
            "one dash, one fingerprint: {stem}"
        );
        assert_eq!(stem.len(), "model-".len() + 8);
    }

    #[test]
    fn should_prefix_reserved_device_names_so_windows_can_create_the_file() {
        // Given a model literally named like a Windows device
        // When the stem is derived
        // Then it is prefixed so Windows can create it; a clean lowercase
        // spelling keeps no fingerprint (the reserved set is closed, so
        // prefixing cannot collide with another clean name)
        assert_eq!(prompt_file_stem("con"), "model-con");
        // `LPT1` upper-cases into a reserved name but the lowercasing itself
        // was lossy, so it must carry a fingerprint (US-17: `LPT1` and `lpt1`
        // may not share one file).
        let upper = prompt_file_stem("LPT1");
        assert!(upper.starts_with("model-lpt1-"), "{upper}");
        assert_ne!(upper, prompt_file_stem("lpt1"));
        assert_eq!(prompt_file_stem("lpt1"), "model-lpt1");
    }

    #[test]
    fn should_keep_the_stem_within_the_budget_when_the_name_is_very_long() {
        // Given a name far beyond any filesystem component limit
        // When the stem is derived
        // Then it fits the budget, fingerprint included
        let long = "a".repeat(400);
        let stem = prompt_file_stem(&long);
        assert!(
            stem.chars().count() <= MAX_STEM_LEN,
            "{}",
            stem.chars().count()
        );
        assert!(stem.ends_with(&fingerprint(&long)));
    }

    #[test]
    fn should_satisfy_the_self_check_invariant_over_a_collision_prone_corpus() {
        // Given a corpus of names chosen to collide under naive sanitization
        let corpus = [
            "Mimo-Pro",
            "mimo-pro",
            "a/b",
            "a-b",
            "a b",
            "a_b",
            "con",
            "con ",
            "///",
            "...",
            "meta-llama/Llama-3-70B",
            "meta-llama-Llama-3-70B",
            &"a".repeat(400),
        ];
        // When every stem is derived
        // Then any two distinct names sharing a stem both carry a fingerprint,
        // and those fingerprints differ
        for (i, left) in corpus.iter().enumerate() {
            for right in corpus.iter().skip(i + 1) {
                if left == right {
                    continue;
                }
                let (ls, rs) = (prompt_file_stem(left), prompt_file_stem(right));
                if ls == rs {
                    panic!("distinct names {left:?} and {right:?} share the stem {ls:?}");
                }
            }
        }
    }

    #[test]
    fn should_round_trip_a_name_containing_yaml_metacharacters_when_rendering_a_file() {
        // Given a model name with `:` and `#` (YAML metacharacters)
        // When a file is rendered and parsed back
        let rendered = render_prompt_file("vendor:model#1", "Body.\n");
        let (model, body) = parse_prompt_file(&rendered).expect("parses");
        // Then the owner is recovered verbatim and the body is intact
        assert_eq!(model, "vendor:model#1");
        assert_eq!(body, "Body.\n");
    }

    #[test]
    fn should_parse_a_bare_frontmatter_value_when_the_file_was_hand_written() {
        // Given a file the user wrote without quotes
        let text = "---\nmodel: mimo-v2.6-pro\n---\nBe brief.\n";
        // Then it still parses
        let (model, body) = parse_prompt_file(text).expect("parses");
        assert_eq!(model, "mimo-v2.6-pro");
        assert_eq!(body, "Be brief.\n");
    }

    #[test]
    fn should_reject_a_file_without_a_model_field_because_its_owner_is_unknown() {
        // Given frontmatter with no `model`
        // Then the file is not injectable — a guessed owner would hand one
        // model another model's prompt
        assert!(parse_prompt_file("---\nother: x\n---\nBody.\n").is_none());
        // And a file with no frontmatter at all is likewise ownerless
        assert!(parse_prompt_file("Just a body.\n").is_none());
    }

    #[test]
    fn should_treat_an_empty_body_as_not_injectable() {
        // Given a frontmatter-only file (the "explicitly no prompt" marker)
        // Then nothing is injected
        assert!(!prompt_body_injectable(""));
        assert!(!prompt_body_injectable("  \n\t"));
        // And a body within the limit is injectable
        assert!(prompt_body_injectable("Be brief."));
    }

    #[test]
    fn should_return_the_prompt_unchanged_when_the_model_has_no_body() {
        // Given an assembled prompt and a model with no prompt file
        let prompt = "Base.\n\n## Working Directory\n/tmp\n".to_string();
        // When appending nothing
        // Then the result is byte-identical (D-M13.4: no feature, no change)
        assert_eq!(append_model_prompt(prompt.clone(), None), prompt);
        // And a blank body is the same as none
        assert_eq!(append_model_prompt(prompt.clone(), Some("   \n")), prompt);
    }

    #[test]
    fn should_append_after_the_existing_prompt_when_a_body_is_present() {
        // Given an assembled prompt
        let prompt = "Base.\n\n## Working Directory\n/tmp".to_string();
        // When a body is appended
        let result = append_model_prompt(prompt.clone(), Some("Be brief.\n"));
        // Then the original text is untouched and the body follows it
        assert!(result.starts_with(&prompt));
        assert!(result.ends_with("Be brief.\n"));
    }

    #[test]
    fn should_answer_with_the_body_for_an_alias_whose_raw_name_shares_a_file() {
        // Given one raw model name reached through two provider aliases
        let mut by_raw = std::collections::BTreeMap::new();
        by_raw.insert("mimo-v2.6-pro".to_string(), "Be brief.".to_string());
        let mut ids = std::collections::BTreeMap::new();
        ids.insert(
            "xiaomi/mimo-v2.6-pro".to_string(),
            "mimo-v2.6-pro".to_string(),
        );
        ids.insert(
            "openrouter/mimo-v2.6-pro".to_string(),
            "mimo-v2.6-pro".to_string(),
        );
        let table = ModelPromptTable::new(by_raw, ids);
        // Then both aliases resolve to the same body (sharing is by raw name)
        assert_eq!(table.body_for("xiaomi/mimo-v2.6-pro"), Some("Be brief."));
        assert_eq!(
            table.body_for("openrouter/mimo-v2.6-pro"),
            Some("Be brief.")
        );
        // And an unconfigured alias answers nothing
        assert_eq!(table.body_for("other-model"), None);
    }

    #[test]
    fn should_report_empty_when_no_model_carries_a_body() {
        // Given a table with ids but no bodies
        let mut ids = std::collections::BTreeMap::new();
        ids.insert("a".to_string(), "raw-a".to_string());
        let table = ModelPromptTable::new(Default::default(), ids);
        // Then the caller can short-circuit the whole feature
        assert!(table.is_empty());
        assert_eq!(table.len(), 0);
    }

    #[test]
    fn should_refuse_an_oversized_body_rather_than_truncate_it() {
        // Given a body one byte over the limit
        let body = "x".repeat(MAX_PROMPT_BODY_BYTES + 1);
        // Then it is refused whole — a truncated directive would be followed
        // as if complete
        assert!(!prompt_body_injectable(&body));
        // And exactly at the limit is accepted
        assert!(prompt_body_injectable(&"x".repeat(MAX_PROMPT_BODY_BYTES)));
    }
}
