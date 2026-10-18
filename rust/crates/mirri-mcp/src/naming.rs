//! Tool naming utilities.
//!
//! MCP tools are namespaced by their parent server name to avoid collisions
//! when multiple servers advertise tools with the same local name (e.g.
//! every filesystem server has a `read_file` tool).
//!
//! The qualified name format is:
//!
//! ```text
//! mcp__<sanitized_server>__<sanitized_tool>
//! ```
//!
//! Names are sanitized by replacing characters outside `[A-Za-z0-9_-]` with
//! underscores. When the fully qualified name exceeds 64 characters, it is
//! truncated and suffixed with a 8-hex-digit FNV-1a hash of the original
//! name to preserve uniqueness.

/// Maximum length of a qualified tool name before truncation is applied.
const MAX_TOOL_NAME_LENGTH: usize = 64;

/// Format: `mcp__<server>__<tool>`.
const NAME_PREFIX: &str = "mcp__";

/// Produce a qualified, collision-resistant tool name from a server name
/// and a tool name.
///
/// # Examples
///
/// ```
/// use mirri_mcp::naming::qualify_tool_name;
///
/// let name = qualify_tool_name("filesystem", "read_file");
/// assert_eq!(name, "mcp__filesystem__read_file");
/// ```
pub fn qualify_tool_name(server_name: &str, tool_name: &str) -> String {
    let sanitized_server = sanitize(server_name);
    let sanitized_tool = sanitize(tool_name);

    // Build the full name. Use the sanitized parts; if sanitization produced
    // an empty string, fall back to "_" so the separator structure is
    // preserved.
    let server_part = if sanitized_server.is_empty() {
        "_"
    } else {
        &sanitized_server
    };
    let tool_part = if sanitized_tool.is_empty() {
        "_"
    } else {
        &sanitized_tool
    };

    let full = format!("{NAME_PREFIX}{server_part}__{tool_part}");

    if full.len() <= MAX_TOOL_NAME_LENGTH {
        return full;
    }

    // Truncate and append hash suffix.
    truncate_with_hash(&full, MAX_TOOL_NAME_LENGTH)
}

/// Replace characters outside `[A-Za-z0-9_-]` with underscores.
///
/// This ensures the resulting name is a valid identifier in most contexts
/// (LLM tool-name schemas, JSON keys, command-line flags, etc.).
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Truncate `name` to `max_len` characters and append an 8-hex-digit hash
/// suffix derived from the original (un-truncated) name.
///
/// The suffix is `_<hash>` (9 chars), so the truncated portion is
/// `max_len - 9` chars long.
fn truncate_with_hash(name: &str, max_len: usize) -> String {
    let hash = fnv1a_hash(name);
    let suffix = format!("_{hash:08x}");
    let suffix_len = suffix.len();

    // Truncate the name so that name + suffix == max_len.
    let keep_len = max_len.saturating_sub(suffix_len);

    // Truncate at a char boundary to avoid splitting a multi-byte char.
    let truncated: String = name.chars().take(keep_len).collect();

    format!("{truncated}{suffix}")
}

/// Compute the 32-bit FNV-1a hash of a string.
///
/// FNV-1a is a simple, fast, non-cryptographic hash with good distribution
/// for short strings. It is used here purely for name disambiguation after
/// truncation — it does not need to be cryptographically secure.
///
/// Algorithm:
/// 1. Start with the 32-bit FNV offset basis: `0x811c9dc5`.
/// 2. For each byte: `hash = hash XOR byte`, then `hash = hash * 0x01000193`.
pub fn fnv1a_hash(s: &str) -> u32 {
    const FNV_OFFSET_BASIS: u32 = 0x811c9dc5;
    const FNV_PRIME: u32 = 0x0100_0193;

    let mut hash = FNV_OFFSET_BASIS;
    for byte in s.bytes() {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_qualify_tool_name_when_server_and_tool_are_simple() {
        assert_eq!(
            qualify_tool_name("filesystem", "read_file"),
            "mcp__filesystem__read_file"
        );
    }

    #[test]
    fn should_replace_special_chars_when_sanitizing_name() {
        assert_eq!(sanitize("my-server.name"), "my-server_name");
        assert_eq!(sanitize("foo bar"), "foo_bar");
        assert_eq!(sanitize("already_clean"), "already_clean");
    }

    #[test]
    fn should_return_empty_string_when_input_is_empty() {
        assert_eq!(sanitize(""), "");
    }

    #[test]
    fn should_use_underscore_placeholder_when_server_or_tool_name_is_empty() {
        assert_eq!(qualify_tool_name("", "tool"), "mcp_____tool");
        assert_eq!(qualify_tool_name("server", ""), "mcp__server___");
    }
    #[test]
    fn should_truncate_name_when_exceeding_max_length() {
        let long_server = "a".repeat(40);
        let long_tool = "b".repeat(40);
        let name = qualify_tool_name(&long_server, &long_tool);

        assert_eq!(name.len(), MAX_TOOL_NAME_LENGTH);
        // The suffix should be _<8 hex chars>.
        let suffix = &name[name.len() - 9..];
        assert!(suffix.starts_with('_'));
        // The 8 chars after '_' should be valid hex.
        let hex_part = &suffix[1..];
        u32::from_str_radix(hex_part, 16).expect("suffix should be valid hex");
    }

    #[test]
    fn should_produce_same_result_when_qualifying_long_name_twice() {
        let long_server = "a".repeat(40);
        let long_tool = "b".repeat(40);
        let name1 = qualify_tool_name(&long_server, &long_tool);
        let name2 = qualify_tool_name(&long_server, &long_tool);
        assert_eq!(name1, name2);
    }

    #[test]
    fn should_match_known_values_when_computing_fnv1a_hash() {
        // FNV-1a 32-bit: empty string → offset basis.
        assert_eq!(fnv1a_hash(""), 0x811c9dc5);
        // FNV-1a 32-bit: "a" → 0xe40c292c
        assert_eq!(fnv1a_hash("a"), 0xe40c292c);
        // FNV-1a 32-bit: "foobar" → 0xbf9cf968
        assert_eq!(fnv1a_hash("foobar"), 0xbf9cf968);
    }

    #[test]
    fn should_not_truncate_when_name_is_exactly_max_length() {
        // Build a name that is exactly 64 chars.
        // "mcp__" (5) + server + "__" (2) + tool = 64
        // server + tool = 57
        // server = 28, tool = 29
        let server = "s".repeat(28);
        let tool = "t".repeat(29);
        let name = qualify_tool_name(&server, &tool);
        assert_eq!(name.len(), 64);
        // No hash suffix (no '_<hex>' at the end beyond the normal structure).
        assert!(!name.ends_with(&format!("_{:08x}", fnv1a_hash(&name))));
    }

    #[test]
    fn should_replace_non_ascii_chars_when_sanitizing_name() {
        assert_eq!(sanitize("café"), "caf_");
        assert_eq!(sanitize("héllo"), "h_llo");
    }
}
