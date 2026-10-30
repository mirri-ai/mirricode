//! Integration tests for plugin manifest parsing, path safety, and field
//! validation.
//!
//! Tests are written in BDD style (`should ... when ...`) and encode the TS
//! v2 benchmark semantics (`packages/agent-core-v2/src/app/plugin/manifest.ts`)
//! for batches B1, B3, B4, B5, B6, B7, B10 and the diagnostics model (B2/K)
//! of `plan/rust-plugin-parity.md`.

use mirri_plugin_host::manifest::{
    PluginAuthor, PluginDiagnosticSeverity, PluginInterface, PluginManifest, PluginManifestKind,
    PluginSessionStart, PluginState, SYSTEM_PROMPT_MAX_BYTES, parse_manifest, validate_manifest,
    validate_path_in_root,
};
use std::io::Write;

fn make_manifest_dir(json: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let manifest_path = dir.path().join("mirri.plugin.json");
    let mut file = std::fs::File::create(&manifest_path).expect("failed to create manifest");
    file.write_all(json.as_bytes())
        .expect("failed to write manifest");
    dir
}

/// Write a manifest candidate at `<root>/<relative>` with the given name.
fn write_candidate(dir: &tempfile::TempDir, relative: &str, name: &str) {
    let path = dir.path().join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, serde_json::json!({ "name": name }).to_string()).unwrap();
}

/// Parse and unwrap the survivor manifest — the happy-path helper.
fn parse_ok(dir: &tempfile::TempDir) -> PluginManifest {
    parse_manifest(dir.path())
        .manifest
        .expect("manifest should parse")
}

/// The diagnostics of a parse, with severity quick-checks.
fn diagnostics_of(dir: &tempfile::TempDir) -> Vec<mirri_plugin_host::manifest::PluginDiagnostic> {
    parse_manifest(dir.path()).diagnostics
}

fn blank_manifest(name: &str) -> PluginManifest {
    PluginManifest {
        name: name.to_string(),
        version: None,
        description: None,
        skills: None,
        agents: None,
        session_start: None,
        mcp_servers: None,
        hooks: None,
        commands: None,
        system_prompt: None,
        system_prompt_path: None,
        interface: None,
        keywords: None,
        author: None,
        homepage: None,
        license: None,
        skill_instructions: None,
    }
}

// ---------------------------------------------------------------------------
// B1 — candidate paths and priority
// ---------------------------------------------------------------------------

#[test]
fn should_parse_manifest_when_root_manifest_is_valid() {
    let dir = make_manifest_dir(r#"{"name":"my-plugin","version":"1.0.0"}"#);
    let manifest = parse_ok(&dir);
    assert_eq!(manifest.name, "my-plugin");
    assert_eq!(manifest.version.as_deref(), Some("1.0.0"));
}

#[test]
fn should_parse_manifest_when_nested_in_mirri_plugin_dir() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let nested = dir.path().join(".mirri-plugin");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(nested.join("plugin.json"), r#"{"name":"nested-plugin"}"#).unwrap();

    let manifest = parse_ok(&dir);
    assert_eq!(manifest.name, "nested-plugin");
}

#[test]
fn should_parse_manifest_when_kimi_plugin_json_exists() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    std::fs::write(
        dir.path().join("kimi.plugin.json"),
        r#"{"name":"kimi-plugin"}"#,
    )
    .unwrap();

    let manifest = parse_ok(&dir);
    assert_eq!(manifest.name, "kimi-plugin");
}

#[test]
fn should_parse_manifest_when_only_kimi_dir_manifest_exists() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    write_candidate(&dir, ".kimi-plugin/plugin.json", "kimi-dir-plugin");

    let manifest = parse_ok(&dir);
    assert_eq!(manifest.name, "kimi-dir-plugin");
}

#[test]
fn should_prefer_mirri_root_manifest_when_all_four_candidates_exist() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    write_candidate(&dir, "mirri.plugin.json", "mirri-root-plugin");
    write_candidate(&dir, ".mirri-plugin/plugin.json", "mirri-dir-plugin");
    write_candidate(&dir, "kimi.plugin.json", "kimi-root-plugin");
    write_candidate(&dir, ".kimi-plugin/plugin.json", "kimi-dir-plugin");

    let manifest = parse_ok(&dir);
    assert_eq!(manifest.name, "mirri-root-plugin");
}

#[test]
fn should_prefer_mirri_dir_manifest_over_kimi_root_manifest() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    write_candidate(&dir, ".mirri-plugin/plugin.json", "mirri-dir-plugin");
    write_candidate(&dir, "kimi.plugin.json", "kimi-root-plugin");
    write_candidate(&dir, ".kimi-plugin/plugin.json", "kimi-dir-plugin");

    let manifest = parse_ok(&dir);
    assert_eq!(manifest.name, "mirri-dir-plugin");
}

#[test]
fn should_prefer_kimi_root_manifest_over_kimi_dir_manifest() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    write_candidate(&dir, "kimi.plugin.json", "kimi-root-plugin");
    write_candidate(&dir, ".kimi-plugin/plugin.json", "kimi-dir-plugin");

    let manifest = parse_ok(&dir);
    assert_eq!(manifest.name, "kimi-root-plugin");
}

