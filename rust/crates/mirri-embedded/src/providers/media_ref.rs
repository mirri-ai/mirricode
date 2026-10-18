//! `fileref:` — the transient provider file-reference URL scheme (T11a,
//! F-P8-5).
//!
//! The media materializer rewrites a message's inline `data:` image URI onto
//! a `fileref:{provider};{file_id}` URL in the *transient chat params only*
//! — context memory keeps the provider-neutral `data:` URI and wire records
//! keep their `blobref:` offloads, so nothing persisted ever carries the
//! scheme. The provider adapter's request builder recognizes the prefix and
//! emits the provider's native file reference (Anthropic:
//! `source.type == "file"`).
//!
//! Naming follows the `blobref:` / `ms://` precedent: a namespaced,
//! engine-internal URL that never crosses a public API. Because a client
//! must never be able to inject one (Anthropic: "never accept file IDs
//! from untrusted sources"), the `message.send` entry rejects user-submitted
//! parts carrying the prefix (brief §4.6, red-line tested).

/// The URL prefix every transient file reference carries (`"fileref:"`).
pub const FILEREF_PREFIX: &str = "fileref:";

/// Format a transient file-reference URL: `fileref:{provider};{file_id}`.
pub fn format_fileref(provider: &str, file_id: &str) -> String {
    format!("{FILEREF_PREFIX}{provider};{file_id}")
}

/// Parse a `fileref:` URL into its file id, but only when it belongs to
/// `provider` (a reference minted for another provider's channel is not
/// ours to consume). `None` for anything else.
pub fn parse_fileref<'a>(url: &'a str, provider: &str) -> Option<&'a str> {
    let rest = url.strip_prefix(FILEREF_PREFIX)?;
    let (owner, file_id) = rest.split_once(';')?;
    (owner == provider).then_some(file_id)
}

/// Whether `url` is a `fileref:` reference (any provider).
pub fn is_fileref(url: &str) -> bool {
    url.starts_with(FILEREF_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_format_a_provider_scoped_reference_when_given_a_file_id() {
        // Given a provider and a file id
        // When the reference is formatted
        // Then it carries the prefix, provider, and id
        assert_eq!(
            format_fileref("anthropic", "file-abc123"),
            "fileref:anthropic;file-abc123"
        );
    }

    #[test]
    fn should_parse_the_file_id_when_the_provider_matches() {
        // Given a reference minted for anthropic
        let url = format_fileref("anthropic", "file-abc123");

        // When parsed as an anthropic reference
        // Then the file id comes back
        assert_eq!(parse_fileref(&url, "anthropic"), Some("file-abc123"));
    }

    #[test]
    fn should_refuse_the_reference_when_the_provider_differs() {
        // Given a reference minted for anthropic
        let url = format_fileref("anthropic", "file-abc123");

        // When parsed as a kimi reference
        // Then it is not consumed (the channel is not ours)
        assert_eq!(parse_fileref(&url, "kimi"), None);
    }

    #[test]
    fn should_not_parse_urls_outside_the_scheme() {
        // Given URLs that are not file references
        let urls = [
            "data:image/png;base64,iVBORw0KGgo=",
            "https://example.com/cat.png",
            "blobref:image/png;sha256-deadbeef",
            "fileref-lookalike;file-abc123",
        ];

        // When parsed for any provider
        // Then none is consumed
        for url in urls {
            assert_eq!(parse_fileref(url, "anthropic"), None, "url: {url}");
        }
    }

    #[test]
    fn should_detect_the_scheme_when_checking_any_provider() {
        // Given a reference and a non-reference URL
        // Then only the reference is detected
        assert!(is_fileref("fileref:anthropic;file-abc123"));
        assert!(!is_fileref("data:image/png;base64,iVBORw0KGgo="));
        assert!(!is_fileref("https://example.com/cat.png"));
        assert!(!is_fileref("blobref:image/png;sha256-deadbeef"));
    }
}
