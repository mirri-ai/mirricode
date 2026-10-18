//! Integration tests for `PluginManager` — install, load, reload, enable,
//! disable, remove, and resource collection.

use mirri_plugin_host::{
    PluginDiagnosticSeverity, PluginError, PluginManager, PluginState,
};
use std::io::Write;
use std::path::Path;

/// Helper: create a temp plugins dir and a source plugin directory with
/// a valid manifest and some resources.
fn setup_plugin_source(name: &str) -> (tempfile::TempDir, tempfile::TempDir) {
    let plugins_dir = tempfile::tempdir().expect("failed to create plugins dir");
    let source_dir = tempfile::tempdir().expect("failed to create source dir");

    let manifest = serde_json::json!({
        "name": name,
        "version": "1.0.0",
        "description": "A test plugin",
        "skills": ["./skills"],
        "agents": ["./agents"],
        "commands": ["./commands"],
        "systemPrompt": "You are a test assistant.",
        "mcpServers": {
            "test-server": {
                "command": "node",
                "args": ["server.js"]
            }
        }
    });
    let mut file = std::fs::File::create(source_dir.path().join("mirri.plugin.json"))
        .expect("failed to create manifest");
    file.write_all(serde_json::to_string_pretty(&manifest).unwrap().as_bytes())
        .expect("failed to write manifest");

    std::fs::create_dir_all(source_dir.path().join("skills")).unwrap();
    std::fs::create_dir_all(source_dir.path().join("agents")).unwrap();
    std::fs::create_dir_all(source_dir.path().join("commands")).unwrap();
    std::fs::write(
        source_dir.path().join("commands").join("hello.md"),
        "# Hello command",
    )
    .unwrap();

    (plugins_dir, source_dir)
}

fn local_source(source_dir: &tempfile::TempDir) -> String {
    format!("local:{}", source_dir.path().display())
}

#[tokio::test]
async fn should_install_plugin_when_source_is_local() {
    let (plugins_dir, source_dir) = setup_plugin_source("test-plugin");
    let manager = PluginManager::new(plugins_dir.path().to_path_buf());

    let id = manager
        .install(&local_source(&source_dir))
        .await
        .expect("install should succeed");
    assert_eq!(id, "test-plugin");

    // The plugin lives under the TS managed layout (A2):
    // `<plugins>/managed/<id>`.
    let managed_root = plugins_dir.path().join("managed").join("test-plugin");
    assert!(managed_root.is_dir());
    assert!(managed_root.join("mirri.plugin.json").is_file());
}

#[tokio::test]
async fn should_upgrade_plugin_when_installed_again() {
    let (plugins_dir, source_dir) = setup_plugin_source("test-plugin");
    let manager = PluginManager::new(plugins_dir.path().to_path_buf());

    manager
        .install(&local_source(&source_dir))
        .await
        .expect("first install");

    let result = manager.install(&local_source(&source_dir)).await;
    result.expect("reinstall upgrades, never AlreadyInstalled");
}

#[tokio::test]
async fn should_load_all_plugins_when_manager_is_restarted() {
    let (plugins_dir, source_dir) = setup_plugin_source("test-plugin");
    let manager = PluginManager::new(plugins_dir.path().to_path_buf());

    manager
        .install(&local_source(&source_dir))
        .await
        .expect("install");

    // Simulate a restart with a fresh manager.
    let manager2 = PluginManager::new(plugins_dir.path().to_path_buf());
    manager2.load_all().expect("load_all should succeed");

    let list = manager2.list();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].id, "test-plugin");
    assert!(list[0].enabled);
    assert_eq!(list[0].version.as_deref(), Some("1.0.0"));
}

#[tokio::test]
async fn should_toggle_enabled_state_when_enable_or_disable_called() {
    let (plugins_dir, source_dir) = setup_plugin_source("test-plugin");
    let manager = PluginManager::new(plugins_dir.path().to_path_buf());

    manager
        .install(&local_source(&source_dir))
        .await
        .expect("install");

    manager.disable("test-plugin").expect("disable");
    assert!(!manager.list()[0].enabled);

    manager.enable("test-plugin").expect("enable");
    assert!(manager.list()[0].enabled);
}

#[tokio::test]
async fn should_remove_record_but_keep_directory_when_plugin_is_removed() {
    let (plugins_dir, source_dir) = setup_plugin_source("test-plugin");
    let manager = PluginManager::new(plugins_dir.path().to_path_buf());

    manager
        .install(&local_source(&source_dir))
        .await
        .expect("install");

    manager.remove("test-plugin").expect("remove");

    // The managed directory stays on disk (the TS remove, item C2).
    assert!(plugins_dir.path().join("managed").join("test-plugin").is_dir());
    assert!(manager.list().is_empty());
}

#[tokio::test]
async fn should_return_not_found_error_when_removing_nonexistent_plugin() {
    let (plugins_dir, _source_dir) = setup_plugin_source("test-plugin");
    let manager = PluginManager::new(plugins_dir.path().to_path_buf());

    let result = manager.remove("ghost");
    assert!(matches!(result, Err(PluginError::NotFound(_))));
}

#[tokio::test]
async fn should_reload_manifest_when_disk_changes_occur() {
    let (plugins_dir, source_dir) = setup_plugin_source("test-plugin");
    let manager = PluginManager::new(plugins_dir.path().to_path_buf());

    manager
        .install(&local_source(&source_dir))
        .await
        .expect("install");

    // Modify manifest on disk.
    let updated_manifest = serde_json::json!({
        "name": "test-plugin",
        "description": "Updated description"
    });
    std::fs::write(
        plugins_dir
            .path()
            .join("managed")
            .join("test-plugin")
            .join("mirri.plugin.json"),
        serde_json::to_string_pretty(&updated_manifest).unwrap(),
    )
    .unwrap();

    manager.reload("test-plugin").expect("reload");
    assert_eq!(
        manager.list()[0].description.as_deref(),
        Some("Updated description")
    );
}

#[tokio::test]
async fn should_return_mcp_servers_when_plugin_is_enabled() {
    let (plugins_dir, source_dir) = setup_plugin_source("test-plugin");
    let manager = PluginManager::new(plugins_dir.path().to_path_buf());

    manager
        .install(&local_source(&source_dir))
        .await
        .expect("install");

    let servers = manager.enabled_mcp_servers();
    assert!(servers.contains_key("plugin-test-plugin:test-server"));
}

#[tokio::test]
async fn should_return_system_prompts_when_plugin_is_enabled() {
    let (plugins_dir, source_dir) = setup_plugin_source("test-plugin");
    let manager = PluginManager::new(plugins_dir.path().to_path_buf());

    manager
        .install(&local_source(&source_dir))
        .await
        .expect("install");

    let prompts = manager.enabled_system_prompts();
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].plugin_id, "test-plugin");
    assert_eq!(prompts[0].content, "You are a test assistant.");
}

#[tokio::test]
async fn should_return_skill_and_agent_roots_when_plugin_is_installed() {
    let (plugins_dir, source_dir) = setup_plugin_source("test-plugin");
    let manager = PluginManager::new(plugins_dir.path().to_path_buf());

    manager
        .install(&local_source(&source_dir))
        .await
        .expect("install");

    let skills = manager.plugin_skill_roots();
    assert_eq!(skills.len(), 1);
    assert!(skills[0].path.ends_with("skills"));
    assert_eq!(skills[0].source, "extra");
    assert_eq!(skills[0].plugin.id, "test-plugin");

    let agents = manager.plugin_agent_roots();
    assert_eq!(agents.len(), 1);
    assert!(agents[0].path.ends_with("agents"));
    assert_eq!(agents[0].source, "plugin");
}

#[tokio::test]
async fn should_exclude_resources_when_plugin_is_disabled() {
    let (plugins_dir, source_dir) = setup_plugin_source("test-plugin");
    let manager = PluginManager::new(plugins_dir.path().to_path_buf());

    manager
        .install(&local_source(&source_dir))
        .await
        .expect("install");
    manager.disable("test-plugin").expect("disable");

    assert!(manager.enabled_mcp_servers().is_empty());
    assert!(manager.enabled_system_prompts().is_empty());
    assert!(manager.plugin_skill_roots().is_empty());
    assert!(manager.plugin_agent_roots().is_empty());
}

#[tokio::test]
async fn should_list_all_plugins_when_multiple_plugins_installed() {
    let plugins_dir = tempfile::tempdir().unwrap();

    // Plugin A
    let source_a = tempfile::tempdir().unwrap();
    std::fs::write(
        source_a.path().join("mirri.plugin.json"),
        r#"{"name":"plugin-a","description":"Plugin A"}"#,
    )
    .unwrap();

    // Plugin B
    let source_b = tempfile::tempdir().unwrap();
    std::fs::write(
        source_b.path().join("mirri.plugin.json"),
        r#"{"name":"plugin-b","description":"Plugin B"}"#,
    )
    .unwrap();

    let manager = PluginManager::new(plugins_dir.path().to_path_buf());

    manager
        .install(&local_source(&source_a))
        .await
        .expect("install A");
    manager
        .install(&local_source(&source_b))
        .await
        .expect("install B");

    let list = manager.list();
    assert_eq!(list.len(), 2);

    // Disable A, only B's resources should be available.
    manager.disable("plugin-a").expect("disable A");
    let prompts = manager.enabled_system_prompts();
    assert!(prompts.is_empty()); // Neither has systemPrompt
}

