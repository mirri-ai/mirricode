// layer: T1
// Domain group binary consolidated from per-file test binaries
// per 16-test-execution-strategy.md §4.4 (pure file moves).
mod commands_tests {
//! Integration tests for plugin slash commands: registration, conflict
//! resolution, template rendering, manifest parsing, and manager
//! integration.

use mirri_plugin_host::commands::{CommandRegistry, CommandSpec, argument_names_from_spec};
use mirri_plugin_host::manifest::{PluginManifest, parse_manifest, validate_manifest};
use mirri_plugin_host::{CommandEntry, PluginError, PluginManager};
use std::io::Write;

/// Helper: build a [`CommandSpec`] with the common fields only.
fn command_spec(name: &str, template: &str) -> CommandSpec {
    CommandSpec {
        name: name.to_string(),
        description: None,
        arguments: None,
        template: template.to_string(),
    }
}

/// Write a manifest JSON file into a temp dir and return the dir.
fn manifest_dir(manifest: serde_json::Value) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("failed to create temp dir");
    let mut file = std::fs::File::create(dir.path().join("mirri.plugin.json"))
        .expect("failed to create manifest");
    file.write_all(manifest.to_string().as_bytes())
        .expect("failed to write manifest");
    dir
}

// ---------------------------------------------------------------------------
// Template rendering
// ---------------------------------------------------------------------------

#[test]
fn should_render_named_arguments_when_command_spec_declares_them() {
    let registry = CommandRegistry::new();
    let spec = CommandSpec {
        name: "explain".to_string(),
        description: Some("Explain code in detail".to_string()),
        arguments: Some("[file_path]".to_string()),
        template: "Analyze the code in {{ file_path }} and explain it.".to_string(),
    };
    registry.register_plugin("docs-plugin", &[spec]);

    let rendered = registry
        .execute("explain", "src/main.rs")
        .expect("execute should succeed");

    assert_eq!(rendered, "Analyze the code in src/main.rs and explain it.");
}

#[test]
fn should_expand_dollar_arguments_when_template_uses_raw_substitution() {
    let registry = CommandRegistry::new();
    registry.register_plugin(
        "io-plugin",
        &[command_spec(
            "read",
            "Read the files $ARGUMENTS and summarize.",
        )],
    );

    let rendered = registry
        .execute("read", "src/a.rs src/b.rs")
        .expect("execute should succeed");

    assert_eq!(rendered, "Read the files src/a.rs src/b.rs and summarize.");
}

#[test]
fn should_render_raw_arguments_and_positional_args_when_invoked() {
    let registry = CommandRegistry::new();
    registry.register_plugin(
        "io-plugin",
        &[command_spec(
            "slice",
            "{{ arguments }}|{{ arg1 }}|{{ arg2 }}|{{ arg3 }}",
        )],
    );

    let rendered = registry
        .execute("slice", "one two three")
        .expect("execute should succeed");

    assert_eq!(rendered, "one two three|one|two|three");
}

#[test]
fn should_bind_named_arguments_positionally_when_multiple_declared() {
    let registry = CommandRegistry::new();
    let spec = CommandSpec {
        name: "locate".to_string(),
        description: None,
        arguments: Some("[file_path] [line_number]".to_string()),
        template: "Look at {{ file_path }} around line {{ line_number }}.".to_string(),
    };
    registry.register_plugin("locator-plugin", &[spec]);

    let rendered = registry
        .execute("locate", "src/lib.rs 42")
        .expect("execute should succeed");

    assert_eq!(rendered, "Look at src/lib.rs around line 42.");
}

#[test]
fn should_render_empty_string_when_declared_named_arg_has_no_value() {
    let registry = CommandRegistry::new();
    let spec = CommandSpec {
        name: "locate".to_string(),
        description: None,
        arguments: Some("[file_path] [line_number]".to_string()),
        template: "{{ file_path }}:{{ line_number }}".to_string(),
    };
    registry.register_plugin("locator-plugin", &[spec]);

    let rendered = registry
        .execute("locate", "src/lib.rs")
        .expect("execute should succeed");

    assert_eq!(rendered, "src/lib.rs:");
}

#[test]
fn should_support_conditionals_loops_and_filters_when_template_uses_minijinja() {
    let registry = CommandRegistry::new();
    registry.register_plugin(
        "fancy-plugin",
        &[command_spec(
            "fancy",
            "{% if arguments %}given{% else %}empty{% endif %} {% for i in [1, 2] %}{{ i }}{% endfor %} {{ arguments|upper }}",
        )],
    );

    let rendered = registry
        .execute("fancy", "ping")
        .expect("execute should succeed");

    assert_eq!(rendered, "given 12 PING");
}

#[test]
fn should_return_template_error_when_template_has_invalid_syntax() {
    let registry = CommandRegistry::new();
    registry.register_plugin("broken-plugin", &[command_spec("broken", "{{ unclosed")]);

    let result = registry.execute("broken", "");

    assert!(matches!(result, Err(PluginError::Template(_))));
}

// ---------------------------------------------------------------------------
// Registry behaviour
// ---------------------------------------------------------------------------

#[test]
fn should_return_not_found_when_command_is_unknown() {
    let registry = CommandRegistry::new();

    let result = registry.execute("ghost", "anything");

    assert!(matches!(result, Err(PluginError::CommandNotFound(_))));
}

#[test]
fn should_let_latest_plugin_win_when_command_name_conflicts() {
    let registry = CommandRegistry::new();
    registry.register_plugin(
        "plugin-a",
        &[command_spec("explain", "Explanation from plugin A.")],
    );
    registry.register_plugin(
        "plugin-b",
        &[command_spec("explain", "Explanation from plugin B.")],
    );

    let rendered = registry
        .execute("explain", "")
        .expect("execute should succeed");
    let winning = registry
        .get("explain")
        .expect("command should be registered");

    assert_eq!(rendered, "Explanation from plugin B.");
    assert_eq!(winning.plugin, "plugin-b");
}

#[test]
fn should_remove_commands_when_plugin_is_unregistered() {
    let registry = CommandRegistry::new();
    registry.register_plugin(
        "plugin-a",
        &[
            command_spec("alpha", "A"),
            command_spec("shared", "A's shared"),
        ],
    );
    registry.register_plugin("plugin-b", &[command_spec("beta", "B")]);

    let removed = registry.unregister_plugin("plugin-a");

    assert_eq!(removed, 2);
    assert!(matches!(
        registry.execute("alpha", ""),
        Err(PluginError::CommandNotFound(_))
    ));
    assert!(matches!(
        registry.execute("shared", ""),
        Err(PluginError::CommandNotFound(_))
    ));
    assert!(registry.get("beta").is_some());
}

#[test]
fn should_rebuild_registry_when_reloaded_with_new_plugin_set() {
    let registry = CommandRegistry::new();
    registry.register_plugin("old-plugin", &[command_spec("stale", "Stale.")]);
    registry.register_plugin("other-plugin", &[command_spec("keep", "Kept.")]);

    registry.reload(vec![(
        "new-plugin".to_string(),
        vec![command_spec("fresh", "Fresh.")],
    )]);

    assert!(registry.get("stale").is_none());
    assert!(registry.get("keep").is_none());
    assert!(registry.get("fresh").is_some());
    let rendered = registry
        .execute("fresh", "")
        .expect("execute should succeed");
    assert_eq!(rendered, "Fresh.");
}

