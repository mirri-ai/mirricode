//! Prompt-derived session titles.
//!
//! TS-parity port of the v1 kernel's `updatePromptMetadata` title rule: while
//! a session's title is neither user-set nor already derived, the first user
//! prompt becomes the title. The prompt text is sanitized before it is used —
//! a sidebar must never display credentials that happened to ride the first
//! prompt — and truncated to 200 characters (the `session.rename` limit).

use std::sync::OnceLock;

use regex::Regex;

/// The maximum title length (matches `session.rename`'s validation and the
/// TS kernel's `MAX_TITLE_LENGTH`).
const MAX_TITLE_LENGTH: usize = 200;

/// Titles that count as "not yet titled" — anything the engine itself would
/// have written before a real title exists. `"New Session"` is TS parity;
/// `"Untitled Session"` is this engine's default.
const UNTITLED_TITLES: [&str; 2] = ["Untitled Session", "New Session"];

/// Whether `title` is a placeholder rather than a real, derived title.
pub fn is_untitled_title(title: &str) -> bool {
    let trimmed = title.trim();
    trimmed.is_empty() || UNTITLED_TITLES.contains(&trimmed)
}

/// Derive a session title from a user prompt (TS `titleFromPromptMetadataText`
/// over the sanitized prompt text): redact credential-looking substrings,
/// collapse control characters and whitespace, trim, and truncate to
/// [`MAX_TITLE_LENGTH`] characters. Returns `None` when nothing displayable
/// remains (e.g. an image-only prompt).
pub fn title_from_prompt_text(text: &str) -> Option<String> {
    let sanitized = sanitize(text);
    if sanitized.is_empty() {
        return None;
    }
    Some(sanitized.chars().take(MAX_TITLE_LENGTH).collect())
}

struct SanitizeRegexes {
    private_key: Regex,
    bearer: Regex,
    secret_assignment: Regex,
    sk_token: Regex,
    long_token: Regex,
    control_chars: Regex,
    whitespace: Regex,
}

fn sanitize_regexes() -> &'static SanitizeRegexes {
    static REGEXES: OnceLock<SanitizeRegexes> = OnceLock::new();
    REGEXES.get_or_init(|| SanitizeRegexes {
        private_key: Regex::new(
            r"(?i)-----BEGIN [^-]*PRIVATE KEY-----[\s\S]*?-----END [^-]*PRIVATE KEY-----",
        )
        .expect("valid private-key regex"),
        bearer: Regex::new(r"(?i)\b(authorization)\s*:\s*bearer\s+\S+")
            .expect("valid bearer regex"),
        secret_assignment: Regex::new(
            r#"(?i)\b(api[_-]?key|token|secret|password|passwd|pwd)\b\s*[:=]\s*(?:"[^"]*"|'[^']*'|\S+)"#,
        )
        .expect("valid secret-assignment regex"),
        sk_token: Regex::new(r"\bsk-[A-Za-z0-9_-]{12,}\b").expect("valid sk-token regex"),
        long_token: Regex::new(r"\b[A-Za-z0-9][A-Za-z0-9+/=_-]{39,}\b")
            .expect("valid long-token regex"),
        control_chars: Regex::new(r"\p{Cc}+").expect("valid control-char regex"),
        whitespace: Regex::new(r"\s+").expect("valid whitespace regex"),
    })
}

/// The TS `sanitizeAndTruncatePromptText` rules: redact private keys, bearer
/// headers, secret-looking assignments, and token-looking blobs; replace
/// control characters and collapse runs of whitespace to single spaces.
fn sanitize(text: &str) -> String {
    let regexes = sanitize_regexes();
    let redacted = regexes.private_key.replace_all(text, "[redacted]");
    let redacted = regexes
        .bearer
        .replace_all(&redacted, "$1: Bearer [redacted]");
    let redacted = regexes
        .secret_assignment
        .replace_all(&redacted, "$1=[redacted]");
    let redacted = regexes.sk_token.replace_all(&redacted, "[redacted]");
    let redacted = regexes.long_token.replace_all(&redacted, "[redacted]");
    let collapsed = regexes.control_chars.replace_all(&redacted, " ");
    let collapsed = regexes.whitespace.replace_all(&collapsed, " ");
    collapsed.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_treat_placeholder_and_empty_titles_as_untitled() {
        // Given the engine default and the TS-era placeholder titles
        for title in ["Untitled Session", "New Session", "  ", ""] {
            // When checked
            // Then they count as untitled
            assert!(is_untitled_title(title), "'{title}' should be untitled");
        }
        // And any real title does not
        assert!(!is_untitled_title("Write a hello world sample"));
    }

    #[test]
    fn should_derive_title_from_plain_prompt_text() {
        // Given a plain user prompt
        // When a title is derived
        let title = title_from_prompt_text("  fix the login bug\nin the auth module  ").unwrap();
        // Then the text is trimmed and whitespace-collapsed
        assert_eq!(title, "fix the login bug in the auth module");
    }

    #[test]
    fn should_truncate_long_prompts_to_200_chars() {
        // Given a prompt longer than the title limit (plain words, so no
        // token-redaction rule collapses it)
        let long_prompt = "fix the login bug in the auth module ".repeat(20);
        // When a title is derived
        let title = title_from_prompt_text(&long_prompt).unwrap();
        // Then it is truncated to the limit
        assert_eq!(title.chars().count(), 200);
    }

    #[test]
    fn should_redact_credentials_before_deriving_title() {
        // Given prompts carrying credential-looking substrings
        let cases = [
            (
                "deploy with api_key: sk-abcdef1234567890",
                "deploy with api_key=[redacted]",
            ),
            // NOTE: the trailing quote is consumed by the bearer pattern's
            // greedy \S+ — the same behavior as the TS kernel's regex.
            (
                "curl -H 'Authorization: Bearer eyJhbGciOi'",
                "curl -H 'Authorization: Bearer [redacted]",
            ),
            (
                "password: hunter2 restart the db",
                "password=[redacted] restart the db",
            ),
        ];
        for (prompt, expected) in cases {
            // When a title is derived
            let title = title_from_prompt_text(prompt).unwrap();
            // Then the credential is redacted
            assert_eq!(title, expected, "prompt: {prompt}");
        }
    }

    #[test]
    fn should_redact_private_key_blocks_before_deriving_title() {
        // Given a prompt containing a private key block
        let prompt = "sign this:\n-----BEGIN RSA PRIVATE KEY-----\nabc\n-----END RSA PRIVATE KEY-----\nthanks";
        // When a title is derived
        let title = title_from_prompt_text(prompt).unwrap();
        // Then the key body is gone
        assert_eq!(title, "sign this: [redacted] thanks");
    }

    #[test]
    fn should_return_none_when_nothing_displayable_remains() {
        // Given a prompt that sanitizes to nothing
        // When a title is derived
        // Then none is returned (the title stays as-is)
        assert_eq!(title_from_prompt_text("   "), None);
        assert_eq!(title_from_prompt_text(""), None);
    }
}