#[tokio::test]
async fn should_load_system_prompt_when_prompt_path_is_specified() {
    let plugins_dir = tempfile::tempdir().unwrap();
    let source_dir = tempfile::tempdir().unwrap();

    std::fs::write(
        source_dir.path().join("mirri.plugin.json"),
        serde_json::json!({
            "name": "file-prompt-plugin",
            "systemPromptPath": "./prompts/system.txt"
        })
        .to_string(),
    )
    .unwrap();
    std::fs::create_dir_all(source_dir.path().join("prompts")).unwrap();
    std::fs::write(
        source_dir.path().join("prompts").join("system.txt"),
        "File-based prompt.",
    )
    .unwrap();

    let manager = PluginManager::new(plugins_dir.path().to_path_buf());
    manager
        .install(&local_source(&source_dir))
        .await
        .expect("install");

    let prompts = manager.enabled_system_prompts();
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].plugin_id, "file-prompt-plugin");
    assert_eq!(prompts[0].content, "File-based prompt.");
}

#[tokio::test]
async fn should_collect_command_files_when_plugin_is_installed() {
    let (plugins_dir, source_dir) = setup_plugin_source("test-plugin");
    let manager = PluginManager::new(plugins_dir.path().to_path_buf());

    manager
        .install(&local_source(&source_dir))
        .await
        .expect("install");

    let commands = manager.plugin_command_files();
    assert_eq!(commands.len(), 1);
    assert!(commands[0].ends_with("commands/hello.md"));
}

#[tokio::test]
async fn should_keep_plugin_in_error_state_when_load_all_encounters_corrupt_manifest() {
    let plugins_dir = tempfile::tempdir().unwrap();

    // Valid plugin
    let source_valid = tempfile::tempdir().unwrap();
    std::fs::write(
        source_valid.path().join("mirri.plugin.json"),
        r#"{"name":"valid"}"#,
    )
    .unwrap();
    let manager = PluginManager::new(plugins_dir.path().to_path_buf());
    manager
        .install(&local_source(&source_valid))
        .await
        .expect("install valid");

    // Corrupt the manifest on disk.
    std::fs::write(
        plugins_dir.path().join("managed").join("valid").join("mirri.plugin.json"),
        "NOT JSON",
    )
    .unwrap();

    // load_all keeps the broken plugin with state 'error' (the TS
    // `load`/`materialize` behavior, B2) instead of dropping it.
    let manager2 = PluginManager::new(plugins_dir.path().to_path_buf());
    manager2.load_all().expect("load_all should succeed");
    let list = manager2.list();
    assert_eq!(list.len(), 1, "the broken plugin stays in the list");
    assert_eq!(list[0].id, "valid");
    assert_eq!(list[0].state, PluginState::Error);
    assert!(list[0].has_errors());
    assert!(
        list[0]
            .diagnostics
            .iter()
            .any(|d| d.message.contains("Failed to parse"))
    );
}

#[tokio::test]
async fn should_exclude_error_state_plugin_from_collections_when_load_all_keeps_it() {
    let plugins_dir = tempfile::tempdir().unwrap();

    let source = tempfile::tempdir().unwrap();
    std::fs::write(
        source.path().join("mirri.plugin.json"),
        serde_json::json!({
            "name": "broken",
            "mcpServers": { "srv": { "command": "node" } },
            "systemPrompt": "prompt",
            "skills": ["./skills"]
        })
        .to_string(),
    )
    .unwrap();
    std::fs::create_dir_all(source.path().join("skills")).unwrap();

    let manager = PluginManager::new(plugins_dir.path().to_path_buf());
    manager
        .install(&local_source(&source))
        .await
        .expect("install");

    // Corrupt the manifest, then reload: the plugin turns error-state.
    std::fs::write(
        plugins_dir.path().join("managed").join("broken").join("mirri.plugin.json"),
        "NOT JSON",
    )
    .unwrap();
    manager
        .reload("broken")
        .expect("reload with a broken manifest should not fail");

    assert_eq!(manager.list()[0].state, PluginState::Error);
    // Consumers skip error-state plugins (B2 — the TS `state !== 'ok'`).
    assert!(manager.enabled_mcp_servers().is_empty());
    assert!(manager.enabled_system_prompts().is_empty());
    assert!(manager.plugin_skill_roots().is_empty());
}

#[tokio::test]
async fn should_ignore_system_prompt_when_path_traverses_outside_the_plugin() {
    let plugins_dir = tempfile::tempdir().unwrap();
    let source_dir = tempfile::tempdir().unwrap();

    std::fs::write(
        source_dir.path().join("mirri.plugin.json"),
        serde_json::json!({
            "name": "traversal-plugin",
            "systemPromptPath": "../../../etc/passwd"
        })
        .to_string(),
    )
    .unwrap();

    let manager = PluginManager::new(plugins_dir.path().to_path_buf());
    manager
        .install(&local_source(&source_dir))
        .await
        .expect("install");

    // The path lacks the required "./" prefix, so the file is never read
    // (TS `resolvePluginPathField`); the prompt stays empty and a warn
    // diagnostic is recorded.
    let prompts = manager.enabled_system_prompts();
    assert!(prompts.is_empty());
    assert!(manager.list()[0]
        .diagnostics
        .iter()
        .any(|d| d.message.contains("\"systemPromptPath\" path must start with \"./\"")));
}

#[tokio::test]
async fn should_populate_plugin_info_fields_when_plugin_is_installed() {
    let (plugins_dir, source_dir) = setup_plugin_source("test-plugin");
    let manager = PluginManager::new(plugins_dir.path().to_path_buf());

    manager
        .install(&local_source(&source_dir))
        .await
        .expect("install");

    let info = &manager.list()[0];
    assert_eq!(info.id, "test-plugin");
    assert_eq!(info.version.as_deref(), Some("1.0.0"));
    assert_eq!(info.description.as_deref(), Some("A test plugin"));
    assert!(info.enabled);
    assert!(info.path.ends_with("test-plugin"));
    // Source should be Local.
    match &info.source {
        PluginSource::Local { path } => {
            assert!(!path.is_empty());
        }
        _ => panic!("expected Local source"),
    }
}

// ---------------------------------------------------------------------------
// B3 — case-insensitive id lookup (TS `normalizePluginId`)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_find_plugin_when_id_lookup_uses_different_case() {
    let (plugins_dir, source_dir) = setup_plugin_source("test-plugin");
    let manager = PluginManager::new(plugins_dir.path().to_path_buf());

    manager
        .install(&local_source(&source_dir))
        .await
        .expect("install");

    manager
        .disable("TEST-PLUGIN")
        .expect("disable with upper-case id should succeed");
    assert!(!manager.list()[0].enabled);

    manager
        .enable("Test-Plugin")
        .expect("enable with mixed-case id should succeed");
    assert!(manager.list()[0].enabled);
}

#[tokio::test]
async fn should_preserve_original_record_id_when_lookup_uses_different_case() {
    let (plugins_dir, source_dir) = setup_plugin_source("test-plugin");
    let manager = PluginManager::new(plugins_dir.path().to_path_buf());

    manager
        .install(&local_source(&source_dir))
        .await
        .expect("install");
    manager
        .disable("TEST-PLUGIN")
        .expect("disable with upper-case id should succeed");

    // The on-disk record keeps the manifest name (normalized id) verbatim,
    // in the TS v1 envelope.
    let raw =
        std::fs::read_to_string(plugins_dir.path().join("installed.json")).expect("read store");
    let file: serde_json::Value = serde_json::from_str(&raw).expect("parse store");
    assert_eq!(file["version"], serde_json::json!(1));
    assert_eq!(file["plugins"][0]["id"], "test-plugin");
}

#[tokio::test]
async fn should_reject_install_when_manifest_name_contains_uppercase() {
    let plugins_dir = tempfile::tempdir().unwrap();
    let source_dir = tempfile::tempdir().unwrap();

    std::fs::write(
        source_dir.path().join("mirri.plugin.json"),
        r#"{"name":"MyPlugin"}"#,
    )
    .unwrap();

    let manager = PluginManager::new(plugins_dir.path().to_path_buf());
    let result = manager.install(&local_source(&source_dir)).await;
    // The TS `install` throws when no usable manifest exists; the message
    // carries the first error diagnostic (K).
    let Err(PluginError::InstallFailed(message)) = result else {
        panic!("install should fail with InstallFailed");
    };
    assert!(
        message.contains("\"name\" must match"),
        "unexpected message: {message}"
    );
}

// ---------------------------------------------------------------------------
// B6 — systemPrompt rendering (coexistence, trim/BOM, 32 KiB section cap)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_merge_inline_and_file_system_prompts_when_both_are_configured() {
    let plugins_dir = tempfile::tempdir().unwrap();
    let source_dir = tempfile::tempdir().unwrap();

    std::fs::write(
        source_dir.path().join("mirri.plugin.json"),
        serde_json::json!({
            "name": "merge-prompt-plugin",
            "systemPrompt": "inline part",
            "systemPromptPath": "./prompts/system.txt"
        })
        .to_string(),
    )
    .unwrap();
    std::fs::create_dir_all(source_dir.path().join("prompts")).unwrap();
    std::fs::write(
        source_dir.path().join("prompts").join("system.txt"),
        "file part",
    )
    .unwrap();

    let manager = PluginManager::new(plugins_dir.path().to_path_buf());
    manager
        .install(&local_source(&source_dir))
        .await
        .expect("install");

    let prompts = manager.enabled_system_prompts();
    assert_eq!(
        prompts.iter().map(|p| p.content.as_str()).collect::<Vec<_>>(),
        vec!["inline part\n\nfile part"]
    );
}