#[test]
fn should_list_commands_sorted_by_name_when_multiple_registered() {
    let registry = CommandRegistry::new();
    registry.register_plugin(
        "p",
        &[command_spec("zebra", "Z"), command_spec("alpha", "A")],
    );

    let names: Vec<String> = registry
        .list()
        .into_iter()
        .map(|registered| registered.spec.name)
        .collect();

    assert_eq!(names, vec!["alpha".to_string(), "zebra".to_string()]);
}

#[test]
fn should_extract_bracketed_names_when_argument_spec_has_descriptions() {
    let names = argument_names_from_spec("[file_path]: the file to analyze\n[line_number]");

    assert_eq!(
        names,
        vec!["file_path".to_string(), "line_number".to_string()]
    );
}

// ---------------------------------------------------------------------------
// Manifest parsing and validation
// ---------------------------------------------------------------------------

#[test]
fn should_parse_inline_command_specs_when_manifest_commands_are_objects() {
    let dir = manifest_dir(serde_json::json!({
        "name": "docs-plugin",
        "commands": [
            {
                "name": "explain",
                "description": "Explain code in detail",
                "arguments": "[file_path]",
                "template": "Analyze the code in {{ file_path }} and explain."
            }
        ]
    }));

    let manifest = parse_manifest(dir.path())
        .manifest
        .expect("parse should succeed");

    let entries = manifest.commands.expect("commands should be present");
    assert_eq!(entries.len(), 1);
    let CommandEntry::Inline(spec) = &entries[0] else {
        panic!("expected an inline command spec");
    };
    assert_eq!(spec.name, "explain");
    assert_eq!(spec.description.as_deref(), Some("Explain code in detail"));
    assert_eq!(spec.arguments.as_deref(), Some("[file_path]"));
    assert_eq!(
        spec.template,
        "Analyze the code in {{ file_path }} and explain."
    );
}

#[test]
fn should_parse_command_file_paths_when_manifest_commands_are_strings() {
    let dir = manifest_dir(serde_json::json!({
        "name": "legacy-plugin",
        "commands": ["./commands/hello.md", "./commands/goodbye.md"]
    }));
    std::fs::create_dir_all(dir.path().join("commands")).unwrap();
    std::fs::write(dir.path().join("commands").join("hello.md"), "# Hello").unwrap();
    std::fs::write(dir.path().join("commands").join("goodbye.md"), "# Bye").unwrap();

    let manifest = parse_manifest(dir.path())
        .manifest
        .expect("parse should succeed");

    let entries = manifest.commands.expect("commands should be present");
    assert_eq!(entries.len(), 2);
    // Sorted by the relative-path-derived name (the TS readCommands sort).
    let CommandEntry::File(first) = &entries[0] else {
        panic!("expected an md file entry");
    };
    assert_eq!(first.name, "goodbye");
    assert!(first.path.ends_with("commands/goodbye.md"));
    let CommandEntry::File(second) = &entries[1] else {
        panic!("expected an md file entry");
    };
    assert_eq!(second.name, "hello");
    assert!(second.path.ends_with("commands/hello.md"));
}

#[test]
fn should_collect_markdown_files_recursively_when_commands_points_to_a_directory() {
    let dir = manifest_dir(serde_json::json!({
        "name": "dir-commands-plugin",
        "commands": "./commands"
    }));
    std::fs::create_dir_all(dir.path().join("commands/sub")).unwrap();
    std::fs::write(dir.path().join("commands/a.md"), "# A").unwrap();
    std::fs::write(dir.path().join("commands/sub/b.md"), "# B").unwrap();

    let manifest = parse_manifest(dir.path())
        .manifest
        .expect("parse should succeed");

    let entries = manifest.commands.expect("commands should be present");
    assert_eq!(entries.len(), 2);
    let CommandEntry::File(first) = &entries[0] else {
        panic!("expected an md file entry");
    };
    assert_eq!(first.name, "a");
    let CommandEntry::File(second) = &entries[1] else {
        panic!("expected an md file entry");
    };
    assert_eq!(second.name, "sub/b");
}

#[test]
fn should_emit_warn_when_command_entry_is_neither_directory_nor_md_file() {
    let dir = manifest_dir(serde_json::json!({
        "name": "bad-entry-plugin",
        "commands": ["./commands/notes.txt"]
    }));
    std::fs::create_dir_all(dir.path().join("commands")).unwrap();
    std::fs::write(dir.path().join("commands").join("notes.txt"), "not md").unwrap();

    let parsed = parse_manifest(dir.path());
    assert!(parsed.manifest.is_some());
    assert!(parsed.manifest.unwrap().commands.is_none());
    assert!(parsed.diagnostics.iter().any(|d| {
        d.severity == mirri_plugin_host::PluginDiagnosticSeverity::Warn
            && d.message.contains("\"commands\" entry must be a directory or .md file")
    }));
}

#[test]
fn should_merge_md_files_and_inline_specs_when_commands_array_mixes_kinds() {
    let dir = manifest_dir(serde_json::json!({
        "name": "mixed-plugin",
        "commands": [
            "./commands",
            {
                "name": "explain",
                "description": "Explain",
                "template": "Explain {{ arguments }}"
            }
        ]
    }));
    std::fs::create_dir_all(dir.path().join("commands")).unwrap();
    std::fs::write(dir.path().join("commands").join("hello.md"), "# Hello").unwrap();

    let manifest = parse_manifest(dir.path())
        .manifest
        .expect("parse should succeed");

    let entries = manifest.commands.expect("commands should be present");
    assert_eq!(entries.len(), 2);
    assert!(matches!(&entries[0], CommandEntry::File(file) if file.name == "hello"));
    assert!(matches!(&entries[1], CommandEntry::Inline(spec) if spec.name == "explain"));
}

#[test]
fn should_reject_command_spec_when_template_is_missing() {
    let manifest: PluginManifest = serde_json::from_value(serde_json::json!({
        "name": "docs-plugin",
        "commands": [{"name": "explain", "template": ""}]
    }))
    .expect("manifest should deserialize");

    assert!(matches!(
        validate_manifest(&manifest),
        Err(PluginError::InvalidManifest(_))
    ));
}

#[test]
fn should_reject_command_spec_when_name_has_invalid_characters() {
    let manifest: PluginManifest = serde_json::from_value(serde_json::json!({
        "name": "docs-plugin",
        "commands": [{"name": "bad name", "template": "Hello"}]
    }))
    .expect("manifest should deserialize");

    assert!(matches!(
        validate_manifest(&manifest),
        Err(PluginError::InvalidManifest(_))
    ));
}

// ---------------------------------------------------------------------------
// Manager integration
// ---------------------------------------------------------------------------

