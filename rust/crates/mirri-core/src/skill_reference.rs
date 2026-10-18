//! Inline skill/command reference links (glossary SK7, skills US3.6 / ADR-004).
//!
//! A message body is the single source of truth for inline activation: a
//! reference is an inline markdown link whose link text starts with a skill
//! trigger symbol (`$` plus the full-width `¥` / `￥` variants) and whose
//! target is either a `SKILL.md` file path (skill), a `builtin://` pseudo
//! path (builtin skill), or a `command:<name>` pseudo-target (command). Plain
//! markdown links never activate — the trigger prefix plus a legal target is
//! the machine-checkable activation judgment (the double-gated rule of
//! SDR-005).
//!
//! This module is the rust counterpart of the TS `parseInlineReferences`
//! shared parser (`packages/protocol/src/skill-reference.ts`), re-implemented
//! with identical semantics — same trigger set, same target classification,
//! same body-replacement shape (`[display]` marks re-parse idempotently).
//! It is a pure string function (no IO), so it lives in `mirri-core` without
//! breaking the zero-IO guard.
//!
//! Resolution authority (ADR-004): the parser classifies a reference's kind
//! from its target text alone; *activating* a skill (matching the SKILL.md
//! path against the catalog) is the caller's job.

/// Trigger symbols that mark an inline link as an activatable reference.
pub const SKILL_TRIGGER_CHARS: [char; 3] = ['$', '¥', '￥'];

/// Pseudo-target prefix marking an inline reference as a slash command.
pub const COMMAND_TARGET_PREFIX: &str = "command:";

/// Pseudo-target prefix identifying a builtin skill reference (ADR-007 D4) —
/// the form the web chips mint for builtin skills and the catalog stamps on
/// builtin registrations (mirror of the TS `BUILTIN_TARGET_PREFIX`).
pub const BUILTIN_TARGET_PREFIX: &str = "builtin://";

/// Reference kind: skill (SKILL.md target) or command (`command:` target).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InlineReferenceKind {
    Skill,
    Command,
}

/// One parsed inline reference (the TS `InlineReference` shape).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineRef {
    pub kind: InlineReferenceKind,
    /// Display name without the trigger char (e.g. `alpha` for `[$alpha]`).
    pub display: String,
    /// Skill: absolute path to the skill's SKILL.md, or the `builtin://<name>`
    /// pseudo path (the resolution authority — which file the link points at
    /// is which skill gets activated; builtin targets are normalized to the
    /// clean `builtin://` form). Command: `command:<canonical command name>`.
    pub target: String,
    /// Start offset of the full link in the original text.
    pub start: usize,
    /// End offset (exclusive) of the full link in the original text.
    pub end: usize,
}

/// The result of parsing a message body (the TS `ParsedInlineReferences`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedInlineReferences {
    /// Text with each reference replaced by its bare display name.
    pub body: String,
    /// References in order of appearance.
    pub references: Vec<InlineRef>,
}

fn is_command_target(target: &str) -> bool {
    target.starts_with(COMMAND_TARGET_PREFIX)
}

/// A target is a skill only when it is a `SKILL.md` file path (checked case
/// insensitively for the extension) or a `builtin://` pseudo path — mirroring
/// the TS `isSkillTarget` (ADR-007 D4: builtin chips carry the pseudo target).
fn is_skill_target(target: &str) -> bool {
    target.starts_with(BUILTIN_TARGET_PREFIX) || target.to_ascii_lowercase().ends_with("skill.md")
}

/// Normalize a builtin target: strip the stale `/SKILL.md` suffix (case-
/// insensitive) that pre-fix insertion sites persisted on `builtin://` pseudo
/// paths, so classification and resolution see the clean `builtin://name`
/// form (mirror of the TS `normalizeBuiltinTarget`). Non-builtin targets pass
/// through untouched; only the exact trailing suffix strips, so legitimate
/// slash-shaped builtin names (the sub-skill family) survive verbatim.
///
/// `pub` so the catalog resolver applies THE SAME normalization to replay-time
/// journal targets that skip the parser (ADR-007 D4: one consistent point).
pub fn normalize_builtin_target(target: &str) -> String {
    if !target.starts_with(BUILTIN_TARGET_PREFIX) {
        return target.to_string();
    }
    const STALE_SUFFIX: &str = "/skill.md";
    let lower = target.to_ascii_lowercase();
    if lower.ends_with(STALE_SUFFIX) {
        target[..target.len() - STALE_SUFFIX.len()].to_string()
    } else {
        target.to_string()
    }
}