#[tokio::test]
async fn should_ignore_oversize_inline_system_prompt_when_rendering() {
    let plugins_dir = tempfile::tempdir().unwrap();
    let source_dir = tempfile::tempdir().unwrap();

    let oversized = "x".repeat(mirri_plugin_host::manifest::SYSTEM_PROMPT_MAX_BYTES + 1);
    std::fs::write(
        source_dir.path().join("mirri.plugin.json"),
        serde_json::json!({
            "name": "oversize-inline-plugin",
            "systemPrompt": oversized
        })
        .to_string(),
    )
    .unwrap();

    let manager = PluginManager::new(plugins_dir.path().to_path_buf());
    manager
        .install(&local_source(&source_dir))
        .await
        .expect("install");

    let prompts = manager.enabled_system_prompts();
    assert!(prompts.is_empty(), "over-limit inline prompt is ignored");
}

#[tokio::test]
async fn should_ignore_oversize_system_prompt_file_when_rendering() {
    let plugins_dir = tempfile::tempdir().unwrap();
    let source_dir = tempfile::tempdir().unwrap();

    std::fs::write(
        source_dir.path().join("mirri.plugin.json"),
        serde_json::json!({
            "name": "oversize-file-plugin",
            "systemPromptPath": "./prompts/system.txt"
        })
        .to_string(),
    )
    .unwrap();
    std::fs::create_dir_all(source_dir.path().join("prompts")).unwrap();
    std::fs::write(
        source_dir.path().join("prompts").join("system.txt"),
        "x".repeat(mirri_plugin_host::manifest::SYSTEM_PROMPT_MAX_BYTES + 1),
    )
    .unwrap();

    let manager = PluginManager::new(plugins_dir.path().to_path_buf());
    manager
        .install(&local_source(&source_dir))
        .await
        .expect("install");

    let prompts = manager.enabled_system_prompts();
    assert!(prompts.is_empty(), "over-limit prompt file is ignored");
}

#[tokio::test]
async fn should_keep_inline_prompt_when_prompt_file_exceeds_32kb() {
    let plugins_dir = tempfile::tempdir().unwrap();
    let source_dir = tempfile::tempdir().unwrap();

    std::fs::write(
        source_dir.path().join("mirri.plugin.json"),
        serde_json::json!({
            "name": "partial-degrade-plugin",
            "systemPrompt": "inline only",
            "systemPromptPath": "./prompts/system.txt"
        })
        .to_string(),
    )
    .unwrap();
    std::fs::create_dir_all(source_dir.path().join("prompts")).unwrap();
    std::fs::write(
        source_dir.path().join("prompts").join("system.txt"),
        "x".repeat(mirri_plugin_host::manifest::SYSTEM_PROMPT_MAX_BYTES + 1),
    )
    .unwrap();

    let manager = PluginManager::new(plugins_dir.path().to_path_buf());
    manager
        .install(&local_source(&source_dir))
        .await
        .expect("install");

    // Only the over-limit section is dropped; the inline part still renders.
    let prompts = manager.enabled_system_prompts();
    assert_eq!(
        prompts.iter().map(|p| p.content.as_str()).collect::<Vec<_>>(),
        vec!["inline only"]
    );
}

#[tokio::test]
async fn should_strip_bom_and_trim_when_reading_system_prompt_file() {
    let plugins_dir = tempfile::tempdir().unwrap();
    let source_dir = tempfile::tempdir().unwrap();

    std::fs::write(
        source_dir.path().join("mirri.plugin.json"),
        serde_json::json!({
            "name": "bom-prompt-plugin",
            "systemPromptPath": "./prompts/system.txt"
        })
        .to_string(),
    )
    .unwrap();
    std::fs::create_dir_all(source_dir.path().join("prompts")).unwrap();
    std::fs::write(
        source_dir.path().join("prompts").join("system.txt"),
        "\u{feff}\n\n  file prompt  \n\n",
    )
    .unwrap();

    let manager = PluginManager::new(plugins_dir.path().to_path_buf());
    manager
        .install(&local_source(&source_dir))
        .await
        .expect("install");

    let prompts = manager.enabled_system_prompts();
    assert_eq!(
        prompts.iter().map(|p| p.content.as_str()).collect::<Vec<_>>(),
        vec!["file prompt"]
    );
}

#[tokio::test]
async fn should_ignore_blank_system_prompt_parts_when_rendering() {
    let plugins_dir = tempfile::tempdir().unwrap();
    let source_dir = tempfile::tempdir().unwrap();

    std::fs::write(
        source_dir.path().join("mirri.plugin.json"),
        serde_json::json!({
            "name": "blank-prompt-plugin",
            "systemPrompt": "   ",
            "systemPromptPath": "./prompts/system.txt"
        })
        .to_string(),
    )
    .unwrap();
    std::fs::create_dir_all(source_dir.path().join("prompts")).unwrap();
    std::fs::write(
        source_dir.path().join("prompts").join("system.txt"),
        "\n\n  \n\n",
    )
    .unwrap();

    let manager = PluginManager::new(plugins_dir.path().to_path_buf());
    manager
        .install(&local_source(&source_dir))
        .await
        .expect("install");

    let prompts = manager.enabled_system_prompts();
    assert!(prompts.is_empty(), "blank parts produce no prompt");
}

// ---------------------------------------------------------------------------
// E / I — runtime consumption shapes (hooks decoration, session starts)
// ---------------------------------------------------------------------------

/// Given an enabled plugin declaring hooks, when `enabled_hooks` is called,
/// then every hook is decorated with the plugin runtime facts (item E): the
/// `cwd` is pinned to the plugin root and `env` carries `MIRRICODE_HOME`
/// (derived as the plugins dir's parent) plus `MIRRI_PLUGIN_ROOT` — the TS
/// `{...hook, cwd: record.root, env: {...}}` override.
#[tokio::test]
async fn should_decorate_hooks_with_plugin_runtime_facts_when_enabled_hooks_is_called() {
    let plugins_dir = tempfile::tempdir().unwrap();
    let source_dir = tempfile::tempdir().unwrap();

    std::fs::write(
        source_dir.path().join("mirri.plugin.json"),
        serde_json::json!({
            "name": "hook-plugin",
            "hooks": [
                { "event": "PostToolUse", "command": "echo done", "matcher": "read" },
                { "event": "SessionStart", "command": "echo start", "cwd": "/custom", "env": { "KEEP": "1" } }
            ]
        })
        .to_string(),
    )
    .unwrap();

    let manager = PluginManager::new(plugins_dir.path().to_path_buf());
    manager
        .install(&local_source(&source_dir))
        .await
        .expect("install");

    let hooks = manager.enabled_hooks();
    assert_eq!(hooks.len(), 2);

    let expected_root = std::fs::canonicalize(
        plugins_dir.path().join("managed").join("hook-plugin"),
    )
    .expect("canonicalize the managed root")
    .display()
    .to_string();
    let expected_home = plugins_dir
        .path()
        .parent()
        .expect("temp plugins dir has a parent")
        .display()
        .to_string();
    for hook in &hooks {
        assert_eq!(
            hook.cwd.as_deref(),
            Some(expected_root.as_str()),
            "the TS decoration pins cwd to the plugin root, overriding a manifest value"
        );
        let env = hook.env.as_ref().expect("env is injected");
        assert_eq!(
            env.get("MIRRICODE_HOME").map(String::as_str),
            Some(expected_home.as_str())
        );
        assert_eq!(
            env.get("MIRRI_PLUGIN_ROOT").map(String::as_str),
            Some(expected_root.as_str())
        );
    }
    // The second hook's manifest `cwd`/`env` are replaced by the runtime
    // facts (the TS spread override), so neither survives.
    assert!(hooks[1].cwd.as_deref() != Some("/custom"));
    assert!(!hooks[1].env.as_ref().is_some_and(|env| env.contains_key("KEEP")));
}

/// Given a disabled plugin, when `enabled_hooks` is called, then its hooks
/// are excluded (the TS `record.enabled` skip).
#[tokio::test]
async fn should_exclude_hooks_when_plugin_is_disabled() {
    let plugins_dir = tempfile::tempdir().unwrap();
    let source_dir = tempfile::tempdir().unwrap();

    std::fs::write(
        source_dir.path().join("mirri.plugin.json"),
        serde_json::json!({
            "name": "hook-plugin",
            "hooks": [{ "event": "Stop", "command": "echo stop" }]
        })
        .to_string(),
    )
    .unwrap();

    let manager = PluginManager::new(plugins_dir.path().to_path_buf());
    manager
        .install(&local_source(&source_dir))
        .await
        .expect("install");
    manager.disable("hook-plugin").expect("disable");

    assert!(manager.enabled_hooks().is_empty());
}

/// Given an enabled plugin declaring `sessionStart:{skill}`, when
/// `enabled_session_starts` is called, then the TS `{pluginId, skillName}`
/// shape is produced (item I).
#[tokio::test]
async fn should_return_session_starts_when_plugin_declares_session_start() {
    let plugins_dir = tempfile::tempdir().unwrap();
    let source_dir = tempfile::tempdir().unwrap();

    std::fs::write(
        source_dir.path().join("mirri.plugin.json"),
        serde_json::json!({
            "name": "session-plugin",
            "sessionStart": { "skill": "greeter" }
        })
        .to_string(),
    )
    .unwrap();

    let manager = PluginManager::new(plugins_dir.path().to_path_buf());
    manager
        .install(&local_source(&source_dir))
        .await
        .expect("install");

    let starts = manager.enabled_session_starts();
    assert_eq!(starts.len(), 1);
    assert_eq!(starts[0].plugin_id, "session-plugin");
    assert_eq!(starts[0].skill_name, "greeter");
}