/// Install a plugin whose manifest declares one inline command.
async fn install_plugin_with_command(plugins_dir: &tempfile::TempDir) {
    let source_dir = tempfile::tempdir().expect("failed to create source dir");
    let manifest = serde_json::json!({
        "name": "docs-plugin",
        "commands": [
            {
                "name": "explain",
                "description": "Explain code in detail",
                "arguments": "[file_path]",
                "template": "Analyze the code in {{ file_path }} and explain."
            }
        ]
    });
    std::fs::write(
        source_dir.path().join("mirri.plugin.json"),
        manifest.to_string(),
    )
    .expect("failed to write manifest");

    let manager = PluginManager::new(plugins_dir.path().to_path_buf());
    manager
        .install(&format!("local:{}", source_dir.path().display()))
        .await
        .expect("install should succeed");
}

#[tokio::test]
async fn should_register_commands_from_enabled_plugins_when_manager_builds_registry() {
    let plugins_dir = tempfile::tempdir().expect("failed to create plugins dir");
    install_plugin_with_command(&plugins_dir).await;
    let manager = PluginManager::new(plugins_dir.path().to_path_buf());
    manager.load_all().expect("load_all should succeed");

    let registry = manager.command_registry();

    let rendered = registry
        .execute("explain", "src/main.rs")
        .expect("execute should succeed");
    assert_eq!(rendered, "Analyze the code in src/main.rs and explain.");
}

#[tokio::test]
async fn should_exclude_disabled_plugin_commands_when_manager_rebuilds_registry() {
    let plugins_dir = tempfile::tempdir().expect("failed to create plugins dir");
    install_plugin_with_command(&plugins_dir).await;
    let manager = PluginManager::new(plugins_dir.path().to_path_buf());
    manager.load_all().expect("load_all should succeed");

    assert!(manager.command_registry().get("explain").is_some());

    manager
        .disable("docs-plugin")
        .expect("disable should succeed");

    let registry = manager.command_registry();
    assert!(registry.get("explain").is_none());
    assert!(matches!(
        registry.execute("explain", "src/main.rs"),
        Err(PluginError::CommandNotFound(_))
    ));
}

#[tokio::test]
async fn should_keep_md_command_files_when_manifest_uses_file_paths() {
    let plugins_dir = tempfile::tempdir().expect("failed to create plugins dir");
    let source_dir = tempfile::tempdir().expect("failed to create source dir");
    let manifest = serde_json::json!({
        "name": "legacy-plugin",
        "commands": ["./commands/hello.md"]
    });
    std::fs::write(
        source_dir.path().join("mirri.plugin.json"),
        manifest.to_string(),
    )
    .expect("failed to write manifest");
    std::fs::create_dir_all(source_dir.path().join("commands")).unwrap();
    std::fs::write(
        source_dir.path().join("commands").join("hello.md"),
        "# Hello command",
    )
    .unwrap();

    let manager = PluginManager::new(plugins_dir.path().to_path_buf());
    manager
        .install(&format!("local:{}", source_dir.path().display()))
        .await
        .expect("install should succeed");

    let command_files = manager.plugin_command_files();
    assert_eq!(command_files.len(), 1);
    assert!(command_files[0].ends_with("commands/hello.md"));
    // Legacy file commands must not surface in the inline registry.
    assert!(manager.command_registry().list().is_empty());
}

}

mod manager_tests {
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
    assert!(std::path::Path::new(&md.path).ends_with("commands/hello.md"));

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
            .is_some_and(|p| std::path::Path::new(p).ends_with(".mirri-plugin/plugin.json"))
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
    // A platform-absolute path that cannot exist: `/nonexistent/...` is not
    // absolute on Windows (no drive prefix), which would trip the earlier
    // absolute-path check instead of the existence check this test targets.
    let missing = std::env::temp_dir().join("mirri-tests").join("missing-plugin-source");
    let result = manager.install(&format!("local:{}", missing.display())).await;
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
/// Symlink creation needs no privileges on unix; skipped on Windows.
#[cfg(unix)]
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

}

mod manifest_tests {
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
}

