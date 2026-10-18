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