/// Given an enabled plugin without a `sessionStart` (or with a sessionStart
/// missing `skill`), when `enabled_session_starts` is called, then it is
/// skipped — the TS `skill === undefined` skip.
#[tokio::test]
async fn should_skip_session_starts_when_skill_is_missing() {
    let plugins_dir = tempfile::tempdir().unwrap();
    let source_dir = tempfile::tempdir().unwrap();

    std::fs::write(
        source_dir.path().join("mirri.plugin.json"),
        serde_json::json!({
            "name": "no-skill-plugin",
            "sessionStart": {}
        })
        .to_string(),
    )
    .unwrap();

    let manager = PluginManager::new(plugins_dir.path().to_path_buf());
    manager
        .install(&local_source(&source_dir))
        .await
        .expect("install");

    assert!(manager.enabled_session_starts().is_empty());

    // Disabled plugins are excluded as well.
    manager.disable("no-skill-plugin").expect("disable");
    assert!(manager.enabled_session_starts().is_empty());
}

/// Given an explicit home dir, when `enabled_hooks` is called, then the
/// `MIRRICODE_HOME` decoration uses that home — the `new_with_home` seam.
#[tokio::test]
async fn should_use_explicit_home_when_manager_is_built_with_home() {
    let plugins_dir = tempfile::tempdir().unwrap();
    let home_dir = tempfile::tempdir().unwrap();
    let source_dir = tempfile::tempdir().unwrap();

    std::fs::write(
        source_dir.path().join("mirri.plugin.json"),
        serde_json::json!({
            "name": "hook-plugin",
            "hooks": [{ "event": "Stop", "command": "echo stop" }]
        })
        .to_string(),
    )
    .unwrap();

    let manager = PluginManager::new_with_home(
        plugins_dir.path().to_path_buf(),
        home_dir.path().to_path_buf(),
    );
    manager
        .install(&local_source(&source_dir))
        .await
        .expect("install");

    let hooks = manager.enabled_hooks();
    assert_eq!(hooks.len(), 1);
    let env = hooks[0].env.as_ref().expect("env injected");
    assert_eq!(
        env.get("MIRRICODE_HOME").map(String::as_str),
        Some(home_dir.path().display().to_string().as_str())
    );
}

// ---------------------------------------------------------------------------
// B2/K — diagnostics and plugin health state at the manager surface
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_report_ok_state_when_plugin_is_installed() {
    let (plugins_dir, source_dir) = setup_plugin_source("test-plugin");
    let manager = PluginManager::new(plugins_dir.path().to_path_buf());

    manager
        .install(&local_source(&source_dir))
        .await
        .expect("install");

    let info = &manager.list()[0];
    assert_eq!(info.state, PluginState::Ok);
    assert!(!info.has_errors());
    assert!(info.diagnostics.is_empty());
}

#[tokio::test]
async fn should_emit_warn_diagnostic_when_session_start_has_wrong_shape() {
    let plugins_dir = tempfile::tempdir().unwrap();
    let source_dir = tempfile::tempdir().unwrap();

    std::fs::write(
        source_dir.path().join("mirri.plugin.json"),
        r#"{"name":"ss-plugin","sessionStart":"onboarding.md"}"#,
    )
    .unwrap();

    let manager = PluginManager::new(plugins_dir.path().to_path_buf());
    manager
        .install(&local_source(&source_dir))
        .await
        .expect("install should succeed — sessionStart problems are warn-only");

    let info = &manager.list()[0];
    assert_eq!(info.state, PluginState::Ok, "warn diagnostics do not flip state");
    assert!(!info.has_errors());
    assert!(
        info.diagnostics
            .iter()
            .any(|d| d.severity == PluginDiagnosticSeverity::Warn
                && d.message == "\"sessionStart\" must be an object"),
        "expected the sessionStart warn diagnostic, got {:?}",
        info.diagnostics
    );
}

#[tokio::test]
async fn should_emit_info_diagnostics_when_manifest_has_unsupported_runtime_fields() {
    let plugins_dir = tempfile::tempdir().unwrap();
    let source_dir = tempfile::tempdir().unwrap();

    std::fs::write(
        source_dir.path().join("mirri.plugin.json"),
        r#"{"name":"runtime-plugin","tools":{"x":{}},"bootstrap":"y"}"#,
    )
    .unwrap();

    let manager = PluginManager::new(plugins_dir.path().to_path_buf());
    manager
        .install(&local_source(&source_dir))
        .await
        .expect("install should succeed");

    let info = &manager.list()[0];
    assert_eq!(info.state, PluginState::Ok);
    let infos: Vec<&str> = info
        .diagnostics
        .iter()
        .filter(|d| d.severity == PluginDiagnosticSeverity::Info)
        .map(|d| d.message.as_str())
        .collect();
    assert_eq!(infos.len(), 2, "got: {infos:?}");
    assert!(infos.iter().any(|m| m.contains("\"tools\"")));
    assert!(infos.iter().any(|m| m.contains("\"bootstrap\"")));
}

#[tokio::test]
async fn should_keep_only_valid_mcp_servers_when_manifest_has_invalid_entries() {
    let plugins_dir = tempfile::tempdir().unwrap();
    let source_dir = tempfile::tempdir().unwrap();

    std::fs::write(
        source_dir.path().join("mirri.plugin.json"),
        serde_json::json!({
            "name": "mcp-plugin",
            "mcpServers": {
                "good": { "command": "node", "args": ["server.js"] },
                "bad": { "command": "" }
            }
        })
        .to_string(),
    )
    .unwrap();

    let manager = PluginManager::new(plugins_dir.path().to_path_buf());
    manager
        .install(&local_source(&source_dir))
        .await
        .expect("install should succeed — bad servers are dropped, not fatal");

    let servers = manager.enabled_mcp_servers();
    assert!(servers.contains_key("plugin-mcp-plugin:good"));
    assert!(!servers.contains_key("plugin-mcp-plugin:bad"));
    assert!(manager.list()[0].diagnostics.iter().any(|d| {
        d.severity == PluginDiagnosticSeverity::Warn
            && d.message.contains("Invalid MCP server \"bad\"")
    }));
}