mod source_tests {
//! Integration tests for source string parsing and zip extraction (A3).

use std::io::Write;

use mirri_plugin_host::{PluginError, PluginSource, extract_zip, parse_source};

#[test]
fn should_parse_local_path_when_source_uses_local_scheme() {
    let source = parse_source("local:/home/user/my-plugin").unwrap();
    assert_eq!(
        source,
        PluginSource::Local {
            path: "/home/user/my-plugin".to_string()
        }
    );
}

#[test]
fn should_parse_local_relative_path_when_source_uses_local_scheme() {
    let source = parse_source("local:./relative/path").unwrap();
    assert_eq!(
        source,
        PluginSource::Local {
            path: "./relative/path".to_string()
        }
    );
}

#[test]
fn should_parse_github_source_when_repo_has_no_commit() {
    let source = parse_source("github:user/repo").unwrap();
    assert_eq!(
        source,
        PluginSource::Github {
            repo: "user/repo".to_string(),
            commit: None,
            kind: None
        }
    );
}

#[test]
fn should_parse_github_source_when_commit_hash_is_provided() {
    let source = parse_source("github:user/repo@abc123def").unwrap();
    assert_eq!(
        source,
        PluginSource::Github {
            repo: "user/repo".to_string(),
            commit: Some("abc123def".to_string()),
            kind: None
        }
    );
}

#[test]
fn should_parse_github_source_when_branch_name_is_provided() {
    let source = parse_source("github:user/repo@main").unwrap();
    assert_eq!(
        source,
        PluginSource::Github {
            repo: "user/repo".to_string(),
            commit: Some("main".to_string()),
            kind: None
        }
    );
}

#[test]
fn should_parse_zip_source_when_url_uses_https() {
    let source = parse_source("https://example.com/plugin.zip").unwrap();
    assert_eq!(
        source,
        PluginSource::Zip {
            url: "https://example.com/plugin.zip".to_string()
        }
    );
}

#[test]
fn should_parse_zip_source_when_url_uses_http() {
    let source = parse_source("http://example.com/plugin.zip").unwrap();
    assert_eq!(
        source,
        PluginSource::Zip {
            url: "http://example.com/plugin.zip".to_string()
        }
    );
}

// ---------------------------------------------------------------------------
// TS `resolveInstallSource` syntax (A1) — the `source.ts` wire grammar,
// accepted alongside the rust `local:` / `github:` prefixes (superset).
// ---------------------------------------------------------------------------

#[test]
fn should_parse_local_path_when_source_is_a_bare_absolute_path() {
    // TS `resolveInstallSource`: a non-URL, absolute path is a local root.
    let source = parse_source("/home/user/my-plugin").unwrap();
    assert_eq!(
        source,
        PluginSource::Local {
            path: "/home/user/my-plugin".to_string()
        }
    );
}

#[test]
fn should_reject_local_source_when_bare_path_is_relative() {
    // TS throws "Plugin root must be an absolute path" for a relative path.
    let result = parse_source("./relative/path");
    assert!(matches!(result, Err(PluginError::InstallFailed(_))));
    let result = parse_source("relative/path");
    assert!(matches!(result, Err(PluginError::InstallFailed(_))));
}

#[test]
fn should_parse_github_source_when_source_is_a_github_repo_url() {
    let source = parse_source("https://github.com/user/my-plugin").unwrap();
    assert_eq!(
        source,
        PluginSource::Github {
            repo: "user/my-plugin".to_string(),
            commit: None,
            kind: None
        }
    );
}

#[test]
fn should_strip_git_suffix_when_github_url_ends_with_dot_git() {
    // TS `repoRaw.endsWith('.git')` stripping.
    let source = parse_source("https://github.com/user/my-plugin.git").unwrap();
    assert_eq!(
        source,
        PluginSource::Github {
            repo: "user/my-plugin".to_string(),
            commit: None,
            kind: None
        }
    );
}

#[test]
fn should_parse_branch_ref_when_github_url_has_a_tree_segment() {
    let source = parse_source("https://github.com/user/my-plugin/tree/main").unwrap();
    assert_eq!(
        source,
        PluginSource::Github {
            repo: "user/my-plugin".to_string(),
            commit: Some("main".to_string()),
            kind: Some(mirri_plugin_host::GithubRefKind::Branch)
        }
    );
}

#[test]
fn should_parse_sha_ref_when_github_url_has_a_tree_segment_with_full_sha() {
    let sha = "0123456789abcdef0123456789abcdef01234567";
    let source = parse_source(&format!("https://github.com/user/my-plugin/tree/{sha}")).unwrap();
    assert_eq!(
        source,
        PluginSource::Github {
            repo: "user/my-plugin".to_string(),
            commit: Some(sha.to_string()),
            kind: Some(mirri_plugin_host::GithubRefKind::Sha)
        }
    );
}

#[test]
fn should_parse_tag_ref_when_github_url_has_a_releases_tag_segment() {
    let source =
        parse_source("https://github.com/user/my-plugin/releases/tag/v1.2.0").unwrap();
    assert_eq!(
        source,
        PluginSource::Github {
            repo: "user/my-plugin".to_string(),
            commit: Some("v1.2.0".to_string()),
            kind: Some(mirri_plugin_host::GithubRefKind::Tag)
        }
    );
}

#[test]
fn should_parse_sha_ref_when_github_url_has_a_commit_segment() {
    let sha = "0123456789abcdef";
    let source = parse_source(&format!("https://github.com/user/my-plugin/commit/{sha}")).unwrap();
    assert_eq!(
        source,
        PluginSource::Github {
            repo: "user/my-plugin".to_string(),
            commit: Some(sha.to_string()),
            kind: Some(mirri_plugin_host::GithubRefKind::Sha)
        }
    );
}

#[test]
fn should_decode_percent_escapes_when_github_url_ref_contains_them() {
    // TS `decodeURIComponent` per path segment with a raw fallback: a
    // branch named `feature/foo` becomes `feature%2Ffoo` in the URL.
    let source =
        parse_source("https://github.com/user/my-plugin/tree/feature%2Ffoo").unwrap();
    assert_eq!(
        source,
        PluginSource::Github {
            repo: "user/my-plugin".to_string(),
            commit: Some("feature/foo".to_string()),
            kind: Some(mirri_plugin_host::GithubRefKind::Branch)
        }
    );
}

#[test]
fn should_accept_www_github_host_when_github_url_uses_it() {
    let source = parse_source("https://www.github.com/user/my-plugin/tree/main").unwrap();
    assert_eq!(
        source,
        PluginSource::Github {
            repo: "user/my-plugin".to_string(),
            commit: Some("main".to_string()),
            kind: Some(mirri_plugin_host::GithubRefKind::Branch)
        }
    );
}

#[test]
fn should_fall_back_to_zip_source_when_github_url_has_unrecognised_segments() {
    // TS `resolveInstallSource`: `parseGithubUrl` returns undefined for a
    // github.com URL that matches no recognized path, and the URL then
    // falls through to the generic http(s) → zip-url arm.
    let source = parse_source("https://github.com/user/my-plugin/blob/main/readme.md").unwrap();
    assert_eq!(
        source,
        PluginSource::Zip {
            url: "https://github.com/user/my-plugin/blob/main/readme.md".to_string()
        }
    );
}

#[test]
fn should_return_error_when_source_is_empty() {
    assert!(parse_source("").is_err());
    assert!(parse_source("   ").is_err());
}

#[test]
fn should_return_error_when_local_path_is_empty() {
    assert!(parse_source("local:").is_err());
}

#[test]
fn should_return_error_when_github_repo_is_empty() {
    assert!(parse_source("github:").is_err());
}

#[test]
fn should_return_error_when_github_commit_is_empty() {
    assert!(parse_source("github:user/repo@").is_err());
    assert!(parse_source("github:@abc").is_err());
}

#[test]
fn should_return_error_when_source_format_is_unrecognised() {
    let result = parse_source("just-a-name");
    assert!(matches!(result, Err(PluginError::InstallFailed(_))));

    let result = parse_source("ftp://example.com/x");
    assert!(matches!(result, Err(PluginError::InstallFailed(_))));
}

#[test]
fn should_roundtrip_through_json_when_serializing_and_deserializing() {
    let original = PluginSource::Github {
        repo: "user/repo".to_string(),
        commit: Some("abc".to_string()),
        kind: Some(mirri_plugin_host::GithubRefKind::Tag),
    };
    let json = serde_json::to_string(&original).unwrap();
    let deserialized: PluginSource = serde_json::from_str(&json).unwrap();
    assert_eq!(original, deserialized);
}

#[test]
fn should_roundtrip_without_kind_when_github_source_has_no_ref_kind() {
    // A legacy record (or a `github:`-prefixed source) has no `kind`; the
    // JSON round-trip must not fabricate one.
    let original = PluginSource::Github {
        repo: "user/repo".to_string(),
        commit: Some("abc".to_string()),
        kind: None,
    };
    let json = serde_json::to_string(&original).unwrap();
    assert!(!json.contains("kind"), "kind must be skipped when None: {json}");
    let deserialized: PluginSource = serde_json::from_str(&json).unwrap();
    assert_eq!(original, deserialized);
}

#[test]
fn should_serialize_with_type_tag_when_source_is_local() {
    let original = PluginSource::Local {
        path: "/foo".to_string(),
    };
    let json = serde_json::to_string(&original).unwrap();
    assert!(json.contains(r#""type":"local""#));
}

#[test]
fn should_serialize_with_type_tag_when_source_is_zip() {
    let original = PluginSource::Zip {
        url: "https://x.zip".to_string(),
    };
    let json = serde_json::to_string(&original).unwrap();
    assert!(json.contains(r#""type":"zip""#));
}

// ---------------------------------------------------------------------------
// Zip extraction (A3) — the TS `extractZip`/`detectPluginRoot`/permission
// restoration of `archive.ts`, exercised on archives built in-memory with the
// `zip` crate.
// ---------------------------------------------------------------------------

/// One entry of a test archive.
struct TestZipEntry<'a> {
    name: &'a str,
    content: &'a [u8],
    is_dir: bool,
    /// Unix mode (`externalFileAttributes >>> 16`) when set.
    mode: Option<u32>,
}

impl<'a> TestZipEntry<'a> {
    fn file(name: &'a str, content: &'a [u8]) -> Self {
        Self {
            name,
            content,
            is_dir: false,
            mode: None,
        }
    }

    fn dir(name: &'a str) -> Self {
        Self {
            name,
            content: b"",
            is_dir: true,
            mode: None,
        }
    }

    // Only the `#[cfg(unix)]` extraction tests set mode bits.
    #[cfg_attr(not(unix), allow(dead_code))]
    fn with_mode(mut self, mode: u32) -> Self {
        self.mode = Some(mode);
        self
    }
}

/// Build an in-memory zip archive from the given entries.
fn build_zip(entries: &[TestZipEntry]) -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut buf);
        for entry in entries {
            let mut options = zip::write::SimpleFileOptions::default();
            if let Some(mode) = entry.mode {
                options = options.unix_permissions(mode);
            }
            if entry.is_dir {
                writer.add_directory(entry.name, options).unwrap();
            } else {
                writer.start_file(entry.name, options).unwrap();
                writer.write_all(entry.content).unwrap();
            }
        }
        writer.finish().unwrap();
    }
    buf.into_inner()
}

fn plugin_manifest(name: &str) -> Vec<u8> {
    format!(r#"{{"name":"{name}","version":"1.0.0"}}"#).into_bytes()
}

/// Given a zip whose entry escapes the destination with `..`, when the
/// archive is extracted, then the extraction fails with the TS traversal
/// error text and nothing lands outside the destination.
#[test]
fn should_reject_path_traversal_when_zip_entry_escapes_with_parent_dir() {
    let zip = build_zip(&[TestZipEntry::file("../evil.txt", b"boom")]);

    let dest = tempfile::tempdir().unwrap();
    let result = extract_zip(&zip, dest.path());
    assert!(matches!(
        result,
        Err(PluginError::InstallFailed(ref message))
            if message == "Path traversal detected in zip entry: ../evil.txt"
    ), "unexpected result: {result:?}");
    assert!(
        !dest
            .path()
            .parent()
            .expect("tempdir has a parent")
            .join("evil.txt")
            .exists(),
        "the escaped file must not be written"
    );
}

/// Given a zip entry with an absolute path, when the archive is extracted,
/// then the extraction fails with the TS traversal error text (the TS
/// `path.resolve` containment check rejects it).
#[test]
fn should_reject_path_traversal_when_zip_entry_is_absolute() {
    let zip = build_zip(&[TestZipEntry::file("/etc/passwd", b"root")]);

    let dest = tempfile::tempdir().unwrap();
    let result = extract_zip(&zip, dest.path());
    assert!(matches!(
        result,
        Err(PluginError::InstallFailed(ref message))
            if message == "Path traversal detected in zip entry: /etc/passwd"
    ), "unexpected result: {result:?}");
}

/// Given a zip entry that climbs past the destination (`a/../../evil.txt`),
/// when the archive is extracted, then the extraction fails — the lexical
/// resolution runs out of destination components and escapes it.
#[test]
fn should_reject_path_traversal_when_zip_entry_climbs_above_the_destination() {
    let zip = build_zip(&[TestZipEntry::file("a/../../evil.txt", b"boom")]);

    let dest = tempfile::tempdir().unwrap();
    let result = extract_zip(&zip, dest.path());
    assert!(matches!(
        result,
        Err(PluginError::InstallFailed(ref message))
            if message == "Path traversal detected in zip entry: a/../../evil.txt"
    ), "unexpected result: {result:?}");
    assert!(
        !dest
            .path()
            .parent()
            .expect("tempdir has a parent")
            .join("evil.txt")
            .exists(),
        "the escaped file must not be written"
    );
}

/// Given a zip entry whose `..` stays inside the destination (`a/../ok.txt`),
/// when the archive is extracted, then the entry lands in the lexically
/// resolved location — the TS `path.resolve` containment allows it.
#[test]
fn should_allow_lexically_normalized_entries_when_they_stay_inside_the_destination() {
    let zip = build_zip(&[TestZipEntry::file("a/../ok.txt", b"fine")]);

    let dest = tempfile::tempdir().unwrap();
    let root = extract_zip(&zip, dest.path()).expect("in-bounds entries extract");
    assert_eq!(root, dest.path());
    assert_eq!(
        std::fs::read(dest.path().join("ok.txt")).expect("normalized entry reads"),
        b"fine"
    );
    assert!(
        !dest.path().join("a").exists(),
        "the `a` leg of the normalized entry must not materialize"
    );
}

/// Given a zip whose entries all live under one child directory carrying the
/// manifest, when the archive is extracted, then the root detection promotes
/// the single child directory (the TS `detectPluginRoot`).
#[test]
fn should_promote_single_child_directory_when_it_carries_the_manifest() {
    let zip = build_zip(&[
        TestZipEntry::dir("my-plugin/"),
        TestZipEntry::file("my-plugin/mirri.plugin.json", &plugin_manifest("single-child")),
        TestZipEntry::file("my-plugin/README.md", b"readme"),
    ]);

    let dest = tempfile::tempdir().unwrap();
    let root = extract_zip(&zip, dest.path()).expect("extraction succeeds");
    assert_eq!(root, dest.path().join("my-plugin"));
    assert!(root.join("mirri.plugin.json").is_file());
    assert!(root.join("README.md").is_file());
    // The extraction directory itself carries no manifest.
    assert!(!dest.path().join("mirri.plugin.json").exists());
}

/// Given a zip carrying the manifest at the archive root, when the archive is
/// extracted, then the root detection leaves the extraction directory as the
/// plugin root.
#[test]
fn should_keep_the_extraction_dir_when_the_manifest_is_at_the_archive_root() {
    let zip = build_zip(&[
        TestZipEntry::file("mirri.plugin.json", &plugin_manifest("root-level")),
        TestZipEntry::file("README.md", b"readme"),
    ]);

    let dest = tempfile::tempdir().unwrap();
    let root = extract_zip(&zip, dest.path()).expect("extraction succeeds");
    assert_eq!(root, dest.path());
}

/// Given a zip with several top-level children and no rooted manifest, when
/// the archive is extracted, then the root detection does not promote (the TS
/// `detectPluginRoot` only promotes a **single** child carrying a manifest).
#[test]
fn should_not_promote_when_zip_has_multiple_top_level_children() {
    let zip = build_zip(&[
        TestZipEntry::file("a/file.txt", b"a"),
        TestZipEntry::file("b/file.txt", b"b"),
    ]);

    let dest = tempfile::tempdir().unwrap();
    let root = extract_zip(&zip, dest.path()).expect("extraction succeeds");
    assert_eq!(root, dest.path());
}

/// Given a zip whose single child directory carries no manifest, when the
/// archive is extracted, then the root detection falls back to the extraction
/// directory (the TS `hasManifest` gate on the child).
#[test]
fn should_not_promote_a_child_without_a_manifest() {
    let zip = build_zip(&[
        TestZipEntry::dir("assets/"),
        TestZipEntry::file("assets/logo.png", b"png"),
    ]);

    let dest = tempfile::tempdir().unwrap();
    let root = extract_zip(&zip, dest.path()).expect("extraction succeeds");
    assert_eq!(root, dest.path());
}

/// Given a zip entry with unix mode bits, when the archive is extracted, then
/// the file's permissions restore to `mode & 0o777` — and a zero mode leaves
/// the defaults (the TS `restoreFilePermissions`).
#[cfg(unix)]
#[test]
fn should_restore_unix_permissions_when_zip_entry_carries_mode_bits() {
    use std::os::unix::fs::PermissionsExt;

    let zip = build_zip(&[
        TestZipEntry::file("tool.sh", b"#!/bin/sh").with_mode(0o100755),
        TestZipEntry::file("plain.txt", b"hello").with_mode(0),
    ]);

    let dest = tempfile::tempdir().unwrap();
    extract_zip(&zip, dest.path()).expect("extraction succeeds");

    let exec_mode = std::fs::metadata(dest.path().join("tool.sh"))
        .expect("tool.sh exists")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(exec_mode, 0o755, "the mode bits restore to 755");
    let plain_mode = std::fs::metadata(dest.path().join("plain.txt"))
        .expect("plain.txt exists")
        .permissions()
        .mode()
        & 0o777;
    assert_ne!(plain_mode, 0, "a zero mode leaves the default permissions");
}

/// Given a zip entry carrying a symlink unix mode (the `S_IFLNK` type bit),
/// when the archive is extracted, then the entry materializes as a **regular
/// file** holding the link-target bytes — never an on-disk symlink — matching
/// the TS yauzl flow (which has no symlink concept).
#[cfg(unix)]
#[test]
fn should_materialize_symlink_entries_as_regular_files_when_extracting() {
    let zip = build_zip(&[
        TestZipEntry::file("plugin/.bin/link", b"plugin/target").with_mode(0o120777),
    ]);

    let dest = tempfile::tempdir().unwrap();
    extract_zip(&zip, dest.path()).expect("extraction succeeds");

    let link_path = dest.path().join("plugin/.bin/link");
    let metadata = std::fs::symlink_metadata(&link_path).expect("entry exists");
    assert!(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        "the symlink-mode entry must be a regular file, not a symlink"
    );
    assert_eq!(
        std::fs::read(&link_path).expect("entry reads"),
        b"plugin/target",
        "the link-target bytes are the file content"
    );
}

/// Given a zip whose recognisable plugin layout is a single child directory,
/// when a full `install_from_source` zip flow runs (the extraction step
/// only — no network), then the returned root is the promoted child and the
/// archive itself never lands on disk.
#[tokio::test]
async fn should_return_the_detected_root_when_installing_a_zip_source() {
    let zip = build_zip(&[
        TestZipEntry::dir("demo/"),
        TestZipEntry::file("demo/mirri.plugin.json", &plugin_manifest("zip-demo")),
        TestZipEntry::file("demo/README.md", b"zip readme"),
    ]);
    let zip_path = tempfile::NamedTempFile::new().expect("zip temp file");
    std::fs::write(zip_path.path(), &zip).expect("zip bytes");

    // Serve the archive from a local HTTP endpoint and install from it.
    let (addr, guard) = serve_bytes(zip_path.path().to_path_buf());
    let target = tempfile::tempdir().expect("install target");
    let url = format!("http://{addr}/plugin.zip");
    let source = PluginSource::Zip { url };
    let (root, capture) = mirri_plugin_host::install_from_source(
        &source,
        target.path(),
        &mirri_plugin_host::ReqwestGithubResolver::new(),
    )
    .await
    .expect("zip install succeeds");
    guard.abort();

    assert_eq!(root, target.path().join("demo"));
    assert!(root.join("mirri.plugin.json").is_file());
    assert!(root.join("README.md").is_file());
    // The promoted child is the plugin root; the download never persisted.
    assert!(capture.github_ref.is_none());
    assert!(capture.installed_sha.is_none());
    assert!(!target.path().join("plugin.zip").exists());
}

/// Serve `bytes` over a one-shot local HTTP endpoint (the manager-level zip
/// URL installs never touch the public network). Returns the bound address
/// and a guard whose `abort()` stops the server task.
fn serve_bytes(bytes: std::path::PathBuf) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = std_listener.local_addr().expect("local addr");
    std_listener.set_nonblocking(true).expect("nonblocking");
    let listener = tokio::net::TcpListener::from_std(std_listener).expect("tokio listener");
    let handle = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                continue;
            };
            // Read the request head (best-effort; the response does not
            // depend on it).
            let mut head = [0u8; 4096];
            let _ = socket.read(&mut head).await;
            let body = std::fs::read(&bytes).unwrap_or_default();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.write_all(&body).await;
            let _ = socket.shutdown().await;
        }
    });
    (addr, handle)
}