#[test]
fn should_report_error_when_manifest_file_is_missing() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let parsed = parse_manifest(dir.path());
    assert!(parsed.manifest.is_none());
    let error = parsed
        .diagnostics
        .iter()
        .find(|d| d.severity == PluginDiagnosticSeverity::Error)
        .expect("an error diagnostic should exist");
    assert!(
        error.message.contains("No manifest at"),
        "unexpected message: {}",
        error.message
    );
}

#[test]
fn should_report_error_when_json_is_invalid() {
    let dir = make_manifest_dir(r#"{"name": broken}"#);
    let parsed = parse_manifest(dir.path());
    assert!(parsed.manifest.is_none());
    let error = parsed
        .diagnostics
        .iter()
        .find(|d| d.severity == PluginDiagnosticSeverity::Error)
        .expect("an error diagnostic should exist");
    assert!(error.message.contains("Failed to parse"), "unexpected message: {}", error.message);
}

#[test]
fn should_report_error_when_manifest_is_not_an_object() {
    let dir = make_manifest_dir(r#"[1, 2, 3]"#);
    let parsed = parse_manifest(dir.path());
    assert!(parsed.manifest.is_none());
    let error = parsed
        .diagnostics
        .iter()
        .find(|d| d.severity == PluginDiagnosticSeverity::Error)
        .expect("an error diagnostic should exist");
    assert_eq!(error.message, "manifest must be a JSON object");
}

#[test]
fn should_carry_kind_and_path_when_manifest_parses() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    write_candidate(&dir, ".kimi-plugin/plugin.json", "kimi-dir-plugin");
    let parsed = parse_manifest(dir.path());
    assert_eq!(parsed.kind, Some(PluginManifestKind::Dir));
    assert!(parsed.path.is_some());
    assert!(parsed.shadowed_path.is_none());
}

#[test]
fn should_carry_shadowed_path_when_root_and_dir_candidates_coexist() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    write_candidate(&dir, "mirri.plugin.json", "mirri-root-plugin");
    write_candidate(&dir, ".mirri-plugin/plugin.json", "mirri-dir-plugin");
    let parsed = parse_manifest(dir.path());
    assert_eq!(parsed.kind, Some(PluginManifestKind::Root));
    // The reported paths are canonical (B8: parseManifest receives a
    // realpath'd root — macOS `/var` → `/private/var`).
    let canonical_root = std::fs::canonicalize(dir.path()).expect("canonicalize");
    assert_eq!(
        parsed.shadowed_path.as_deref(),
        Some(canonical_root.join(".mirri-plugin/plugin.json").as_path())
    );
}

// ---------------------------------------------------------------------------
// B2 — diagnostics model
// ---------------------------------------------------------------------------