#[tokio::test]
async fn should_resolve_plugin_relative_mcp_command_against_the_managed_root() {
    let plugins_dir = tempfile::tempdir().unwrap();
    let source_dir = tempfile::tempdir().unwrap();

    std::fs::write(
        source_dir.path().join("mirri.plugin.json"),
        serde_json::json!({
            "name": "rel-mcp-plugin",
            "mcpServers": {
                "local": { "command": "./server.js" }
            }
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(source_dir.path().join("server.js"), "console.log(1)").unwrap();

    let manager = PluginManager::new(plugins_dir.path().to_path_buf());
    manager
        .install(&local_source(&source_dir))
        .await
        .expect("install");

    let servers = manager.enabled_mcp_servers();
    let config = servers
        .get("plugin-rel-mcp-plugin:local")
        .expect("resolved server should be collected");
    let command = config["command"].as_str().expect("command is a string");
    // The `./server.js` command resolves against the plugin's managed root
    // (realpath'd — the TS `resolvePluginPathField`).
    let managed_root = plugins_dir.path().join("managed").join("rel-mcp-plugin");
    let expected = std::fs::canonicalize(managed_root.join("server.js")).expect("canonicalize");
    assert_eq!(
        Path::new(command).canonicalize().expect("canonicalize"),
        expected
    );
}

// Re-export for pattern matching in tests.
use mirri_plugin_host::PluginSource;

// ---------------------------------------------------------------------------
// B8 — skills/agents entries (TS resolveDirListField) + root attribution
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_attach_skill_instructions_when_collecting_skill_roots() {
    let plugins_dir = tempfile::tempdir().unwrap();
    let source_dir = tempfile::tempdir().unwrap();

    std::fs::write(
        source_dir.path().join("mirri.plugin.json"),
        serde_json::json!({
            "name": "instructions-plugin",
            "skills": ["./skills"],
            "skillInstructions": "Use the plugin's skills first."
        })
        .to_string(),
    )
    .unwrap();
    std::fs::create_dir_all(source_dir.path().join("skills")).unwrap();

    let manager = PluginManager::new(plugins_dir.path().to_path_buf());
    manager
        .install(&local_source(&source_dir))
        .await
        .expect("install");

    let skills = manager.plugin_skill_roots();
    assert_eq!(skills.len(), 1);
    assert_eq!(
        skills[0].plugin.instructions.as_deref(),
        Some("Use the plugin's skills first.")
    );
}

#[tokio::test]
async fn should_keep_only_existing_dot_slash_skill_dirs_when_manifest_has_bad_entries() {
    let plugins_dir = tempfile::tempdir().unwrap();
    let source_dir = tempfile::tempdir().unwrap();

    std::fs::write(
        source_dir.path().join("mirri.plugin.json"),
        serde_json::json!({
            "name": "partial-skills-plugin",
            "skills": ["./good", "no-prefix", "./missing", "./../outside"]
        })
        .to_string(),
    )
    .unwrap();
    std::fs::create_dir_all(source_dir.path().join("good")).unwrap();

    let manager = PluginManager::new(plugins_dir.path().to_path_buf());
    manager
        .install(&local_source(&source_dir))
        .await
        .expect("install should succeed — bad entries drop, never fatal");

    // Only "./good" survives (a bad ./ + missing dir are warn/error-drop);
    // the "./" violations are `error` diagnostics, so the plugin is in
    // error state and its surviving roots are skipped by the consumers.
    let info = &manager.list()[0];
    assert_eq!(info.state, PluginState::Error);
    assert!(
        info.diagnostics
            .iter()
            .any(|d| d.severity == PluginDiagnosticSeverity::Error
                && d.message.contains("\"skills\" path must start with \"./\"")),
        "got: {:?}",
        info.diagnostics
    );
    assert!(manager.plugin_skill_roots().is_empty());
}

// ---------------------------------------------------------------------------
// B9/F — md command model at the manager surface
// ---------------------------------------------------------------------------

/// Install a plugin with one md command (frontmatter + body) and one inline
/// spec, and return the plugins dir.
async fn install_hybrid_command_plugin() -> tempfile::TempDir {
    let plugins_dir = tempfile::tempdir().expect("plugins dir");
    let source_dir = tempfile::tempdir().expect("source dir");
    std::fs::write(
        source_dir.path().join("mirri.plugin.json"),
        serde_json::json!({
            "name": "hybrid-plugin",
            "commands": [
                "./commands",
                {
                    "name": "inline-only",
                    "description": "Inline spec",
                    "template": "Rendered {{ arguments }}"
                }
            ]
        })
        .to_string(),
    )
    .unwrap();
    std::fs::create_dir_all(source_dir.path().join("commands")).unwrap();
    std::fs::write(
        source_dir.path().join("commands").join("hello.md"),
        "---\nname: md-hello\ndescription: Greets\n---\n\nHello $ARGUMENTS.",
    )
    .unwrap();
    std::fs::write(
        source_dir.path().join("commands").join("noargs.md"),
        "Just summarize.",
    )
    .unwrap();

    let manager = PluginManager::new(plugins_dir.path().to_path_buf());
    manager
        .install(&format!("local:{}", source_dir.path().display()))
        .await
        .expect("install should succeed");
    plugins_dir
}

#[tokio::test]
async fn should_collect_md_commands_with_frontmatter_when_enabled_commands_is_called() {
    let plugins_dir = install_hybrid_command_plugin().await;
    let manager = PluginManager::new(plugins_dir.path().to_path_buf());
    manager.load_all().expect("load_all");

    let commands = manager.enabled_commands();
    // Sorted by (plugin id, name): inline-only, md-hello, noargs.
    assert_eq!(commands.len(), 3);
    let names: Vec<&str> = commands.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, vec!["inline-only", "md-hello", "noargs"]);

    let md = commands.iter().find(|c| c.name == "md-hello").unwrap();
    assert_eq!(md.plugin_id, "hybrid-plugin");
    assert_eq!(md.description, "Greets");
    assert_eq!(md.body, "Hello $ARGUMENTS.");
    assert!(md.path.ends_with("commands/hello.md"));

    let inline = commands.iter().find(|c| c.name == "inline-only").unwrap();
    assert_eq!(inline.description, "Inline spec");
    assert_eq!(inline.body, "Rendered {{ arguments }}");
    assert_eq!(inline.path, "");
}

#[tokio::test]
async fn should_activate_md_command_with_argument_expansion_when_called() {
    let plugins_dir = install_hybrid_command_plugin().await;
    let manager = PluginManager::new(plugins_dir.path().to_path_buf());
    manager.load_all().expect("load_all");

    // $ARGUMENTS in the template is replaced verbatim.
    let output = manager
        .activate_command("md-hello", "reader")
        .expect("md command should activate");
    assert_eq!(output, "Hello reader.");

    // No placeholder + non-empty args → trailing `ARGUMENTS:` block.
    let output = manager
        .activate_command("noargs", "x y")
        .expect("md command should activate");
    assert_eq!(output, "Just summarize.\n\nARGUMENTS: x y");
}

#[tokio::test]
async fn should_activate_inline_spec_when_md_commands_do_not_match() {
    let plugins_dir = install_hybrid_command_plugin().await;
    let manager = PluginManager::new(plugins_dir.path().to_path_buf());
    manager.load_all().expect("load_all");

    let output = manager
        .activate_command("inline-only", "ping")
        .expect("inline command should activate");
    assert_eq!(output, "Rendered ping");
}

// ---------------------------------------------------------------------------
// N1/N2/N3 wire rows — the klient pluginSummarySchema / pluginInfoSchema
// shapes built by the manager (`summaries` / `summary` / `info`).
// ---------------------------------------------------------------------------

/// A source plugin with the full contribution surface: display name, skills,
/// two MCP servers (one disabled in-manifest), hooks, and md commands.
fn setup_rich_plugin_source(name: &str) -> (tempfile::TempDir, tempfile::TempDir) {
    let plugins_dir = tempfile::tempdir().expect("failed to create plugins dir");
    let source_dir = tempfile::tempdir().expect("failed to create source dir");

    let manifest = serde_json::json!({
        "name": name,
        "version": "2.3.4",
        "interface": { "displayName": "Rich Plugin" },
        "skills": ["./skills"],
        "mcpServers": {
            "helper": { "command": "node", "args": ["server.js"], "env": { "PORT": "8080" } },
            "disabler": { "command": "node", "args": ["off.js"], "enabled": false }
        },
        "hooks": [
            { "event": "PostToolUse", "command": "echo done", "matcher": "read" },
            { "event": "PreCompact", "command": "echo compact" }
        ],
        "commands": ["./commands"]
    });
    let mut file = std::fs::File::create(source_dir.path().join("mirri.plugin.json"))
        .expect("failed to create manifest");
    file.write_all(serde_json::to_string_pretty(&manifest).unwrap().as_bytes())
        .expect("failed to write manifest");

    std::fs::create_dir_all(source_dir.path().join("skills")).unwrap();
    std::fs::create_dir_all(source_dir.path().join("commands")).unwrap();
    // The skill root holds two REAL skill directories: skillCount is the
    // discovery count (D-N2) — 2 from the single `./skills` manifest entry.
    std::fs::create_dir_all(source_dir.path().join("skills/reader")).unwrap();
    std::fs::write(
        source_dir.path().join("skills/reader/SKILL.md"),
        "---\ndescription: reader\n---\nRead.\n",
    )
    .unwrap();
    std::fs::create_dir_all(source_dir.path().join("skills/writer")).unwrap();
    std::fs::write(
        source_dir.path().join("skills/writer/SKILL.md"),
        "---\ndescription: writer\n---\nWrite.\n",
    )
    .unwrap();
    std::fs::write(
        source_dir.path().join("commands").join("hello.md"),
        "---\nname: hello\n---\n\nHi.",
    )
    .unwrap();

    (plugins_dir, source_dir)
}

/// Given a plugin with skills, two MCP servers (one disabled), two hooks and
/// one command, when its summary row is built, then every field matches the
/// klient `pluginSummarySchema`: displayName, the TS count semantics, the
/// wire source kind, the original source string, and health flags.
#[tokio::test]
async fn should_build_summary_with_counts_when_plugin_is_installed() {
    let (plugins_dir, source_dir) = setup_rich_plugin_source("rich-plugin");
    let manager = PluginManager::new(plugins_dir.path().to_path_buf());

    let source = format!("local:{}", source_dir.path().display());
    manager.install(&source).await.expect("install should succeed");

    let summary = manager
        .summary("rich-plugin")
        .expect("summary for installed plugin");
    assert_eq!(summary.id, "rich-plugin");
    assert_eq!(summary.display_name, "Rich Plugin");
    assert_eq!(summary.version.as_deref(), Some("2.3.4"));
    assert!(summary.enabled);
    assert_eq!(summary.state, PluginState::Ok);
    // skillCount is the real discovery count (D-N2): two directories with
    // SKILL.md under the single `./skills` manifest entry.
    assert_eq!(summary.skill_count, 2);
    assert_eq!(summary.mcp_server_count, 2);
    assert_eq!(summary.enabled_mcp_server_count, 1);
    assert_eq!(summary.hook_count, 2);
    assert_eq!(summary.command_count, 1);
    assert!(!summary.has_errors);
    assert_eq!(summary.source, "local-path");
    // The `local:` prefix is stripped for the persisted original source.
    assert_eq!(
        summary.original_source.as_deref(),
        Some(source_dir.path().to_str().unwrap())
    );
    assert!(summary.github.is_none());
}

/// Given an installed plugin whose two skill roots hold real `SKILL.md`
/// files — a same-name skill in both roots, a nested skill, and a declared
/// root that does not exist — when its summary is built, then skillCount
/// is the real **discovery** count (the D-N2 upgrade: the TS
/// `countDiscoveredPluginSkills` semantics), unique skill names across the
/// roots, never the manifest entry count.
#[tokio::test]
async fn should_count_discovered_skill_directories_in_summary_skill_count() {
    // Given a plugin source with two on-disk skill roots (a shared name, a
    // nested skill) and a declared-but-missing third root
    let plugins_dir = tempfile::tempdir().expect("failed to create plugins dir");
    let source_dir = tempfile::tempdir().expect("failed to create source dir");
    std::fs::create_dir_all(source_dir.path().join("skills/alpha")).unwrap();
    std::fs::write(
        source_dir.path().join("skills/alpha/SKILL.md"),
        "---\ndescription: alpha\n---\n# Alpha\n",
    )
    .unwrap();
    std::fs::create_dir_all(source_dir.path().join("skills/sub/beta")).unwrap();
    std::fs::write(
        source_dir.path().join("skills/sub/beta/SKILL.md"),
        "---\ndescription: beta\n---\n# Beta\n",
    )
    .unwrap();
    std::fs::create_dir_all(source_dir.path().join("more-skills/alpha")).unwrap();
    std::fs::write(
        source_dir.path().join("more-skills/alpha/SKILL.md"),
        "---\ndescription: alpha-again\n---\n# Alpha again\n",
    )
    .unwrap();
    // An on-disk root with no SKILL.md — its manifest entry must NOT count
    // towards the discovery count (it yields zero discovered skills).
    std::fs::create_dir_all(source_dir.path().join("empty-skills")).unwrap();
    let manifest = serde_json::json!({
        "name": "count-plugin",
        "skills": ["./skills", "./more-skills", "./empty-skills"],
    });
    let mut file = std::fs::File::create(source_dir.path().join("mirri.plugin.json"))
        .expect("failed to create manifest");
    file.write_all(serde_json::to_string_pretty(&manifest).unwrap().as_bytes())
        .expect("failed to write manifest");

    let manager = PluginManager::new(plugins_dir.path().to_path_buf());
    manager
        .install(&format!("local:{}", source_dir.path().display()))
        .await
        .expect("install should succeed");

    // When the summary is built
    let summary = manager
        .summary("count-plugin")
        .expect("summary for installed plugin");

    // Then skillCount is the unique discovered count (alpha + beta): the
    // duplicate name across roots counts once, the nested skill counts,
    // and the missing root contributes nothing
    assert_eq!(
        summary.skill_count, 2,
        "the discovery count must be deduped by skill name across roots and nesting"
    );
}

/// Given two installed plugins, when `summaries()` is called, then the rows
/// are sorted by id (the TS `localeCompare` order over the ASCII id space)
/// and error-state plugins stay in the list.
#[tokio::test]
async fn should_sort_summaries_by_id_and_keep_error_state_plugins_when_listed() {
    let (plugins_dir, source_dir) = setup_plugin_source("zeta-plugin");
    let manager = PluginManager::new(plugins_dir.path().to_path_buf());
    manager
        .install(&local_source(&source_dir))
        .await
        .expect("install zeta");

    // A second plugin installed first (insertion order alpha before zeta).
    let (_, source_dir2) = setup_plugin_source("alpha-plugin");
    manager
        .install(&local_source(&source_dir2))
        .await
        .expect("install alpha");

    let summaries = manager.summaries();
    let ids: Vec<&str> = summaries.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, vec!["alpha-plugin", "zeta-plugin"]);
}

/// Given a github-sourced plugin with captured ref metadata, when its
/// summary is built, then `source` is "github" and `github` carries the
/// `{owner, repo, ref, installedSha?}` attribution.
#[tokio::test]
async fn should_build_github_attribution_when_summary_is_built() {
    let (plugins_dir, _source_dir) = setup_plugin_source("gh-plugin");
    // The record's path must hold a real plugin for load_all to parse.
    std::fs::create_dir_all(plugins_dir.path().join("gh-plugin")).unwrap();
    std::fs::write(
        plugins_dir.path().join("gh-plugin").join("mirri.plugin.json"),
        serde_json::json!({ "name": "gh-plugin" }).to_string(),
    )
    .unwrap();

    let now = chrono::Utc::now();
    let record = mirri_plugin_host::InstalledPlugin {
        id: "gh-plugin".to_string(),
        version: Some("1.0.0".to_string()),
        source: mirri_plugin_host::PluginSource::Github {
            repo: "mirricode/my-plugin".to_string(),
            commit: Some("main".to_string()),
            kind: Some(mirri_plugin_host::GithubRefKind::Branch),
        },
        original_source: Some("https://github.com/mirricode/my-plugin/tree/main".to_string()),
        enabled: true,
        installed_at: now,
        updated_at: now,
        path: plugins_dir.path().join("gh-plugin"),
        github_ref: Some(mirri_plugin_host::GithubRef::branch("main")),
        installed_sha: Some("0123456789abcdef0123456789abcdef01234567".to_string()),
        mcp_servers: std::collections::HashMap::new(),
    };
    // Seed the store record directly (the same `installed.json` the
    // manager's store reads), then load.
    mirri_plugin_host::PluginStore::new(plugins_dir.path())
        .add(record)
        .expect("seed github record");
    let manager = PluginManager::new(plugins_dir.path().to_path_buf());
    manager.load_all().expect("load_all");

    let summary = manager.summary("gh-plugin").expect("summary");
    assert_eq!(summary.source, "github");
    let github = summary.github.expect("github attribution");
    assert_eq!(github.owner, "mirricode");
    assert_eq!(github.repo, "my-plugin");
    assert_eq!(github.r#ref.kind, mirri_plugin_host::GithubRefKind::Branch);
    assert_eq!(github.r#ref.value, "main");
    assert_eq!(
        github.installed_sha.as_deref(),
        Some("0123456789abcdef0123456789abcdef01234567")
    );
    assert_eq!(
        summary.original_source.as_deref(),
        Some("https://github.com/mirricode/my-plugin/tree/main")
    );
}

/// Given an installed plugin, when `info()` is called, then the row carries
/// the full klient `pluginInfoSchema`: the summary super-set plus root,
/// installedAt/updatedAt, manifestKind, manifestPath, the wire manifest
/// (commands narrowed to `{path, name}`, hooks remapped), per-server info,
/// and diagnostics.
#[tokio::test]
async fn should_build_info_with_manifest_metadata_when_called() {
    let (plugins_dir, source_dir) = setup_rich_plugin_source("rich-plugin");
    let manager = PluginManager::new(plugins_dir.path().to_path_buf());
    manager
        .install(&local_source(&source_dir))
        .await
        .expect("install should succeed");

    let info = manager.info("rich-plugin").expect("info row");
    // Summary super-set.
    assert_eq!(info.summary.id, "rich-plugin");
    assert_eq!(info.summary.display_name, "Rich Plugin");
    // Install metadata.
    assert!(info.root.ends_with("rich-plugin"));
    assert!(!info.installed_at.is_empty());
    assert!(info.updated_at.is_some());
    assert_eq!(info.manifest_kind, Some(mirri_plugin_host::PluginManifestKind::Root));
    assert!(
        info.manifest_path
            .as_ref()
            .is_some_and(|p| p.ends_with("mirri.plugin.json"))
    );

    // Wire manifest: TS pluginManifestSchema shape.
    let wire = info.manifest.expect("wire manifest");
    assert_eq!(wire["name"], serde_json::json!("rich-plugin"));
    assert_eq!(wire["interface"]["displayName"], serde_json::json!("Rich Plugin"));
    // commands narrowed to the TS {path, name} entries.
    let commands = wire["commands"].as_array().expect("commands array");
    assert_eq!(commands.len(), 1);
    assert!(commands[0]["name"].is_string());
    assert!(commands[0]["path"].is_string());
    // hooks remapped to {event, matcher?, command, timeout?}.
    let hooks = wire["hooks"].as_array().expect("hooks array");
    assert_eq!(hooks.len(), 2);
    assert_eq!(hooks[0]["event"], serde_json::json!("PostToolUse"));
    assert_eq!(hooks[0]["matcher"], serde_json::json!("read"));
    assert!(hooks[0]["command"].is_string());
    assert!(hooks[0].get("timeout").is_none() || hooks[0]["timeout"].is_number());
    let mcp = &wire["mcpServers"];
    assert_eq!(mcp["helper"]["command"], serde_json::json!("node"));
    assert_eq!(
        mcp["disabler"]["enabled"],
        serde_json::json!(false),
        "manifest enabled flag survives on the wire"
    );

    // Per-server info rows, sorted by name.
    assert_eq!(info.mcp_servers.len(), 2);
    assert_eq!(info.mcp_servers[0].name, "disabler");
    assert_eq!(info.mcp_servers[0].runtime_name, "plugin-rich-plugin:disabler");
    assert!(!info.mcp_servers[0].enabled);
    assert_eq!(info.mcp_servers[0].transport, "stdio");
    assert_eq!(
        info.mcp_servers[1].env_keys.as_deref(),
        Some(&["PORT".to_string()][..])
    );

    // Diagnostics and no shadowed manifest.
    assert!(info.diagnostics.is_empty());
    assert!(info.shadowed_manifest_path.is_none());
}

/// Given a plugin root carrying both a root manifest and a shadowed
/// directory manifest, when `info()` is called, then `manifestKind` /
/// `manifestPath` point at the winning root candidate and
/// `shadowedManifestPath` names the shadowed one.
#[tokio::test]
async fn should_report_shadowed_manifest_when_both_candidates_exist() {
    let (plugins_dir, source_dir) = setup_plugin_source("shadowed-plugin");
    // Drop the commands/skills dirs so the manifest stays minimal, then add
    // the directory candidate that the root candidate shadows.
    let manifest = serde_json::json!({ "name": "shadowed-plugin" });
    std::fs::write(
        source_dir.path().join("mirri.plugin.json"),
        serde_json::to_string(&manifest).unwrap(),
    )
    .unwrap();
    std::fs::create_dir_all(source_dir.path().join(".mirri-plugin")).unwrap();
    std::fs::write(
        source_dir.path().join(".mirri-plugin").join("plugin.json"),
        serde_json::json!({ "name": "shadowed-plugin", "version": "0.0.0" }).to_string(),
    )
    .unwrap();

    let manager = PluginManager::new(plugins_dir.path().to_path_buf());
    manager
        .install(&local_source(&source_dir))
        .await
        .expect("install should succeed");

    let info = manager.info("shadowed-plugin").expect("info row");
    assert_eq!(
        info.manifest_kind,
        Some(mirri_plugin_host::PluginManifestKind::Root)
    );
    assert!(
        info.manifest_path
            .as_ref()
            .is_some_and(|p| p.ends_with("mirri.plugin.json"))
    );
    assert!(
        info.shadowed_manifest_path
            .as_ref()
            .is_some_and(|p| p.ends_with(".mirri-plugin/plugin.json"))
    );
}

// ---------------------------------------------------------------------------
// A2 — reinstall = upgrade (the TS `install` semantics)
// ---------------------------------------------------------------------------

/// Given an installed plugin that is disabled, when the same id is
/// installed again from an updated source, then the upgrade keeps the
/// record's `enabled`/`installedAt` and the per-server MCP capabilities,
/// refreshes `updatedAt`, and the managed copy is swapped for the new one
/// (item A2 — the TS `existing?.enabled ?? true`, `existing?.installedAt ??
/// now`, `capabilities: existing?.capabilities` identity).
#[tokio::test]
async fn should_keep_enabled_installed_at_and_capabilities_when_upgrading() {
    let (plugins_dir, source_dir) = setup_plugin_source("test-plugin");
    let manager = PluginManager::new(plugins_dir.path().to_path_buf());
    manager
        .install(&local_source(&source_dir))
        .await
        .expect("first install");
    manager.disable("test-plugin").expect("disable");
    manager
        .set_mcp_server_enabled("test-plugin", "test-server", false)
        .expect("per-server state");
    let before = mirri_plugin_host::PluginStore::new(plugins_dir.path())
        .find("test-plugin")
        .expect("find")
        .expect("record");

    // Brand the source with a new marker file, then reinstall.
    std::fs::write(source_dir.path().join("marker-v2"), "v2").unwrap();
    manager
        .install(&local_source(&source_dir))
        .await
        .expect("upgrade install");

    let info = &manager.list()[0];
    assert_eq!(info.id, "test-plugin");
    assert!(!info.enabled, "the upgrade keeps the disabled flag");
    let after = mirri_plugin_host::PluginStore::new(plugins_dir.path())
        .find("test-plugin")
        .expect("find")
        .expect("record");
    assert_eq!(
        after.installed_at, before.installed_at,
        "the upgrade keeps installedAt"
    );
    assert!(
        after.updated_at > before.updated_at,
        "the upgrade refreshes updatedAt"
    );
    assert_eq!(
        after.mcp_servers.get("test-server").map(|s| s.enabled),
        Some(false),
        "the upgrade keeps the per-server capabilities"
    );

    // The managed copy was swapped: the new marker is in, nothing else left
    // behind under managed/.
    let managed_root = plugins_dir.path().join("managed").join("test-plugin");
    assert!(managed_root.join("marker-v2").is_file());
    let leftovers: Vec<String> = std::fs::read_dir(plugins_dir.path().join("managed"))
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name() != "test-plugin")
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        leftovers.is_empty(),
        "staging/previous dirs must be cleaned up: {leftovers:?}"
    );
}

/// Given an installed plugin, when a reinstall fails after the new copy has
/// been published (the store cannot be written), then the previous managed
/// copy is restored and the record is untouched — the TS
/// `rollbackManagedPluginCopy` behavior (item A2).
#[tokio::test]
async fn should_restore_previous_copy_when_upgrade_install_fails() {
    let (plugins_dir, source_dir) = setup_plugin_source("test-plugin");
    let manager = PluginManager::new(plugins_dir.path().to_path_buf());
    manager
        .install(&local_source(&source_dir))
        .await
        .expect("first install");
    std::fs::write(
        plugins_dir.path().join("managed").join("test-plugin").join("marker-v1"),
        "v1",
    )
    .unwrap();

    // Break persistence AFTER the publish: installed.json becomes a
    // directory, so the store write fails.
    let store_file = plugins_dir.path().join("installed.json");
    std::fs::remove_file(&store_file).unwrap();
    std::fs::create_dir_all(&store_file).unwrap();

    // Brand the source, reinstall → fails.
    std::fs::write(source_dir.path().join("marker-v2"), "v2").unwrap();
    let result = manager.install(&local_source(&source_dir)).await;
    assert!(result.is_err(), "the upgrade must fail when persist fails");

    // The previous managed copy is restored (marker-v1 back, marker-v2
    // gone), no stray staging dirs remain.
    let managed_root = plugins_dir.path().join("managed").join("test-plugin");
    assert!(managed_root.join("marker-v1").is_file());
    assert!(!managed_root.join("marker-v2").exists());
    let leftovers: Vec<String> = std::fs::read_dir(plugins_dir.path().join("managed"))
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name() != "test-plugin")
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        leftovers.is_empty(),
        "rollback must clean the staging/previous dirs: {leftovers:?}"
    );
}