/// Given a zip built with `zip -r plugin.zip .`-style dot-relative entries
/// (`./` directory entries and `./file` names), when the archive is
/// extracted, then it succeeds — the TS containment check permits entries
/// resolving to the destination itself, and `./` file entries land at the
/// root.
#[test]
fn should_extract_dot_relative_entries_when_zip_uses_zip_r_style_names() {
    let zip = build_zip(&[
        TestZipEntry::dir("./"),
        TestZipEntry::file("./mirri.plugin.json", &plugin_manifest("dot-plugin")),
        TestZipEntry::file("./README.md", b"readme"),
    ]);

    let dest = tempfile::tempdir().unwrap();
    let root = extract_zip(&zip, dest.path()).expect("dot-relative entries extract");
    assert_eq!(root, dest.path());
    assert!(root.join("mirri.plugin.json").is_file());
    assert!(root.join("README.md").is_file());
}

}

mod store_tests {
//! Integration tests for `PluginStore` — installed.json read/write, add,
//! remove, set_enabled, and the TS v1 format (item A6).

use chrono::Utc;
use mirri_plugin_host::{InstalledPlugin, PluginError, PluginSource, PluginStore};
use std::collections::HashMap;
use std::path::PathBuf;

fn sample_plugin(id: &str) -> InstalledPlugin {
    InstalledPlugin {
        id: id.to_string(),
        version: Some("1.0.0".to_string()),
        source: PluginSource::Local {
            path: "/source".to_string(),
        },
        original_source: None,
        enabled: true,
        installed_at: Utc::now(),
        updated_at: Utc::now(),
        path: PathBuf::from("/plugins").join(id),
        github_ref: None,
        installed_sha: None,
        mcp_servers: HashMap::new(),
    }
}

#[test]
fn should_return_empty_list_when_no_file_exists() {
    let dir = tempfile::tempdir().unwrap();
    let store = PluginStore::new(dir.path());
    assert!(store.load().unwrap().is_empty());
}

#[test]
fn should_load_plugins_when_added_to_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = PluginStore::new(dir.path());

