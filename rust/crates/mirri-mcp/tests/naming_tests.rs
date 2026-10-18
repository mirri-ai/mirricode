//! Tests for MCP tool naming: sanitize, qualify, truncate, and FNV-1a hash.

use mirri_mcp::naming::{fnv1a_hash, qualify_tool_name};

// ---------------------------------------------------------------------------
// Basic qualification
// ---------------------------------------------------------------------------

#[test]
fn should_qualify_names_when_server_and_tool_are_simple() {
    assert_eq!(
        qualify_tool_name("filesystem", "read_file"),
        "mcp__filesystem__read_file"
    );
    assert_eq!(qualify_tool_name("git", "commit"), "mcp__git__commit");
    assert_eq!(qualify_tool_name("db", "query"), "mcp__db__query");
}

#[test]
fn should_preserve_hyphens_and_underscores_when_qualifying_names() {
    assert_eq!(
        qualify_tool_name("my-server", "my_tool"),
        "mcp__my-server__my_tool"
    );
}

// ---------------------------------------------------------------------------
// Sanitization
// ---------------------------------------------------------------------------

#[test]
fn should_sanitize_dots_to_underscores_when_server_name_contains_dots() {
    // "server.name" → "server_name"
    let name = qualify_tool_name("server.name", "tool");
    assert_eq!(name, "mcp__server_name__tool");
}

#[test]
fn should_sanitize_spaces_to_underscores_when_names_contain_spaces() {
    let name = qualify_tool_name("my server", "my tool");
    assert_eq!(name, "mcp__my_server__my_tool");
}

#[test]
fn should_sanitize_slashes_to_underscores_when_server_name_contains_slashes() {
    let name = qualify_tool_name("path/to/server", "tool");
    assert_eq!(name, "mcp__path_to_server__tool");
}

#[test]
fn should_sanitize_non_ascii_when_server_name_contains_unicode() {
    // "café" → "caf_"
    let name = qualify_tool_name("café", "tool");
    assert_eq!(name, "mcp__caf___tool");
}

#[test]
fn should_sanitize_colons_when_server_name_contains_colons() {
    let name = qualify_tool_name("namespace:server", "tool");
    assert_eq!(name, "mcp__namespace_server__tool");
}

// ---------------------------------------------------------------------------
// Truncation + hash
// ---------------------------------------------------------------------------

#[test]
fn should_not_truncate_when_name_is_under_64_chars() {
    let name = qualify_tool_name("short", "tool");
    assert!(name.len() <= 64);
    assert!(!name.contains('_') || name.matches('_').count() >= 2); // has the mcp__ prefix underscores
    assert_eq!(name, "mcp__short__tool");
}

#[test]
fn should_not_truncate_when_name_is_exactly_64_chars() {
    // mcp__ (5) + server (28) + __ (2) + tool (29) = 64
    let server = "s".repeat(28);
    let tool = "t".repeat(29);
    let name = qualify_tool_name(&server, &tool);
    assert_eq!(name.len(), 64);
    // Should not have a hash suffix (the last 9 chars are all 't's).
    assert!(name.ends_with(&"t".repeat(9)));
}

#[test]
fn should_truncate_when_name_is_over_64_chars() {
    let server = "a".repeat(50);
    let tool = "b".repeat(50);
    let name = qualify_tool_name(&server, &tool);

    assert_eq!(name.len(), 64);
}

#[test]
fn should_have_hash_suffix_when_name_is_truncated() {
    let server = "x".repeat(40);
    let tool = "y".repeat(40);
    let name = qualify_tool_name(&server, &tool);

    assert_eq!(name.len(), 64);

    // The last 9 characters should be _<8 hex digits>.
    let suffix = &name[name.len() - 9..];
    assert!(suffix.starts_with('_'));
    let hex_part = &suffix[1..];
    assert_eq!(hex_part.len(), 8);
    u32::from_str_radix(hex_part, 16).expect("hash suffix should be valid hex");
}

#[test]
fn should_preserve_uniqueness_when_truncating_different_long_names() {
    // Two different long names should produce different qualified names.
    let name1 = qualify_tool_name(&"a".repeat(40), &"b".repeat(40));
    let name2 = qualify_tool_name(&"c".repeat(40), &"d".repeat(40));
    assert_ne!(name1, name2);
}

#[test]
fn should_be_deterministic_when_truncating_same_long_name() {
    let server = "a".repeat(40);
    let tool = "b".repeat(40);
    let name1 = qualify_tool_name(&server, &tool);
    let name2 = qualify_tool_name(&server, &tool);
    assert_eq!(name1, name2);
}

#[test]
fn should_produce_different_hashes_when_long_names_differ_only_in_truncated_part() {
    // Even though the truncated portion might be the same, the hash should
    // differ because the original names differ.
    let name1 = qualify_tool_name(&"a".repeat(60), "tool");
    let name2 = qualify_tool_name(&"b".repeat(60), "tool");
    assert_ne!(name1, name2);
}

// ---------------------------------------------------------------------------
// FNV-1a hash
// ---------------------------------------------------------------------------

#[test]
fn should_return_offset_basis_when_input_is_empty() {
    // FNV-1a 32-bit offset basis.
    assert_eq!(fnv1a_hash(""), 0x811c9dc5);
}

#[test]
fn should_match_known_test_vector_when_input_is_single_char() {
    // Known FNV-1a test vector: "a" → 0xe40c292c
    assert_eq!(fnv1a_hash("a"), 0xe40c292c);
}

#[test]
fn should_match_known_test_vector_when_input_is_foobar() {
    // Known FNV-1a test vector: "foobar" → 0xbf9cf968
    assert_eq!(fnv1a_hash("foobar"), 0xbf9cf968);
}

#[test]
fn should_be_deterministic_when_hashing_same_input() {
    let h1 = fnv1a_hash("test string");
    let h2 = fnv1a_hash("test string");
    assert_eq!(h1, h2);
}

#[test]
fn should_produce_different_hashes_when_inputs_differ() {
    let h1 = fnv1a_hash("hello");
    let h2 = fnv1a_hash("world");
    assert_ne!(h1, h2);
}

// ---------------------------------------------------------------------------
// Edge cases
// ---------------------------------------------------------------------------

#[test]
fn should_qualify_name_when_server_name_is_empty() {
    let name = qualify_tool_name("", "tool");
    assert_eq!(name, "mcp_____tool");
}

#[test]
fn should_qualify_name_when_tool_name_is_empty() {
    let name = qualify_tool_name("server", "");
    assert_eq!(name, "mcp__server___");
}

#[test]
fn should_qualify_name_when_both_server_and_tool_are_empty() {
    let name = qualify_tool_name("", "");
    // mcp__ (2 underscores) + _ (server) + __ (separator) + _ (tool) = 6 underscores
    assert_eq!(name, "mcp______");
}

#[test]
fn should_truncate_correctly_when_names_are_very_long() {
    let server = "s".repeat(200);
    let tool = "t".repeat(200);
    let name = qualify_tool_name(&server, &tool);
    assert_eq!(name.len(), 64);
}

#[test]
fn should_start_with_mcp_prefix_when_qualified() {
    let name = qualify_tool_name("server", "tool");
    assert!(name.starts_with("mcp__"));
}