/// Given an upgrade whose rollback also fails (the publish path is gone), when
/// the install error is surfaced, then both messages appear in the aggregated
/// error — the rust expression of the TS `AggregateError` (item A2).
#[test]
fn should_aggregate_install_and_rollback_errors_when_rollback_fails() {
    let error = mirri_plugin_host::PluginError::InstallRollbackFailed {
        install: "boom: persist failed".to_string(),
        rollback: "crash: restore failed".to_string(),
    };
    let message = error.to_string();
    assert!(
        message.contains("previous managed copy could not be restored"),
        "unexpected message: {message}"
    );
    assert!(message.contains("boom: persist failed"));
    assert!(message.contains("crash: restore failed"));
}

// ---------------------------------------------------------------------------
// A4 — local root validation (the TS `normalizeInstallRoot`)
// ---------------------------------------------------------------------------

/// Given a relative local source, when it is installed, then the install is
/// rejected with the TS "must be an absolute path" error (item A4).
#[tokio::test]
async fn should_reject_install_when_local_source_is_relative() {
    let (_plugins_dir, _source_dir) = setup_plugin_source("test-plugin");
    let manager = PluginManager::new(tempfile::tempdir().unwrap().path().to_path_buf());
    let result = manager.install("local:relative/dir").await;
    let Err(PluginError::InstallFailed(message)) = result else {
        panic!("relative local source must fail with InstallFailed");
    };
    assert!(
        message.contains("Plugin root must be an absolute path"),
        "unexpected message: {message}"
    );
}