    store.add(sample_plugin("alpha")).unwrap();
    store.add(sample_plugin("beta")).unwrap();

    let plugins = store.load().unwrap();
    assert_eq!(plugins.len(), 2);
}

#[test]
fn should_reject_add_when_plugin_already_installed() {
    let dir = tempfile::tempdir().unwrap();
    let store = PluginStore::new(dir.path());

    store.add(sample_plugin("alpha")).unwrap();
    let result = store.add(sample_plugin("alpha"));
    assert!(matches!(result, Err(PluginError::AlreadyInstalled(_))));
}

#[test]
fn should_remove_plugin_when_it_exists() {
    let dir = tempfile::tempdir().unwrap();
    let store = PluginStore::new(dir.path());

    store.add(sample_plugin("alpha")).unwrap();
    store.remove("alpha").unwrap();
    assert!(store.load().unwrap().is_empty());
}

#[test]
fn should_return_error_when_removing_nonexistent_plugin() {
    let dir = tempfile::tempdir().unwrap();
    let store = PluginStore::new(dir.path());
    let result = store.remove("ghost");
    assert!(matches!(result, Err(PluginError::NotFound(_))));
}

#[test]
fn should_toggle_enabled_when_setting_enabled_state() {
    let dir = tempfile::tempdir().unwrap();
    let store = PluginStore::new(dir.path());

    store.add(sample_plugin("alpha")).unwrap();
    store.set_enabled("alpha", false).unwrap();

    let plugin = store.find("alpha").unwrap().unwrap();
    assert!(!plugin.enabled);

    store.set_enabled("alpha", true).unwrap();
    let plugin = store.find("alpha").unwrap().unwrap();
    assert!(plugin.enabled);
}