/// Format a reference as the canonical inline link form — inverse of a single
/// parse step. Mirror of the TS `formatSkillReferenceLink`: the first trigger
/// char (`$`) plus the display name escaped for markdown, then the target.
pub fn format_skill_reference_link(display: &str, target: &str) -> String {
    let text = display.replace('[', "\\[").replace(']', "\\]");
    format!("[{}{}]({})", SKILL_TRIGGER_CHARS[0], text, target)
}

/// Parse all activatable inline references from a message text (the TS
/// `parseInlineReferences`).
///
/// Links without a trigger prefix are left untouched (plain markdown). A
/// trigger link whose target is neither a SKILL.md path nor a `command:`
/// pseudo-target is also left untouched (invalid target = not activatable).
///
/// The parser is a pure string scan — no regex dependency, no IO. It walks
/// the text looking for `[` and consumes a markdown link; the trigger-prefix
/// and target checks decide whether a matched link is a reference.
pub fn parse_inline_references(text: &str) -> ParsedInlineReferences {
    let mut references: Vec<InlineRef> = Vec::new();
    let mut replacements: Vec<(usize, usize, String)> = Vec::new();
    let chars: Vec<char> = text.chars().collect();

    let mut i = 0;
    let n = chars.len();
    while i < n {
        if chars[i] != '[' {
            i += 1;
            continue;
        }
        // Try to consume a markdown link starting at `[`.
        // Link text: everything up to the closing `]` (no `]` inside).
        let mut j = i + 1;
        while j < n && chars[j] != ']' {
            j += 1;
        }
        if j >= n {
            // Unclosed bracket — not a link; advance past the `[`.
            i += 1;
            continue;
        }
        // Link text is chars[i+1..j]; must be non-empty.
        if j == i + 1 {
            i += 1;
            continue;
        }
        // After `]` there must be `(`.
        if j + 1 >= n || chars[j + 1] != '(' {
            i += 1;
            continue;
        }
        // Target: everything up to the first `)` — non-empty, no whitespace.
        // The TS regex `\(([^)\s]+)\)` — no `)` and no whitespace in target.
        let mut k = j + 2;
        while k < n && chars[k] != ')' && !chars[k].is_whitespace() {
            k += 1;
        }
        // Require a closing `)` immediately (no whitespace consumed).
        if k >= n || chars[k] != ')' {
            i += 1;
            continue;
        }
        // Build the target string via char slices.
        let target: String = chars[j + 2..k].iter().collect();

        // The first char of the link text is the trigger; the display is the
        // remainder. Char-slice the display — the trigger may be multi-byte
        // (¥/￥), so a byte slice `link_text[1..]` would panic for them.
        let first_char = chars[i + 1];
        if !SKILL_TRIGGER_CHARS.contains(&first_char) {
            i += 1;
            continue;
        }
        let display: String = chars[i + 2..j].iter().collect();
        let display = display.trim();
        if display.is_empty() {
            i += 1;
            continue;
        }
        let start = byte_offset(&chars, i);
        let end = byte_offset(&chars, k) + 1; // include the closing ')'
        let kind = if is_command_target(&target) {
            Some(InlineReferenceKind::Command)
        } else if is_skill_target(&target) {
            Some(InlineReferenceKind::Skill)
        } else {
            None
        };
        let Some(kind) = kind else {
            i += 1;
            continue;
        };
        // Builtin targets normalize to the clean `builtin://name` form (the
        // stale `/SKILL.md` suffix of historical messages strips here, so
        // `InlineRef.target` and downstream resolution never see the dirty
        // shape — mirror of the TS `parseInlineReferences`).
        let target = normalize_builtin_target(&target);
        references.push(InlineRef {
            kind,
            display: display.to_string(),
            target,
            start,
            end,
        });
        // Body replace: keep the bracketed `[display]` (re-parse idempotent).
        replacements.push((start, end, format!("[{display}]")));
        i = k;
    }

    // Rebuild the body by splicing replacement spans.
    let mut body = String::with_capacity(text.len());
    let mut cursor = 0;
    for (start, end, replacement) in replacements {
        body.push_str(&text[cursor..start]);
        body.push_str(&replacement);
        cursor = end;
    }
    body.push_str(&text[cursor..]);

    ParsedInlineReferences { body, references }
}