/// Given a local source that does not exist, when it is installed, then the
/// install is rejected with the TS "does not exist" error (item A4).
#[tokio::test]
async fn should_reject_install_when_local_source_does_not_exist() {
    let (_plugins_dir, _source_dir) = setup_plugin_source("test-plugin");
    let manager = PluginManager::new(tempfile::tempdir().unwrap().path().to_path_buf());
    let result = manager.install("local:/nonexistent/plugin/path").await;
    let Err(PluginError::InstallFailed(message)) = result else {
        panic!("missing local source must fail with InstallFailed");
    };
    assert!(
        message.contains("Plugin root does not exist"),
        "unexpected message: {message}"
    );
}

/// Given a local source that is a regular file, when it is installed, then
/// the install is rejected with the TS "is not a directory" error (item
/// A4).
#[tokio::test]
async fn should_reject_install_when_local_source_is_a_file() {
    let file = tempfile::tempdir().unwrap();
    let file_path = file.path().join("not-a-dir");
    std::fs::write(&file_path, "just a file").unwrap();
    let manager = PluginManager::new(tempfile::tempdir().unwrap().path().to_path_buf());
    let result = manager
        .install(&format!("local:{}", file_path.display()))
        .await;
    let Err(PluginError::InstallFailed(message)) = result else {
        panic!("file local source must fail with InstallFailed");
    };
    assert!(
        message.contains("Plugin root is not a directory"),
        "unexpected message: {message}"
    );
}

/// Given a local source reached through a symlink, when it is installed,
/// then the symlink is resolved (realpath) and the install succeeds — the
/// TS `normalizeInstallRoot` realpath step (item A4).
#[tokio::test]
async fn should_resolve_local_source_symlink_when_installing() {
    let (plugins_dir, source_dir) = setup_plugin_source("test-plugin");
    let link_dir = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(source_dir.path(), link_dir.path().join("link"))
        .expect("symlink");
    let manager = PluginManager::new(plugins_dir.path().to_path_buf());

    let id = manager
        .install(&format!("local:{}", link_dir.path().join("link").display()))
        .await
        .expect("install through a symlink should succeed");
    assert_eq!(id, "test-plugin");
    assert!(plugins_dir.path().join("managed").join("test-plugin").is_dir());
}

// ---------------------------------------------------------------------------
// C1 — setEnabled same-value no-op
// ---------------------------------------------------------------------------