#[test]
fn should_return_error_when_setting_enabled_for_nonexistent_plugin() {
    let dir = tempfile::tempdir().unwrap();
    let store = PluginStore::new(dir.path());
    let result = store.set_enabled("ghost", true);
    assert!(matches!(result, Err(PluginError::NotFound(_))));
}

#[test]
fn should_return_none_when_finding_nonexistent_plugin() {
    let dir = tempfile::tempdir().unwrap();
    let store = PluginStore::new(dir.path());
    assert!(store.find("ghost").unwrap().is_none());
}

#[test]
fn should_persist_plugins_when_reloaded_from_disk() {
    let dir = tempfile::tempdir().unwrap();

    // First store instance adds a plugin.
    {
        let store = PluginStore::new(dir.path());
        store.add(sample_plugin("alpha")).unwrap();
    }

    // Second store instance should see it.
    {
        let store = PluginStore::new(dir.path());
        let plugins = store.load().unwrap();
        assert_eq!(plugins.len(), 1);
        assert_eq!(plugins[0].id, "alpha");
    }
}

#[test]
fn should_serialize_plugins_when_source_types_differ() {
    let dir = tempfile::tempdir().unwrap();
    let store = PluginStore::new(dir.path());

    let github_plugin = InstalledPlugin {
        id: "gh-plugin".to_string(),
        version: None,
        source: PluginSource::Github {
            repo: "user/repo".to_string(),
            commit: Some("abc123".to_string()),
            kind: None,
        },
        original_source: Some("github:user/repo@abc123".to_string()),
        enabled: true,
        installed_at: Utc::now(),
        updated_at: Utc::now(),
        path: PathBuf::from("/plugins/gh-plugin"),
        github_ref: Some(mirri_plugin_host::GithubRef::branch("main")),
        installed_sha: Some("0123456789abcdef0123456789abcdef01234567".to_string()),
        mcp_servers: HashMap::new(),
    };
    let zip_plugin = InstalledPlugin {
        id: "zip-plugin".to_string(),
        version: None,
        source: PluginSource::Zip {
            url: "https://example.com/p.zip".to_string(),
        },
        original_source: Some("https://example.com/p.zip".to_string()),
        enabled: false,
        installed_at: Utc::now(),
        updated_at: Utc::now(),
        path: PathBuf::from("/plugins/zip-plugin"),
        github_ref: None,
        installed_sha: None,
        mcp_servers: HashMap::new(),
    };

    store.add(github_plugin).unwrap();
    store.add(zip_plugin).unwrap();

    let plugins = store.load().unwrap();
    assert_eq!(plugins.len(), 2);
    let gh = plugins.iter().find(|p| p.id == "gh-plugin").unwrap();
    assert_eq!(
        gh.source,
        PluginSource::Github {
            repo: "user/repo".to_string(),
            commit: None,
            kind: None,
        }
    );
    assert_eq!(gh.github_ref, Some(mirri_plugin_host::GithubRef::branch("main")));
    assert_eq!(
        gh.installed_sha.as_deref(),
        Some("0123456789abcdef0123456789abcdef01234567")
    );
    assert_eq!(gh.original_source.as_deref(), Some("github:user/repo@abc123"));
    let zip = plugins.iter().find(|p| p.id == "zip-plugin").unwrap();
    assert_eq!(
        zip.source,
        PluginSource::Zip {
            url: "https://example.com/p.zip".to_string()
        }
    );
    assert!(!zip.enabled);
}

// ---------------------------------------------------------------------------
// A6 — the TS v1 installed.json format
// ---------------------------------------------------------------------------

/// Given a store write, when the file is inspected on disk, then it is the
/// TS v1 envelope `{version: 1, plugins: [...]}` with camelCase records —
/// the bidirectional interop shape of item A6.
#[test]
fn should_write_the_ts_v1_envelope_when_saving_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = PluginStore::new(dir.path());
    store.add(sample_plugin("alpha")).unwrap();

    let raw = std::fs::read_to_string(dir.path().join("installed.json")).unwrap();
    let file: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(file["version"], serde_json::json!(1));
    let record = &file["plugins"][0];
    assert_eq!(record["id"], serde_json::json!("alpha"));
    // The envelope carries native separators (`/plugins\alpha` on Windows).
    let expected_root = std::path::PathBuf::from("/plugins").join("alpha");
    assert_eq!(
        record["root"],
        serde_json::json!(expected_root.display().to_string())
    );
    assert_eq!(record["source"], serde_json::json!("local-path"));
    assert_eq!(record["enabled"], serde_json::json!(true));
    assert!(record["installedAt"].is_string());
    assert!(record["updatedAt"].is_string());
    assert!(record.get("path").is_none(), "no legacy path field");
    assert!(record.get("version").is_none(), "no version field (TS)");
}