/// Byte offset of the given char index within `text` (mirrored since `chars`
/// is collected from `text`; the char at `idx` starts at this byte offset).
fn byte_offset(chars: &[char], char_index: usize) -> usize {
    chars[..char_index].iter().map(|c| c.len_utf8()).sum()
}

/// Collapse consecutive whitespace between two adjacent references into a
/// single separator space — cosmetic normalization only (the TS
/// `collapseInlineWhitespace`, SDR-005 "空格收敛"). Never touches spaces
/// elsewhere in the text; also trims leading/trailing whitespace.
///
/// The rust implementation is a linear scan: after a reference's closing
/// `)`, a run of 2+ whitespace chars followed by a `[<trigger>` collapses.
pub fn collapse_inline_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut i = 0;
    while i < n {
        // Detect "after a reference target's `)`".
        if chars[i] == ')'
            // Heuristic: a reference target `)` is preceded by non-`)` content
            // and followed by a whitespace run then a trigger link. We only
            // need the target `)` — a bare markdown link's `)` is equally safe
            // to collapse after, matching the TS gap regex which keys on
            // `](target)` regardless of trigger on the left side.
            && i + 1 < n
            && chars[i + 1].is_whitespace()
        {
            // Consume the whitespace run.
            let run_start = i + 1;
            let mut j = run_start;
            while j < n && chars[j].is_whitespace() {
                j += 1;
            }
            // Next must begin a trigger-prefixed link to collapse.
            if j < n && chars[j] == '['
                && let Some(trigger) = chars.get(j + 1)
                && SKILL_TRIGGER_CHARS.contains(trigger)
            {
                // Run length decides: only collapse runs of 2+ into one.
                if j - run_start >= 2 {
                    out.push(')');
                    out.push(' ');
                    // Skip the whitespace run entirely.
                    i = j;
                    continue;
                }
            }
            out.push(chars[i]);
            i += 1;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_parse_multiple_references_in_order_when_message_has_several() {
        // Given a message with two inline skill references
        let text = "use [$alpha](/a/alpha/SKILL.md) then [$beta](/b/beta/SKILL.md) ok";
        // When parsed
        let parsed = parse_inline_references(text);
        // Then both references come back in appearance order with correct kinds
        assert_eq!(parsed.references.len(), 2);
        assert_eq!(parsed.references[0].kind, InlineReferenceKind::Skill);
        assert_eq!(parsed.references[0].display, "alpha");
        assert_eq!(parsed.references[0].target, "/a/alpha/SKILL.md");
        assert_eq!(parsed.references[1].kind, InlineReferenceKind::Skill);
        assert_eq!(parsed.references[1].display, "beta");
        assert_eq!(parsed.references[1].target, "/b/beta/SKILL.md");
        // And the body carries the bracketed display names
        assert_eq!(parsed.body, "use [alpha] then [beta] ok");
    }

    #[test]
    fn should_compute_correct_spans_when_parsing_references() {
        // Given a message with a reference embedded mid-sentence
        let text = "a [$x](/s/x/SKILL.md) b";
        // When parsed
        let parsed = parse_inline_references(text);
        // Then the span covers exactly the full markdown link
        let link = "[$x](/s/x/SKILL.md)";
        assert_eq!(&text[parsed.references[0].start..parsed.references[0].end], link);
        assert_eq!(parsed.references[0].start, 2);
        assert_eq!(parsed.references[0].end, 2 + link.len());
    }

    #[test]
    fn should_ignore_plain_links_when_no_trigger_prefix() {
        // Given a plain markdown link (no trigger char in the link text)
        let text = "see [alpha](/a/alpha/SKILL.md) here";
        // When parsed
        let parsed = parse_inline_references(text);
        // Then nothing activates and the body is unchanged
        assert!(parsed.references.is_empty());
        assert_eq!(parsed.body, text);
    }

    #[test]
    fn should_not_treat_dollar_amount_as_reference_when_it_is_plain_text() {
        // Given a bare `$100` with no matching markdown link shape
        let text = "cost $100 please";
        // When parsed
        let parsed = parse_inline_references(text);
        // Then no reference is produced and the body is unchanged
        assert!(parsed.references.is_empty());
        assert_eq!(parsed.body, "cost $100 please");
    }

    #[test]
    fn should_ignore_trigger_link_when_target_is_not_a_skill_md_path() {
        // Given a trigger link whose target is neither SKILL.md nor command:
        let text = "use [$alpha](/docs/alpha.md) now";
        // When parsed
        let parsed = parse_inline_references(text);
        // Then it is not activatable (invalid target)
        assert!(parsed.references.is_empty());
        assert_eq!(parsed.body, text);
    }

    #[test]
    fn should_parse_command_reference_when_target_has_command_prefix() {
        // Given a command reference target
        let text = "run [$/clear](command:clear) please";
        // When parsed
        let parsed = parse_inline_references(text);
        // Then it is classified as a command
        assert_eq!(parsed.references.len(), 1);
        assert_eq!(parsed.references[0].kind, InlineReferenceKind::Command);
        assert_eq!(parsed.references[0].display, "/clear");
        assert_eq!(parsed.references[0].target, "command:clear");
        assert_eq!(parsed.body, "run [/clear] please");
    }

    #[test]
    fn should_accept_all_trigger_variants_when_link_text_starts_with_them() {
        // Given trigger links with each of the trigger variants
        let text = "[$a](/s/a/SKILL.md) [¥b](/s/b/SKILL.md) [￥c](/s/c/SKILL.md)";
        // When parsed
        let parsed = parse_inline_references(text);
        // Then all three are recognized as skill references
        assert_eq!(parsed.references.len(), 3);
        assert_eq!(parsed.references[0].display, "a");
        assert_eq!(parsed.references[1].display, "b");
        assert_eq!(parsed.references[2].display, "c");
    }

    #[test]
    fn should_skip_reference_when_link_text_is_only_the_trigger() {
        // Given a link whose text is only the trigger char (empty display)
        let text = "[$](/a/SKILL.md)";
        // When parsed
        let parsed = parse_inline_references(text);
        // Then nothing activates (empty display is not a valid reference)
        assert!(parsed.references.is_empty());
        assert_eq!(parsed.body, text);
    }

    #[test]
    fn should_preserve_leading_trailing_text_when_parsing() {
        // Given a message with reference at the start and trailing content
        let text = "[$a](/a/SKILL.md) and more words.";
        // When parsed
        let parsed = parse_inline_references(text);
        // Then the surrounding text survives
        assert_eq!(parsed.body, "[a] and more words.");
    }

    #[test]
    fn should_return_idempotent_body_when_reparsing() {
        // Given a message whose body replaced references with [display]
        let text = "use [$alpha](/a/alpha/SKILL.md)";
        let parsed = parse_inline_references(text);
        // When the body is re-parsed
        let reparsed = parse_inline_references(&parsed.body);
        // Then no further reference activates (plain [alpha] has no trigger)
        assert!(reparsed.references.is_empty());
        assert_eq!(reparsed.body, parsed.body);
    }

    #[test]
    fn should_format_reference_link_when_given_display_and_target() {
        // Given a display name and a skill target
        let link = format_skill_reference_link("alpha", "/a/alpha/SKILL.md");
        // Then the canonical trigger link form is produced
        assert_eq!(link, "[$alpha](/a/alpha/SKILL.md)");
    }

    #[test]
    fn should_escape_brackets_in_display_when_formatting_link() {
        // Given a display name containing markdown brackets
        let link = format_skill_reference_link("a[b]c", "/s/SKILL.md");
        // Then the brackets are escaped so the link text stays single-level
        assert_eq!(link, "[$a\\[b\\]c](/s/SKILL.md)");
    }

    #[test]
    fn should_collapse_consecutive_whitespace_between_adjacent_references() {
        // Given two adjacent references separated by multiple spaces
        let text = "[$a](/a/SKILL.md)   [$b](/b/SKILL.md)";
        // When collapsed
        let collapsed = collapse_inline_whitespace(text);
        // Then the gap becomes a single separator space
        assert_eq!(collapsed, "[$a](/a/SKILL.md) [$b](/b/SKILL.md)");
    }

    #[test]
    fn should_trim_leading_and_trailing_whitespace_when_collapsing() {
        // Given text with surrounding whitespace and an interior gap
        let text = "  [$a](/a/SKILL.md)    [$b](/b/SKILL.md)  ";
        // When collapsed
        let collapsed = collapse_inline_whitespace(text);
        // Then outer whitespace is trimmed and the inner gap collapses
        assert_eq!(collapsed, "[$a](/a/SKILL.md) [$b](/b/SKILL.md)");
    }

    #[test]
    fn should_not_collapse_single_space_between_references() {
        // Given a single separator space between references
        let text = "[$a](/a/SKILL.md) [$b](/b/SKILL.md)";
        // When collapsed
        let collapsed = collapse_inline_whitespace(text);
        // Then the single space is preserved
        assert_eq!(collapsed, text);
    }

    // ===== builtin:// targets (ADR-007 D4 prerequisite) =====

    #[test]
    fn should_classify_builtin_pseudo_path_target_as_skill_reference() {
        // Given a message referencing a builtin skill by its pseudo path
        let text = "use [$update-config](builtin://update-config) now";
        // When parsed
        let parsed = parse_inline_references(text);
        // Then it classifies as a skill reference with the clean target
        assert_eq!(parsed.references.len(), 1);
        assert_eq!(parsed.references[0].kind, InlineReferenceKind::Skill);
        assert_eq!(parsed.references[0].display, "update-config");
        assert_eq!(parsed.references[0].target, "builtin://update-config");
        assert_eq!(parsed.body, "use [update-config] now");
    }

    #[test]
    fn should_normalize_stale_builtin_suffix_when_target_carries_skill_md() {
        // Given the dirty historical form `builtin://name/SKILL.md`
        let text = "use [$update-config](builtin://update-config/SKILL.md) now";
        // When parsed
        let parsed = parse_inline_references(text);
        // Then it classifies AND the target comes out clean
        assert_eq!(parsed.references.len(), 1);
        assert_eq!(parsed.references[0].kind, InlineReferenceKind::Skill);
        assert_eq!(parsed.references[0].target, "builtin://update-config");
        assert_eq!(parsed.body, "use [update-config] now");
    }

    #[test]
    fn should_normalize_stale_builtin_suffix_case_insensitively() {
        // Given a dirty suffix in a different case
        let text = "[$update-config](builtin://update-config/skill.MD)";
        // When parsed
        let parsed = parse_inline_references(text);
        // Then the suffix is stripped regardless of case
        assert_eq!(parsed.references[0].target, "builtin://update-config");
    }

    #[test]
    fn should_keep_slash_shaped_builtin_targets_when_they_carry_no_stale_suffix() {
        // Given the sub-skill family pseudo path (slash-separated, no suffix)
        let text = "[$review](builtin://sub-skill/review)";
        // When parsed
        let parsed = parse_inline_references(text);
        // Then the target is untouched (only the exact `SKILL.md` suffix strips)
        assert_eq!(parsed.references[0].target, "builtin://sub-skill/review");
    }

    #[test]
    fn should_keep_disk_path_targets_unchanged_when_classifying() {
        // Given a disk SKILL.md target with a mixed-case extension
        let text = "[$alpha](/a/SKILL.MD)";
        // When parsed
        let parsed = parse_inline_references(text);
        // Then the disk target is preserved verbatim (classification is
        // case-insensitive, normalization is builtin-only)
        assert_eq!(parsed.references[0].target, "/a/SKILL.MD");
    }

    #[test]
    fn should_not_classify_builtin_target_when_link_text_has_no_trigger() {
        // Given a plain markdown link to a builtin pseudo path
        let text = "see [update-config](builtin://update-config)";
        // When parsed
        let parsed = parse_inline_references(text);
        // Then nothing activates (triggerless links never classify)
        assert!(parsed.references.is_empty());
        assert_eq!(parsed.body, text);
    }
}