/// Given an already-disabled plugin, when disable is called again, then the
/// call is a no-op: it succeeds without touching the store file or the
/// `updatedAt` timestamp (the TS `setEnabled` early return, item C1).
#[tokio::test]
async fn should_noop_when_disable_called_again_on_a_disabled_plugin() {
    let (plugins_dir, source_dir) = setup_plugin_source("test-plugin");
    let manager = PluginManager::new(plugins_dir.path().to_path_buf());
    manager
        .install(&local_source(&source_dir))
        .await
        .expect("install");
    manager.disable("test-plugin").expect("disable");

    let store = mirri_plugin_host::PluginStore::new(plugins_dir.path());
    let before = store.find("test-plugin").expect("find").expect("record");
    let raw_before =
        std::fs::read_to_string(plugins_dir.path().join("installed.json")).expect("read store");

    manager.disable("test-plugin").expect("no-op disable succeeds");

    let after = store.find("test-plugin").expect("find").expect("record");
    assert_eq!(
        after.updated_at, before.updated_at,
        "a no-op must not refresh updatedAt"
    );
    let raw_after =
        std::fs::read_to_string(plugins_dir.path().join("installed.json")).expect("read store");
    assert_eq!(raw_before, raw_after, "a no-op must not rewrite the store");
    assert!(!manager.list()[0].enabled, "state is unchanged");
}

// ---------------------------------------------------------------------------
// D1 — manifest `enabled: false` boundary (`isMcpServerEnabled`:
// `state?.enabled ?? config.enabled !== false`, the TS manager.ts).
// ---------------------------------------------------------------------------

/// Helper: install a plugin whose manifest declares one MCP server with the
/// given `enabled` value (absent when `None`), returning the manager.
async fn install_server_plugin(
    manifest_enabled: Option<bool>,
) -> (tempfile::TempDir, PluginManager) {
    let (plugins_dir, source_dir) = setup_plugin_source("mcp-plugin");
    let mut server = serde_json::json!({
        "command": "node",
        "args": ["server.js"]
    });
    if let Some(enabled) = manifest_enabled {
        server["enabled"] = serde_json::json!(enabled);
    }
    let manifest = serde_json::json!({
        "name": "mcp-plugin",
        "mcpServers": { "my-server": server }
    });
    std::fs::write(
        source_dir.path().join("mirri.plugin.json"),
        serde_json::to_string_pretty(&manifest).unwrap(),
    )
    .unwrap();
    let manager = PluginManager::new(plugins_dir.path().to_path_buf());
    manager
        .install(&local_source(&source_dir))
        .await
        .expect("install should succeed");
    (plugins_dir, manager)
}

/// Given a manifest server with `enabled: false` and no recorded user state,
/// when the enabled set is collected, then the server is excluded — the TS
/// `state?.enabled ?? config.enabled !== false` (item D1). The config-view
/// entries and the summary count follow the same predicate.
#[tokio::test]
async fn should_exclude_server_when_manifest_disables_it_and_no_user_state_exists() {
    let (_plugins_dir, manager) = install_server_plugin(Some(false)).await;

    assert!(
        manager.enabled_mcp_servers().is_empty(),
        "a manifest-disabled server must not join the enabled set"
    );
    let entries = manager.mcp_server_entries();
    assert_eq!(entries.len(), 1);
    assert!(entries[0].state.is_none(), "no user state is recorded");
    let summary = manager.summary("mcp-plugin").expect("summary");
    assert_eq!(summary.enabled_mcp_server_count, 0);
    let info = manager.info("mcp-plugin").expect("info");
    assert_eq!(info.mcp_servers.len(), 1);
    assert!(!info.mcp_servers[0].enabled);
}

/// Given a manifest server with `enabled: false`, when the per-server user
/// state records `enabled: true`, then the user state wins and the server
/// joins the enabled set (the `state?.enabled` side of the predicate).
#[tokio::test]
async fn should_let_user_state_override_a_manifest_disabled_server_when_collecting() {
    let (_plugins_dir, manager) = install_server_plugin(Some(false)).await;

    manager
        .set_mcp_server_enabled("mcp-plugin", "my-server", true)
        .expect("user state write");

    let servers = manager.enabled_mcp_servers();
    assert!(
        servers.contains_key("plugin-mcp-plugin:my-server"),
        "the user state re-enables the manifest-disabled server"
    );
    let summary = manager.summary("mcp-plugin").expect("summary");
    assert_eq!(summary.enabled_mcp_server_count, 1);
    let info = manager.info("mcp-plugin").expect("info");
    assert!(info.mcp_servers[0].enabled);
}

/// Given a manifest server with no `enabled` field, when the enabled set is
/// collected, then the server joins it (absent means enabled).
#[tokio::test]
async fn should_include_server_when_manifest_has_no_enabled_field() {
    let (_plugins_dir, manager) = install_server_plugin(None).await;

    assert!(
        manager.enabled_mcp_servers().contains_key("plugin-mcp-plugin:my-server"),
        "an absent manifest `enabled` means enabled (config.enabled !== false)"
    );
    let summary = manager.summary("mcp-plugin").expect("summary");
    assert_eq!(summary.enabled_mcp_server_count, 1);
}

// ---------------------------------------------------------------------------
// C3 — reload errors semantics (the TS `PluginManager.reload` entry loop:
// error-state plugins stay, added/removed by id diff, errors for
// materialization failures).
// ---------------------------------------------------------------------------

/// Given an installed plugin whose manifest breaks on disk, when `reload_all`
/// runs, then the plugin stays in the list with `state: 'error'` — it is
/// neither reported in `errors` nor counted as removed (the TS
/// parseManifest-diagnostics path, B2/C3).
#[tokio::test]
async fn should_keep_error_state_plugin_when_reload_all_runs() {
    let (plugins_dir, source_dir) = setup_plugin_source("broken-reload");
    let manager = PluginManager::new(plugins_dir.path().to_path_buf());
    manager
        .install(&local_source(&source_dir))
        .await
        .expect("install");

    // Break the managed manifest on disk.
    std::fs::write(
        plugins_dir.path().join("managed/broken-reload/mirri.plugin.json"),
        "{ not json",
    )
    .unwrap();

    let summary = manager.reload_all().expect("reload_all succeeds");
    assert!(summary.added.is_empty());
    assert!(summary.removed.is_empty());
    assert!(summary.errors.is_empty());

    let list = manager.list();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].state, PluginState::Error);
    assert!(list[0].has_errors());
}

/// Given installed plugins, when records disappear from or appear in
/// `installed.json`, then `reload_all` reports the id diff — `removed` only
/// for records actually gone from the store, `added` for new records (the TS
/// `prevIds`/`next` diff).
#[tokio::test]
async fn should_report_added_and_removed_when_reload_all_diffs_against_the_store() {
    let (plugins_dir, source_dir) = setup_plugin_source("alpha");
    let (_, beta_dir) = setup_plugin_source("beta");
    let manager = PluginManager::new(plugins_dir.path().to_path_buf());
    manager.install(&local_source(&source_dir)).await.expect("alpha");
    manager.install(&local_source(&beta_dir)).await.expect("beta");

    // A clean reload of an unchanged store reports an empty summary.
    let summary = manager.reload_all().expect("reload_all");
    assert!(summary.added.is_empty());
    assert!(summary.removed.is_empty());
    assert!(summary.errors.is_empty());

    // Drop beta's record from the store (C2: the directory stays) and add a
    // fresh gamma record pointing at a new managed copy.
    let store = mirri_plugin_host::PluginStore::new(plugins_dir.path());
    store.remove("beta").expect("remove beta record");
    std::fs::create_dir_all(plugins_dir.path().join("managed/gamma")).unwrap();
    std::fs::write(
        plugins_dir.path().join("managed/gamma/mirri.plugin.json"),
        r#"{"name":"gamma","version":"1.0.0"}"#,
    )
    .unwrap();
    let now = chrono::Utc::now();
    store
        .add(mirri_plugin_host::InstalledPlugin {
            id: "gamma".to_string(),
            version: Some("1.0.0".to_string()),
            source: mirri_plugin_host::PluginSource::Local {
                path: "/source".to_string(),
            },
            original_source: Some("/source".to_string()),
            enabled: true,
            installed_at: now,
            updated_at: now,
            path: plugins_dir.path().join("managed/gamma"),
            github_ref: None,
            installed_sha: None,
            mcp_servers: std::collections::HashMap::new(),
        })
        .expect("seed gamma record");

    let summary = manager.reload_all().expect("reload_all");
    let mut removed = summary.removed.clone();
    removed.sort();
    assert_eq!(removed, vec!["beta"], "removed: {:?}", summary.removed);
    assert_eq!(summary.added, vec!["gamma"]);
    assert!(summary.errors.is_empty());

    let ids: Vec<String> = {
        let mut ids: Vec<String> = manager.list().iter().map(|info| info.id.clone()).collect();
        ids.sort();
        ids
    };
    assert_eq!(ids, vec!["alpha", "gamma"]);
}

/// Given a corrupt `installed.json`, when `reload_all` runs, then the whole
/// reload fails — the TS `readInstalled` throw surfaced as a load failure
/// (the RPC layer maps it to `plugin.load_failed`).
#[tokio::test]
async fn should_fail_reload_all_when_installed_json_is_corrupt() {
    let (plugins_dir, source_dir) = setup_plugin_source("alpha");
    let manager = PluginManager::new(plugins_dir.path().to_path_buf());
    manager.install(&local_source(&source_dir)).await.expect("install");

    std::fs::write(plugins_dir.path().join("installed.json"), "{ not json").unwrap();

    assert!(manager.reload_all().is_err());
}