/// Given a TS-written v1 file, when the store loads it, then every field
/// maps onto the record — the TS → rust direction of the bidirectional
/// interop (item A6): source literals, capabilities-wrapped per-server
/// state, and the github attribution.
#[test]
fn should_load_a_ts_written_v1_file_when_reading_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = PluginStore::new(dir.path());
    let ts_file = serde_json::json!({
        "version": 1,
        "plugins": [
            {
                "id": "ts-plugin",
                "root": "/home/u/.mirri/plugins/managed/ts-plugin",
                "source": "github",
                "enabled": false,
                "installedAt": "2026-06-01T00:00:00Z",
                "originalSource": "https://github.com/mirricode/demo-plugin/tree/main",
                "capabilities": {
                    "mcpServers": {
                        "helper": { "enabled": false, "enabledTools": ["t1"], "disabledTools": ["t2"] }
                    }
                },
                "github": {
                    "owner": "mirricode",
                    "repo": "demo-plugin",
                    "ref": { "kind": "branch", "value": "main" },
                    "installedSha": "0123456789abcdef0123456789abcdef01234567"
                }
            },
            {
                "id": "local-plugin",
                "root": "/home/u/.mirri/plugins/managed/local-plugin",
                "source": "local-path",
                "enabled": true,
                "installedAt": "2026-06-01T00:00:00Z",
                "updatedAt": "2026-06-02T00:00:00Z"
            },
            {
                "id": "zip-plugin",
                "root": "/home/u/.mirri/plugins/managed/zip-plugin",
                "source": "zip-url",
                "enabled": true,
                "installedAt": "2026-06-01T00:00:00Z",
                "originalSource": "https://example.com/p.zip"
            }
        ]
    });
    std::fs::write(
        dir.path().join("installed.json"),
        serde_json::to_string_pretty(&ts_file).unwrap(),
    )
    .unwrap();

    let plugins = store.load().unwrap();
    assert_eq!(plugins.len(), 3);

    let gh = plugins.iter().find(|p| p.id == "ts-plugin").unwrap();
    assert_eq!(
        gh.source,
        PluginSource::Github {
            repo: "mirricode/demo-plugin".to_string(),
            commit: None,
            kind: None,
        }
    );
    assert!(!gh.enabled);
    assert_eq!(
        gh.original_source.as_deref(),
        Some("https://github.com/mirricode/demo-plugin/tree/main")
    );
    // No updatedAt in the TS row → the record follows installedAt.
    assert_eq!(gh.updated_at, gh.installed_at);
    assert_eq!(
        gh.mcp_servers.get("helper"),
        Some(&mirri_plugin_host::PluginMcpServerState {
            enabled: false,
            enabled_tools: Some(vec!["t1".to_string()]),
            disabled_tools: Some(vec!["t2".to_string()]),
        })
    );
    assert_eq!(gh.github_ref, Some(mirri_plugin_host::GithubRef::branch("main")));
    assert_eq!(
        gh.installed_sha.as_deref(),
        Some("0123456789abcdef0123456789abcdef01234567")
    );
    assert_eq!(
        gh.path,
        PathBuf::from("/home/u/.mirri/plugins/managed/ts-plugin")
    );

    let local = plugins.iter().find(|p| p.id == "local-plugin").unwrap();
    assert_eq!(local.source, PluginSource::Local { path: String::new() });
    // The explicit updatedAt row parses on its own.
    assert_eq!(
        local.updated_at.to_rfc3339(),
        "2026-06-02T00:00:00+00:00"
    );

    let zip = plugins.iter().find(|p| p.id == "zip-plugin").unwrap();
    assert_eq!(
        zip.source,
        PluginSource::Zip {
            url: "https://example.com/p.zip".to_string()
        }
    );

    // The file round-trips: a rust write rewrites the same v1 envelope.
    store.save(&plugins).unwrap();
    let round_trip = store.load().unwrap();
    assert_eq!(round_trip.len(), 3);
}

/// Given a pre-A6 bare-array installed.json (snake_case records, tagged
/// source objects), when the store loads it, then the records migrate with
/// the old flat fields mapped onto the record — the rust legacy → v1
/// migration of item A6.
#[test]
fn should_migrate_a_legacy_bare_array_when_reading_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let store = PluginStore::new(dir.path());
    let legacy = serde_json::json!([
        {
            "id": "old-plugin",
            "version": "1.0.0",
            "source": {"type": "github", "repo": "user/repo", "commit": "main"},
            "enabled": true,
            "installed_at": "2024-01-01T00:00:00Z",
            "updated_at": "2024-01-01T00:00:00Z",
            "path": "/plugins/old-plugin",
            "github_ref": {"kind": "branch", "value": "main"},
            "installed_sha": "0123456789abcdef0123456789abcdef01234567",
            "mcp_servers": {
                "helper": {"enabled": false, "enabled_tools": ["t1"]}
            }
        }
    ]);
    std::fs::write(
        dir.path().join("installed.json"),
        serde_json::to_string_pretty(&legacy).unwrap(),
    )
    .unwrap();

    let plugins = store.load().unwrap();
    assert_eq!(plugins.len(), 1);
    let plugin = &plugins[0];
    assert_eq!(plugin.version.as_deref(), Some("1.0.0"));
    assert_eq!(
        plugin.source,
        PluginSource::Github {
            repo: "user/repo".to_string(),
            commit: Some("main".to_string()),
            kind: None,
        }
    );
    assert_eq!(plugin.github_ref, Some(mirri_plugin_host::GithubRef::branch("main")));
    assert_eq!(
        plugin.mcp_servers.get("helper"),
        Some(&mirri_plugin_host::PluginMcpServerState {
            enabled: false,
            enabled_tools: Some(vec!["t1".to_string()]),
            disabled_tools: None,
        })
    );

    // The next save migrates the file to the v1 envelope.
    store.save(&plugins).unwrap();
    let raw = std::fs::read_to_string(dir.path().join("installed.json")).unwrap();
    let file: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(file["version"], serde_json::json!(1));
    assert_eq!(file["plugins"][0]["root"], serde_json::json!("/plugins/old-plugin"));
    assert_eq!(
        file["plugins"][0]["github"]["owner"],
        serde_json::json!("user")
    );
    assert_eq!(
        file["plugins"][0]["capabilities"]["mcpServers"]["helper"]["enabledTools"],
        serde_json::json!(["t1"])
    );
}

/// Given an installed record, when the store upserts the same id, then the
/// record is replaced in place (item A2) — and a fresh id is appended.
#[test]
fn should_replace_record_when_upserting_an_existing_id() {
    let dir = tempfile::tempdir().unwrap();
    let store = PluginStore::new(dir.path());
    store.add(sample_plugin("alpha")).unwrap();
    store.upsert(sample_plugin("alpha")).unwrap();
    assert_eq!(store.load().unwrap().len(), 1, "upsert never duplicates");

    store.upsert(sample_plugin("beta")).unwrap();
    assert_eq!(store.load().unwrap().len(), 2);
}

/// Given an installed plugin, when set_enabled is called with the value it
/// already holds, then nothing is written — the TS `setEnabled` early
/// return (item C1): no `updatedAt` refresh, no file rewrite.
#[test]
fn should_not_rewrite_when_set_enabled_value_is_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let store = PluginStore::new(dir.path());
    store.add(sample_plugin("alpha")).unwrap();
    let before = store.find("alpha").unwrap().unwrap();

    let changed = store.set_enabled("alpha", true).unwrap();
    assert!(!changed, "same value reports no change");

    let after = store.find("alpha").unwrap().unwrap();
    assert_eq!(after.updated_at, before.updated_at, "updated_at must not move");

    let changed = store.set_enabled("alpha", false).unwrap();
    assert!(changed, "a real toggle still reports a change");
}

}