#[test]
fn should_report_error_and_no_manifest_when_name_is_invalid() {
    let dir = make_manifest_dir(r#"{"name":"bad name!"}"#);
    let parsed = parse_manifest(dir.path());
    assert!(parsed.manifest.is_none());
    let error = parsed
        .diagnostics
        .iter()
        .find(|d| d.severity == PluginDiagnosticSeverity::Error)
        .expect("an error diagnostic should exist");
    assert!(error.message.contains("\"name\" must match"), "unexpected message: {}", error.message);
}

#[test]
fn should_report_error_when_name_is_missing() {
    let dir = make_manifest_dir(r#"{"version":"1.0.0"}"#);
    let parsed = parse_manifest(dir.path());
    assert!(parsed.manifest.is_none());
    let error = parsed
        .diagnostics
        .iter()
        .find(|d| d.severity == PluginDiagnosticSeverity::Error)
        .expect("an error diagnostic should exist");
    assert_eq!(error.message, "\"name\" is required");
}

#[test]
fn should_derive_state_error_when_manifest_is_missing() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let parsed = parse_manifest(dir.path());
    assert_eq!(parsed.state(), PluginState::Error);
    assert!(parsed.has_errors());
}

#[test]
fn should_derive_state_ok_when_manifest_parses_cleanly() {
    let dir = make_manifest_dir(r#"{"name":"p"}"#);
    let parsed = parse_manifest(dir.path());
    assert_eq!(parsed.state(), PluginState::Ok);
    assert!(!parsed.has_errors());
}

#[test]
fn should_emit_warn_when_session_start_is_not_an_object() {
    let dir = make_manifest_dir(r#"{"name":"p","sessionStart":"onboarding.md"}"#);
    let diagnostics = diagnostics_of(&dir);
    assert_eq!(
        diagnostics
            .iter()
            .find(|d| d.message.contains("sessionStart"))
            .map(|d| d.severity),
        Some(PluginDiagnosticSeverity::Warn)
    );
}

#[test]
fn should_emit_warn_when_session_start_skill_is_blank() {
    let dir = make_manifest_dir(r#"{"name":"p","sessionStart":{"skill":"   "}}"#);
    let diagnostics = diagnostics_of(&dir);
    assert_eq!(
        diagnostics
            .iter()
            .find(|d| d.message.contains("sessionStart.skill"))
            .map(|d| d.severity),
        Some(PluginDiagnosticSeverity::Warn)
    );
}

#[test]
fn should_emit_info_diagnostics_for_unsupported_runtime_fields() {
    let dir = make_manifest_dir(
        r#"{"name":"p","tools":{},"configFile":"x","apps":[]}"#,
    );
    let diagnostics = diagnostics_of(&dir);
    let infos: Vec<&str> = diagnostics
        .iter()
        .filter(|d| d.severity == PluginDiagnosticSeverity::Info)
        .map(|d| d.message.as_str())
        .collect();
    assert_eq!(infos.len(), 3, "expected 3 info diagnostics, got: {infos:?}");
    assert!(infos.iter().any(|m| m.contains("\"tools\"")));
    assert!(infos.iter().any(|m| m.contains("\"configFile\"")));
    assert!(infos.iter().any(|m| m.contains("\"apps\"")));
}

#[test]
fn should_keep_state_ok_when_only_warn_and_info_diagnostics_exist() {
    let dir = make_manifest_dir(
        r#"{"name":"p","sessionStart":"x","bootstrap":"y"}"#,
    );
    let parsed = parse_manifest(dir.path());
    assert!(parsed.manifest.is_some());
    assert!(!parsed.has_errors());
    assert_eq!(parsed.state(), PluginState::Ok);
}

// ---------------------------------------------------------------------------
// B3 — name rules
// ---------------------------------------------------------------------------

#[test]
fn should_succeed_when_manifest_is_valid() {
    let manifest = blank_manifest("valid_name-1");
    assert!(validate_manifest(&manifest).is_ok());
}

#[test]
fn should_return_error_when_name_is_empty() {
    let manifest = blank_manifest("");
    assert!(validate_manifest(&manifest).is_err());
}

#[test]
fn should_return_error_when_name_contains_special_chars() {
    let manifest = blank_manifest("name with spaces");
    assert!(validate_manifest(&manifest).is_err());
}

#[test]
fn should_return_error_when_name_contains_dot() {
    let manifest = blank_manifest("name.with.dots");
    assert!(validate_manifest(&manifest).is_err());
}

#[test]
fn should_report_error_when_name_contains_uppercase_letters() {
    let dir = make_manifest_dir(r#"{"name":"MyPlugin"}"#);
    let parsed = parse_manifest(dir.path());
    assert!(parsed.manifest.is_none());
    assert!(parsed
        .diagnostics
        .iter()
        .any(|d| d.severity == PluginDiagnosticSeverity::Error));
}

#[test]
fn should_report_error_when_name_starts_with_a_separator() {
    let dir = make_manifest_dir(r#"{"name":"-leading-dash"}"#);
    assert!(parse_manifest(dir.path()).manifest.is_none());

    let dir = make_manifest_dir(r#"{"name":"_leading-underscore"}"#);
    assert!(parse_manifest(dir.path()).manifest.is_none());
}

#[test]
fn should_accept_name_when_it_is_exactly_64_characters() {
    let name = format!("a{}", "a".repeat(63));
    let manifest = blank_manifest(&name);
    assert!(validate_manifest(&manifest).is_ok());
}

#[test]
fn should_return_error_when_name_exceeds_64_characters() {
    let name = format!("a{}", "a".repeat(64));
    let manifest = blank_manifest(&name);
    assert!(validate_manifest(&manifest).is_err());
}

// ---------------------------------------------------------------------------
// B4 — sessionStart shape
// ---------------------------------------------------------------------------

#[test]
fn should_parse_session_start_when_it_is_an_object_with_skill() {
    let dir = make_manifest_dir(r#"{"name":"p","sessionStart":{"skill":"onboarding.md"}}"#);
    let manifest = parse_ok(&dir);
    assert_eq!(
        manifest.session_start,
        Some(PluginSessionStart {
            skill: "onboarding.md".to_string()
        })
    );
}

#[test]
fn should_trim_session_start_skill_when_it_has_leading_or_trailing_whitespace() {
    let dir = make_manifest_dir(r#"{"name":"p","sessionStart":{"skill":"  onboarding.md  "}}"#);
    let manifest = parse_ok(&dir);
    assert_eq!(
        manifest.session_start.as_ref().map(|s| s.skill.as_str()),
        Some("onboarding.md")
    );
}

#[test]
fn should_ignore_session_start_when_it_is_not_an_object() {
    let dir = make_manifest_dir(r#"{"name":"p","sessionStart":"onboarding.md"}"#);
    let manifest = parse_ok(&dir);
    assert!(manifest.session_start.is_none());
}

#[test]
fn should_ignore_session_start_when_skill_is_blank() {
    let dir = make_manifest_dir(r#"{"name":"p","sessionStart":{"skill":"   "}}"#);
    let manifest = parse_ok(&dir);
    assert!(manifest.session_start.is_none());
}

// ---------------------------------------------------------------------------
// B5 — interface shape
// ---------------------------------------------------------------------------

#[test]
fn should_parse_interface_when_it_is_an_object_with_all_fields() {
    let dir = make_manifest_dir(
        r#"{"name":"p","interface":{"displayName":"My Plugin","shortDescription":"Short","longDescription":"Long","developerName":"Dev","websiteURL":"https://example.com"}}"#,
    );
    let manifest = parse_ok(&dir);
    let interface = manifest.interface.expect("interface should be present");
    assert_eq!(interface.display_name.as_deref(), Some("My Plugin"));
    assert_eq!(interface.short_description.as_deref(), Some("Short"));
    assert_eq!(interface.long_description.as_deref(), Some("Long"));
    assert_eq!(interface.developer_name.as_deref(), Some("Dev"));
    assert_eq!(interface.website_url.as_deref(), Some("https://example.com"));
}

#[test]
fn should_parse_interface_when_only_some_fields_are_present() {
    let dir = make_manifest_dir(r#"{"name":"p","interface":{"displayName":"My Plugin"}}"#);
    let manifest = parse_ok(&dir);
    let interface = manifest.interface.expect("interface should be present");
    assert_eq!(interface.display_name.as_deref(), Some("My Plugin"));
    assert!(interface.short_description.is_none());
    assert!(interface.long_description.is_none());
    assert!(interface.developer_name.is_none());
    assert!(interface.website_url.is_none());
}

#[test]
fn should_ignore_interface_when_it_is_a_string() {
    let dir = make_manifest_dir(r#"{"name":"p","interface":"cli"}"#);
    let manifest = parse_ok(&dir);
    assert!(manifest.interface.is_none());
}

#[test]
fn should_ignore_interface_when_it_has_no_recognized_fields() {
    let dir = make_manifest_dir(r#"{"name":"p","interface":{"foo":"bar"}}"#);
    let manifest = parse_ok(&dir);
    assert!(manifest.interface.is_none());
}

#[test]
fn should_succeed_when_interface_is_an_object() {
    let manifest = PluginManifest {
        interface: Some(PluginInterface {
            display_name: Some("My Plugin".to_string()),
            ..PluginInterface::default()
        }),
        ..blank_manifest("test")
    };
    assert!(validate_manifest(&manifest).is_ok());
}

// ---------------------------------------------------------------------------
// B6 — systemPrompt semantics (rendered at parse time, B2 diagnostics)
// ---------------------------------------------------------------------------

#[test]
fn should_succeed_when_both_prompt_fields_are_set() {
    let manifest = blank_manifest("test");
    let manifest = PluginManifest {
        system_prompt: Some("inline".to_string()),
        system_prompt_path: Some("path.txt".to_string()),
        ..manifest
    };
    assert!(validate_manifest(&manifest).is_ok());
}

#[test]
fn should_keep_raw_prompt_path_when_both_inline_and_path_system_prompts_are_set() {
    let dir = make_manifest_dir(
        r#"{"name":"p","systemPrompt":"inline","systemPromptPath":"prompts/system.txt"}"#,
    );
    let manifest = parse_ok(&dir);
    assert_eq!(manifest.system_prompt.as_deref(), Some("inline"));
    assert_eq!(manifest.system_prompt_path.as_deref(), Some("prompts/system.txt"));
}

#[test]
fn should_accept_manifest_when_inline_system_prompt_exceeds_32kb() {
    // Over-limit is a render-time degrade (warn + ignored), not a parse
    // error (B6/B2).
    let oversized = "x".repeat(SYSTEM_PROMPT_MAX_BYTES + 1);
    let json = format!(r#"{{"name":"p","systemPrompt":"{oversized}"}}"#);
    let dir = make_manifest_dir(&json);
    let parsed = parse_manifest(dir.path());
    assert!(parsed.manifest.is_some());
    assert!(parsed.manifest.unwrap().system_prompt.is_none());
    assert!(parsed
        .diagnostics
        .iter()
        .any(|d| d.severity == PluginDiagnosticSeverity::Warn
            && d.message.contains("exceeding the 32 KB limit")));
}

#[test]
fn should_trim_inline_system_prompt_when_parsing() {
    let dir = make_manifest_dir(r#"{"name":"p","systemPrompt":"  hello  "}"#);
    let manifest = parse_ok(&dir);
    assert_eq!(manifest.system_prompt.as_deref(), Some("hello"));
}

#[test]
fn should_emit_warn_and_ignore_system_prompt_when_it_is_not_a_string() {
    let dir = make_manifest_dir(r#"{"name":"p","systemPrompt":42}"#);
    let manifest = parse_ok(&dir);
    assert!(manifest.system_prompt.is_none());
    assert!(diagnostics_of(&dir)
        .iter()
        .any(|d| d.severity == PluginDiagnosticSeverity::Warn
            && d.message == "\"systemPrompt\" must be a string"));
}

#[test]
fn should_emit_warn_and_ignore_system_prompt_path_when_it_is_not_a_string() {
    let dir = make_manifest_dir(r#"{"name":"p","systemPromptPath":["prompts/system.txt"]}"#);
    let manifest = parse_ok(&dir);
    assert!(manifest.system_prompt.is_none());
    assert!(diagnostics_of(&dir)
        .iter()
        .any(|d| d.severity == PluginDiagnosticSeverity::Warn
            && d.message == "\"systemPromptPath\" must be a string"));
}

#[test]
fn should_emit_warn_when_system_prompt_path_is_blank() {
    let dir = make_manifest_dir(r#"{"name":"p","systemPromptPath":"   "}"#);
    let manifest = parse_ok(&dir);
    assert!(manifest.system_prompt.is_none());
    assert!(diagnostics_of(&dir)
        .iter()
        .any(|d| d.message == "\"systemPromptPath\" must not be blank"));
}

#[test]
fn should_read_system_prompt_file_when_path_starts_with_dot_slash() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    std::fs::write(
        dir.path().join("mirri.plugin.json"),
        r#"{"name":"p","systemPromptPath":"./prompts/system.txt"}"#,
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("prompts")).unwrap();
    std::fs::write(
        dir.path().join("prompts").join("system.txt"),
        "File-based system prompt.",
    )
    .unwrap();

    let manifest = parse_ok(&dir);
    assert_eq!(manifest.system_prompt.as_deref(), Some("File-based system prompt."));
}

#[test]
fn should_emit_warn_and_ignore_file_when_system_prompt_path_has_no_dot_slash_prefix() {
    // The TS `resolvePluginPathField` requires the "./" prefix (B2/B6).
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    std::fs::write(
        dir.path().join("mirri.plugin.json"),
        r#"{"name":"p","systemPromptPath":"prompts/system.txt"}"#,
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("prompts")).unwrap();
    std::fs::write(
        dir.path().join("prompts").join("system.txt"),
        "File content.",
    )
    .unwrap();

    let manifest = parse_ok(&dir);
    assert!(manifest.system_prompt.is_none());
    assert!(diagnostics_of(&dir).iter().any(|d| {
        d.severity == PluginDiagnosticSeverity::Warn
            && d.message.contains("\"systemPromptPath\" path must start with \"./\"")
    }));
}

#[test]
fn should_emit_warn_when_system_prompt_file_is_missing() {
    let dir = make_manifest_dir(r#"{"name":"p","systemPromptPath":"./missing.txt"}"#);
    let manifest = parse_ok(&dir);
    assert!(manifest.system_prompt.is_none());
    // The warning names the field; the exact wording depends on whether the
    // unresolved path still lexically lands inside the plugin root
    // ("is not a file") or falls outside it ("resolves outside the plugin") —
    // the TS `resolvePluginPathField`/`stat` behavior.
    assert!(
        diagnostics_of(&dir)
            .iter()
            .any(|d| d.severity == PluginDiagnosticSeverity::Warn
                && d.message.contains("\"systemPromptPath\"")),
        "expected a systemPromptPath warn diagnostic"
    );
}

// ---------------------------------------------------------------------------
// B7 — mcpServers validation
// ---------------------------------------------------------------------------

#[test]
fn should_emit_warn_when_mcp_servers_is_not_an_object() {
    let dir = make_manifest_dir(r#"{"name":"p","mcpServers":[{"command":"node"}]}"#);
    let manifest = parse_ok(&dir);
    assert!(manifest.mcp_servers.is_none());
    assert!(diagnostics_of(&dir)
        .iter()
        .any(|d| d.message == "\"mcpServers\" must be an object"));
}

#[test]
fn should_emit_warn_when_mcp_server_name_is_blank() {
    let dir = make_manifest_dir(r#"{"name":"p","mcpServers":{"  ":{"command":"node"}}}"#);
    let manifest = parse_ok(&dir);
    assert!(manifest.mcp_servers.is_none());
    assert!(diagnostics_of(&dir)
        .iter()
        .any(|d| d.message == "\"mcpServers\" entries must have a non-empty name"));
}

#[test]
fn should_drop_invalid_mcp_server_and_keep_valid_ones() {
    let dir = make_manifest_dir(
        r#"{"name":"p","mcpServers":{"good":{"command":"node"},"bad":{"command":""},"worse":42}}"#,
    );
    let manifest = parse_ok(&dir);
    let servers = manifest.mcp_servers.expect("good server should survive");
    assert_eq!(servers.len(), 1);
    assert!(servers.contains_key("good"));
    let warns = diagnostics_of(&dir);
    assert!(warns.iter().any(|d| d.message.contains("Invalid MCP server \"bad\"")));
    assert!(warns.iter().any(|d| d.message.contains("Invalid MCP server \"worse\"")));
}

#[test]
fn should_resolve_dot_slash_command_to_absolute_path_inside_plugin() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    std::fs::write(
        dir.path().join("mirri.plugin.json"),
        r#"{"name":"p","mcpServers":{"local":{"command":"./server.js"}}}"#,
    )
    .unwrap();
    std::fs::write(dir.path().join("server.js"), "console.log(1)").unwrap();

    let manifest = parse_ok(&dir);
    let servers = manifest.mcp_servers.expect("server should survive");
    let mirri_plugin_host::manifest::McpServerConfig::Stdio(stdio) =
        servers.get("local").expect("local server") else {
        panic!("expected stdio transport");
    };
    // `./server.js` resolves to the plugin-relative absolute path
    // (realpath'd — compare canonically so `/tmp` symlinks don't matter).
    let expected = std::fs::canonicalize(dir.path().join("server.js")).expect("canonicalize");
    assert_eq!(
        std::path::Path::new(&stdio.command)
            .canonicalize()
            .expect("canonicalize"),
        expected
    );
}

#[test]
fn should_drop_mcp_server_when_command_has_a_slash_without_dot_prefix() {
    let dir = make_manifest_dir(r#"{"name":"p","mcpServers":{"x":{"command":"bin/server"}}}"#);
    let manifest = parse_ok(&dir);
    assert!(manifest.mcp_servers.is_none());
    assert!(diagnostics_of(&dir).iter().any(|d| {
        d.message == "\"mcpServers.x.command\" must be a PATH command or start with \"./\""
    }));
}

#[test]
fn should_reject_absolute_command_when_it_points_outside_the_plugin() {
    let dir = make_manifest_dir(r#"{"name":"p","mcpServers":{"x":{"command":"/usr/bin/node"}}}"#);
    let manifest = parse_ok(&dir);
    assert!(manifest.mcp_servers.is_none());
    assert!(diagnostics_of(&dir)
        .iter()
        .any(|d| d.message.contains("must be a PATH command or start with \"./\"")));
}

#[test]
fn should_resolve_cwd_when_it_starts_with_dot_slash() {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    std::fs::write(
        dir.path().join("mirri.plugin.json"),
        r#"{"name":"p","mcpServers":{"x":{"command":"node","cwd":"./dist"}}}"#,
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("dist")).unwrap();

    let manifest = parse_ok(&dir);
    let servers = manifest.mcp_servers.expect("server should survive");
    let mirri_plugin_host::manifest::McpServerConfig::Stdio(stdio) =
        servers.get("x").expect("server") else {
        panic!("expected stdio transport");
    };
    assert!(stdio.cwd.as_deref().unwrap().ends_with("dist"));
}

#[test]
fn should_drop_mcp_server_when_cwd_does_not_start_with_dot_slash() {
    let dir = make_manifest_dir(r#"{"name":"p","mcpServers":{"x":{"command":"node","cwd":"dist"}}}"#);
    let manifest = parse_ok(&dir);
    assert!(manifest.mcp_servers.is_none());
    assert!(diagnostics_of(&dir)
        .iter()
        .any(|d| d.message.contains("\"mcpServers.x.cwd\" path must start with \"./\"")));
}

#[test]
fn should_keep_http_server_untouched_when_url_is_valid() {
    let dir = make_manifest_dir(
        r#"{"name":"p","mcpServers":{"remote":{"transport":"http","url":"https://mcp.test/stream","headers":{"X-A":"b"}}}}"#,
    );
    let manifest = parse_ok(&dir);
    let servers = manifest.mcp_servers.expect("server should survive");
    let mirri_plugin_host::manifest::McpServerConfig::Http(remote) =
        servers.get("remote").expect("server") else {
        panic!("expected http transport");
    };
    assert_eq!(remote.url, "https://mcp.test/stream");
    assert_eq!(
        remote.headers.as_ref().unwrap().get("X-A").map(String::as_str),
        Some("b")
    );
}

#[test]
fn should_drop_remote_server_when_url_is_invalid() {
    let dir = make_manifest_dir(
        r#"{"name":"p","mcpServers":{"remote":{"transport":"sse","url":"not-a-url"}}}"#,
    );
    let manifest = parse_ok(&dir);
    assert!(manifest.mcp_servers.is_none());
    assert!(diagnostics_of(&dir).iter().any(|d| {
        d.message.contains("Invalid MCP server \"remote\"") && d.message.contains("\"url\"")
    }));
}

#[test]
fn should_drop_mcp_server_when_startup_timeout_exceeds_the_ts_cap() {
    let dir = make_manifest_dir(
        r#"{"name":"p","mcpServers":{"x":{"command":"node","startupTimeoutMs":2147483648}}}"#,
    );
    let manifest = parse_ok(&dir);
    assert!(manifest.mcp_servers.is_none());
}

#[test]
fn should_accept_common_fields_when_they_are_valid() {
    let dir = make_manifest_dir(
        r#"{"name":"p","mcpServers":{"x":{"command":"node","enabled":true,"enabledTools":["t"],"auth":"oauth","executor":"local","env":{"K":"v"}}}}"#,
    );
    let manifest = parse_ok(&dir);
    let servers = manifest.mcp_servers.expect("server should survive");
    let mirri_plugin_host::manifest::McpServerConfig::Stdio(stdio) =
        servers.get("x").expect("server") else {
        panic!("expected stdio transport");
    };
    assert_eq!(stdio.common.enabled, Some(true));
    assert_eq!(
        stdio.common.enabled_tools.as_deref(),
        Some(&["t".to_string()][..])
    );
    assert_eq!(stdio.executor, Some(mirri_plugin_host::manifest::McpServerExecutor::Local));
}

// ---------------------------------------------------------------------------
// B10 — metadata fields
// ---------------------------------------------------------------------------

#[test]
fn should_preserve_metadata_fields_when_present() {
    let dir = make_manifest_dir(
        r#"{"name":"p","keywords":["kimi","mcp"],"author":{"name":"Acme","email":"dev@acme.dev"},"homepage":"https://acme.dev","license":"MIT","skillInstructions":"Run the onboarding skill."}"#,
    );
    let manifest = parse_ok(&dir);
    assert_eq!(
        manifest.keywords,
        Some(vec!["kimi".to_string(), "mcp".to_string()])
    );
    assert_eq!(
        manifest.author,
        Some(PluginAuthor {
            name: Some("Acme".to_string()),
            email: Some("dev@acme.dev".to_string())
        })
    );
    assert_eq!(manifest.homepage.as_deref(), Some("https://acme.dev"));
    assert_eq!(manifest.license.as_deref(), Some("MIT"));
    assert_eq!(
        manifest.skill_instructions.as_deref(),
        Some("Run the onboarding skill.")
    );
}

#[test]
fn should_create_author_from_string_when_author_is_a_string() {
    let dir = make_manifest_dir(r#"{"name":"p","author":"Acme"}"#);
    let manifest = parse_ok(&dir);
    assert_eq!(
        manifest.author,
        Some(PluginAuthor {
            name: Some("Acme".to_string()),
            email: None
        })
    );
}

#[test]
fn should_ignore_metadata_fields_when_they_have_wrong_shapes() {
    let dir = make_manifest_dir(
        r#"{"name":"p","keywords":"kimi","author":42,"homepage":["https://acme.dev"],"license":{"text":"MIT"},"skillInstructions":7}"#,
    );
    let manifest = parse_ok(&dir);
    assert!(manifest.keywords.is_none());
    assert!(manifest.author.is_none());
    assert!(manifest.homepage.is_none());
    assert!(manifest.license.is_none());
    assert!(manifest.skill_instructions.is_none());
}

#[test]
fn should_trim_metadata_string_fields_when_parsing() {
    let dir = make_manifest_dir(
        r#"{"name":"p","homepage":"  https://acme.dev  ","license":" MIT ","skillInstructions":"  run x  "}"#,
    );
    let manifest = parse_ok(&dir);
    assert_eq!(manifest.homepage.as_deref(), Some("https://acme.dev"));
    assert_eq!(manifest.license.as_deref(), Some("MIT"));
    assert_eq!(manifest.skill_instructions.as_deref(), Some("run x"));
}

#[test]
fn should_preserve_keywords_verbatim_when_entries_have_whitespace() {
    let dir = make_manifest_dir(r#"{"name":"p","keywords":[" kimi ","mcp"]}"#);
    let manifest = parse_ok(&dir);
    assert_eq!(
        manifest.keywords,
        Some(vec![" kimi ".to_string(), "mcp".to_string()])
    );
}

// ---------------------------------------------------------------------------
// Path safety
// ---------------------------------------------------------------------------

#[test]
fn should_accept_path_when_relative_to_root() {
    let root = std::path::Path::new("/plugins/test");
    assert!(validate_path_in_root(std::path::Path::new("skills/my-skill"), root).is_ok());
}

#[test]
fn should_reject_path_when_directory_traversal_attempted() {
    let root = std::path::Path::new("/plugins/test");
    assert!(validate_path_in_root(std::path::Path::new("../../../etc"), root).is_err());
}

#[test]
fn should_reject_path_when_absolute_and_outside_root() {
    let root = std::path::Path::new("/plugins/test");
    assert!(validate_path_in_root(std::path::Path::new("/etc/passwd"), root).is_err());
}

#[test]
fn should_accept_path_when_absolute_and_inside_root() {
    let root = std::path::Path::new("/plugins/test");
    assert!(validate_path_in_root(std::path::Path::new("/plugins/test/skills"), root).is_ok());
}

// ---------------------------------------------------------------------------
// B8 — skills/agents entries (TS `resolveDirListField`)
// ---------------------------------------------------------------------------

/// Write a manifest + create the given relative dirs; returns the temp dir.
fn manifest_with_dirs(json: &str, dirs: &[&str]) -> tempfile::TempDir {
    let dir = make_manifest_dir(json);
    for relative in dirs {
        std::fs::create_dir_all(dir.path().join(relative)).unwrap();
    }
    dir
}

#[test]
fn should_resolve_skill_and_agent_entries_when_paths_start_with_dot_slash() {
    let dir = manifest_with_dirs(
        r#"{"name":"p","skills":["./skills"],"agents":["./agents"]}"#,
        &["skills", "agents"],
    );
    let manifest = parse_ok(&dir);

    // The values are the resolved absolute realpath'd directories (the TS
    // `resolveDirListField` stores real paths in `manifest.skills/agents`).
    let skills = manifest.skills.expect("skills should resolve");
    assert_eq!(skills.len(), 1);
    assert_eq!(std::path::Path::new(&skills[0]).canonicalize().unwrap(), dir.path().join("skills").canonicalize().unwrap());
    let agents = manifest.agents.expect("agents should resolve");
    assert_eq!(agents.len(), 1);
    assert!(agents[0].ends_with("agents"));

    assert!(diagnostics_of(&dir).is_empty(), "no diagnostics for valid entries");
}

#[test]
fn should_emit_error_diagnostic_when_skill_entry_lacks_dot_slash_prefix() {
    let dir = make_manifest_dir(r#"{"name":"p","skills":["skills"]}"#);
    let parsed = parse_manifest(dir.path());
    // An error diagnostic drops the entry; the manifest itself survives.
    assert!(parsed.manifest.is_some());
    assert!(parsed.manifest.as_ref().unwrap().skills.is_none());
    assert!(parsed.diagnostics.iter().any(|d| {
        d.severity == PluginDiagnosticSeverity::Error
            && d.message == "\"skills\" path must start with \"./\" (got \"skills\")"
    }));
    assert_eq!(parsed.state(), PluginState::Error, "error diagnostics flip state");
}

#[test]
fn should_emit_error_diagnostic_when_skill_field_has_wrong_shape() {
    let dir = make_manifest_dir(r#"{"name":"p","skills":{"a":1}}"#);
    let parsed = parse_manifest(dir.path());
    assert!(parsed.manifest.is_some());
    assert!(parsed.manifest.unwrap().skills.is_none());
    assert!(parsed.diagnostics.iter().any(|d| {
        d.severity == PluginDiagnosticSeverity::Error
            && d.message == "\"skills\" must be a string or string[]"
    }));
}

#[test]
fn should_emit_error_diagnostic_when_entry_resolves_outside_the_plugin() {
    // A `./`-prefixed entry that escapes the root (e.g. `./../sibling`)
    // resolves outside and is dropped with an error (the TS realpath +
    // `isWithin` containment check).
    let base = tempfile::tempdir().expect("tempdir");
    let root = base.path().join("plugin");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::create_dir_all(base.path().join("sibling")).unwrap();
    std::fs::write(
        root.join("mirri.plugin.json"),
        r#"{"name":"p","agents":["./../sibling"]}"#,
    )
    .unwrap();

    let parsed = parse_manifest(&root);
    assert!(parsed.manifest.is_some());
    assert!(parsed.manifest.unwrap().agents.is_none());
    assert!(parsed.diagnostics.iter().any(|d| {
        d.severity == PluginDiagnosticSeverity::Error
            && d.message.contains("resolves outside the plugin")
    }));
}

#[test]
fn should_emit_warn_diagnostic_when_skill_entry_is_not_a_directory() {
    let dir = make_manifest_dir(r#"{"name":"p","skills":["./missing"]}"#);
    let parsed = parse_manifest(dir.path());
    assert!(parsed.manifest.is_some());
    assert!(parsed.manifest.as_ref().unwrap().skills.is_none());
    assert!(parsed.diagnostics.iter().any(|d| {
        d.severity == PluginDiagnosticSeverity::Warn
            && d.message == "\"skills\" path is not a directory (./missing)"
    }));
    assert_eq!(parsed.state(), PluginState::Ok, "warn diagnostics do not flip state");
}

#[test]
fn should_accept_a_string_skill_field_when_it_starts_with_dot_slash() {
    let dir = manifest_with_dirs(r#"{"name":"p","skills":"./skills"}"#, &["skills"]);
    let manifest = parse_ok(&dir);
    assert_eq!(manifest.skills.as_ref().unwrap().len(), 1);
}

#[test]
fn should_infer_root_skill_when_fields_absent_and_skill_md_exists() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("mirri.plugin.json"), r#"{"name":"p"}"#).unwrap();
    std::fs::write(dir.path().join("SKILL.md"), "# Root skill").unwrap();

    let manifest = parse_ok(&dir);
    // The inference maps to the canonical plugin root (the TS
    // `skills = [pluginRoot]`) and produces no diagnostics.
    let skills = manifest.skills.expect("root skill inferred");
    assert_eq!(skills.len(), 1);
    assert_eq!(
        std::path::Path::new(&skills[0]),
        std::fs::canonicalize(dir.path()).unwrap()
    );
    assert!(diagnostics_of(&dir).is_empty(), "inference is silent");
}

#[test]
fn should_not_infer_root_skill_when_skill_md_is_missing() {
    let dir = make_manifest_dir(r#"{"name":"p"}"#);
    let manifest = parse_ok(&dir);
    assert!(manifest.skills.is_none());
    assert!(diagnostics_of(&dir).is_empty());
}

#[test]
fn should_infer_agents_dir_when_field_absent_and_agents_directory_exists() {
    let dir = manifest_with_dirs(r#"{"name":"p"}"#, &["agents"]);
    let manifest = parse_ok(&dir);
    let agents = manifest.agents.expect("agents dir inferred");
    assert_eq!(agents.len(), 1);
    assert!(agents[0].ends_with("agents"));
    assert!(diagnostics_of(&dir).is_empty(), "inference is silent");
}

#[test]
fn should_not_infer_agents_dir_when_agents_directory_is_missing() {
    let dir = make_manifest_dir(r#"{"name":"p"}"#);
    let manifest = parse_ok(&dir);
    assert!(manifest.agents.is_none());
}

#[test]
fn should_keep_inference_after_an_empty_explicit_field() {
    // An explicit (empty) array wins: no inference even when the dirs exist.
    let dir = manifest_with_dirs(r#"{"name":"p","skills":[]}"#, &["skills"]);
    let manifest = parse_ok(&dir);
    assert!(manifest.skills.is_none(), "empty explicit field disables inference");
}

// Symlink creation needs no privileges on unix; skipped on Windows.
#[cfg(unix)]
#[test]
fn should_resolve_skill_entry_following_a_symlink_outside_the_plugin() {
    // The TS realpath + `isWithin` check: an entry whose path is a symlink
    // escaping the plugin root resolves outside and is dropped with an
    // error diagnostic.
    let base = tempfile::tempdir().expect("tempdir");
    let root = base.path().join("plugin");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(base.path().join("outside")).unwrap();
    std::fs::write(
        root.join("mirri.plugin.json"),
        r#"{"name":"p","skills":["./escape"]}"#,
    )
    .unwrap();
    std::os::unix::fs::symlink(base.path().join("outside"), root.join("escape")).unwrap();

    let parsed = parse_manifest(&root);
    assert!(parsed.manifest.is_some());
    assert!(parsed.manifest.unwrap().skills.is_none());
    assert!(parsed.diagnostics.iter().any(|d| {
        d.severity == PluginDiagnosticSeverity::Error
            && d.message.contains("resolves outside the plugin")
    }));
}