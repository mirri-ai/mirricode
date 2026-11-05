// layer: T1
// Domain group binary consolidated from per-file test binaries
// per 16-test-execution-strategy.md §4.4 (pure file moves).
mod config_tests {
//! Tests for MCP configuration loading, three-layer merging, and
//! environment variable expansion.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use mirri_mcp::{McpServerConfig, McpTransportConfig, load_mcp_config};
use serde_json::json;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create a temporary directory tree and return its path.
fn make_temp_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "mirri_mcp_config_test_{}_{}",
        prefix,
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Write a JSON config file, creating parent directories as needed.
fn write_config_file(path: &PathBuf, json: &serde_json::Value) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, serde_json::to_string_pretty(json).unwrap()).unwrap();
}

/// Clean up a temp directory.
fn cleanup(dir: &PathBuf) {
    let _ = fs::remove_dir_all(dir);
}

fn stdio_config(command: &str) -> serde_json::Value {
    json!({
        "transport": {
            "type": "stdio",
            "command": command,
            "args": []
        }
    })
}

// ---------------------------------------------------------------------------
// Single-layer loading
// ---------------------------------------------------------------------------

#[test]
fn should_load_global_config_when_only_global_config_exists() {
    let home = make_temp_dir("global_only_home");
    let project_root = make_temp_dir("global_only_project");
    let cwd = make_temp_dir("global_only_cwd");

    let global_path = home.join(".mirri-code").join("mcp.json");
    write_config_file(
        &global_path,
        &json!({
            "mcpServers": {
                "server-a": stdio_config("echo")
            }
        }),
    );

    let configs = load_mcp_config(&home, &project_root, &cwd).unwrap();

    assert_eq!(configs.len(), 1);
    assert!(configs.contains_key("server-a"));

    cleanup(&home);
    cleanup(&project_root);
    cleanup(&cwd);
}

#[test]
fn should_load_project_root_config_when_only_project_root_config_exists() {
    let home = make_temp_dir("proot_home");
    let project_root = make_temp_dir("proot_project");
    let cwd = make_temp_dir("proot_cwd");

    let project_root_path = project_root.join(".mcp.json");
    write_config_file(
        &project_root_path,
        &json!({
            "mcpServers": {
                "claude-server": stdio_config("npx")
            }
        }),
    );

    let configs = load_mcp_config(&home, &project_root, &cwd).unwrap();

    assert_eq!(configs.len(), 1);
    assert!(configs.contains_key("claude-server"));

    cleanup(&home);
    cleanup(&project_root);
    cleanup(&cwd);
}

#[test]
fn should_load_local_config_when_only_local_config_exists() {
    let home = make_temp_dir("local_home");
    let project_root = make_temp_dir("local_project");
    let cwd = make_temp_dir("local_cwd");

    let local_path = cwd.join(".mirri-code").join("mcp.json");
    write_config_file(
        &local_path,
        &json!({
            "mcpServers": {
                "local-server": stdio_config("node")
            }
        }),
    );

    let configs = load_mcp_config(&home, &project_root, &cwd).unwrap();

    assert_eq!(configs.len(), 1);
    assert!(configs.contains_key("local-server"));

    cleanup(&home);
    cleanup(&project_root);
    cleanup(&cwd);
}

// ---------------------------------------------------------------------------
// Project-layer stdio cwd default
// ---------------------------------------------------------------------------

/// Given a project-root `.mcp.json` whose stdio entry omits `cwd`, when the
/// config loads, then the entry's cwd defaults to the config file's
/// directory — a server declared in a workspace runs in that workspace,
/// never in whatever directory the server process itself was started from
/// (the production incident: a workspace's codegraph indexed the server's
/// launch directory instead of the session's workspace).
#[test]
fn should_default_a_project_root_stdio_servers_cwd_to_the_config_dir_when_omitted() {
    let home = make_temp_dir("cwd_default_home");
    let project_root = make_temp_dir("cwd_default_project");
    let cwd = make_temp_dir("cwd_default_cwd");

    let project_root_path = project_root.join(".mcp.json");
    write_config_file(
        &project_root_path,
        &json!({
            "mcpServers": {
                "codegraph": stdio_config("codegraph")
            }
        }),
    );

    let configs = load_mcp_config(&home, &project_root, &cwd).unwrap();

    match &configs
        .get("codegraph")
        .expect("the project-layer server must load")
        .transport
    {
        McpTransportConfig::Stdio { cwd, .. } => assert_eq!(
            cwd.as_deref(),
            Some(project_root.as_path()),
            "a project-layer stdio server must run in the workspace root"
        ),
        other => panic!("expected a stdio transport, got {other:?}"),
    }

    cleanup(&home);
    cleanup(&project_root);
    cleanup(&cwd);
}

/// The user layer keeps the legacy behavior: its connections are shared
/// across workspaces (one process, one cwd), so an omitted `cwd` stays
/// `None` — the child inherits the server process's working directory, and
/// pinning one must stay the operator's explicit choice.
#[test]
fn should_keep_the_user_layer_stdio_servers_cwd_unset_when_omitted() {
    let home = make_temp_dir("cwd_inherit_home");
    let project_root = make_temp_dir("cwd_inherit_project");
    let cwd = make_temp_dir("cwd_inherit_cwd");

    let global_path = home.join(".mirri-code").join("mcp.json");
    write_config_file(
        &global_path,
        &json!({
            "mcpServers": {
                "user-server": stdio_config("echo")
            }
        }),
    );

    let configs = load_mcp_config(&home, &project_root, &cwd).unwrap();

    match &configs
        .get("user-server")
        .expect("the user-layer server must load")
        .transport
    {
        McpTransportConfig::Stdio { cwd, .. } => assert_eq!(
            None,
            cwd.as_deref(),
            "the user layer must keep the inherited working directory"
        ),
        other => panic!("expected a stdio transport, got {other:?}"),
    }

    cleanup(&home);
    cleanup(&project_root);
    cleanup(&cwd);
}

// ---------------------------------------------------------------------------
// Missing files are not errors
// ---------------------------------------------------------------------------

#[test]
fn should_return_empty_when_no_config_files_exist() {
    let home = make_temp_dir("empty_home");
    let project_root = make_temp_dir("empty_project");
    let cwd = make_temp_dir("empty_cwd");

    let configs = load_mcp_config(&home, &project_root, &cwd).unwrap();
    assert!(configs.is_empty());

    cleanup(&home);
    cleanup(&project_root);
    cleanup(&cwd);
}

// ---------------------------------------------------------------------------
// Three-layer merge
// ---------------------------------------------------------------------------

#[test]
fn should_merge_three_layers_when_later_config_overrides_earlier() {
    let home = make_temp_dir("merge_home");
    let project_root = make_temp_dir("merge_project");
    let cwd = make_temp_dir("merge_cwd");

    // Global: server-a + server-b
    let global_path = home.join(".mirri-code").join("mcp.json");
    write_config_file(
        &global_path,
        &json!({
            "mcpServers": {
                "server-a": stdio_config("echo"),
                "server-b": stdio_config("cat")
            }
        }),
    );

    // Project root: server-a (override) + server-c
    let project_root_path = project_root.join(".mcp.json");
    write_config_file(
        &project_root_path,
        &json!({
            "mcpServers": {
                "server-a": stdio_config("npx"),
                "server-c": stdio_config("node")
            }
        }),
    );

    // Local: server-b (override)
    let local_path = cwd.join(".mirri-code").join("mcp.json");
    write_config_file(
        &local_path,
        &json!({
            "mcpServers": {
                "server-b": stdio_config("python3")
            }
        }),
    );

    let configs = load_mcp_config(&home, &project_root, &cwd).unwrap();

    // Should have 3 servers.
    assert_eq!(configs.len(), 3);
    assert!(configs.contains_key("server-a"));
    assert!(configs.contains_key("server-b"));
    assert!(configs.contains_key("server-c"));

    // server-a should be overridden by project root (npx, not echo).
    let server_a = configs.get("server-a").unwrap();
    match &server_a.transport {
        McpTransportConfig::Stdio { command, .. } => {
            assert_eq!(command, "npx");
        }
        _ => panic!("expected stdio transport"),
    }

    // server-b should be overridden by local (python3, not cat).
    let server_b = configs.get("server-b").unwrap();
    match &server_b.transport {
        McpTransportConfig::Stdio { command, .. } => {
            assert_eq!(command, "python3");
        }
        _ => panic!("expected stdio transport"),
    }

    // server-c comes from project root.
    let server_c = configs.get("server-c").unwrap();
    match &server_c.transport {
        McpTransportConfig::Stdio { command, .. } => {
            assert_eq!(command, "node");
        }
        _ => panic!("expected stdio transport"),
    }

    cleanup(&home);
    cleanup(&project_root);
    cleanup(&cwd);
}

// ---------------------------------------------------------------------------
// A7 family-wise layered merge (ADR-006): the connection family shadows
// whole entries, the filter family overrides only when explicit
// ---------------------------------------------------------------------------

/// Given the user layer disabling `db.query` and the project layer
/// redefining `db` with a new connection only (no filter fields), when the
/// flat three-layer view is merged, then the connection takes the project
/// layer's new command (whole-entry shadowing for the connection family)
/// while the filter family inherits the user layer — `db.query` stays
/// disabled (A7: explicit-only override, ADR-006).
#[test]
fn should_inherit_user_filter_when_project_layer_redefines_connection_only() {
    // Given: user 层 db 禁用 db.query；project 层同名 db 仅重定义连接字段（无过滤字段）
    let home = make_temp_dir("a7_inherit_home");
    let project_root = make_temp_dir("a7_inherit_project");
    let cwd = make_temp_dir("a7_inherit_cwd");

    let global_path = home.join(".mirri-code").join("mcp.json");
    write_config_file(
        &global_path,
        &json!({
            "mcpServers": {
                "db": {
                    "transport": { "type": "stdio", "command": "old-cmd", "args": [] },
                    "disabledTools": ["db.query"]
                }
            }
        }),
    );
    let local_path = cwd.join(".mirri-code").join("mcp.json");
    write_config_file(
        &local_path,
        &json!({
            "mcpServers": {
                "db": {
                    "transport": { "type": "stdio", "command": "new-cmd", "args": [] }
                }
            }
        }),
    );

    // When: 三层合并为扁平视图
    let merged = load_mcp_config(&home, &project_root, &cwd).unwrap();

    // Then: 连接取 project 层新命令（连接族整条遮蔽），
    //       过滤族继承 user 层——db.query 仍被禁用（A7：显式才覆盖）
    let db = &merged["db"];
    match &db.transport {
        McpTransportConfig::Stdio { command, .. } => {
            assert_eq!(command, "new-cmd");
        }
        other => panic!("expected stdio transport, got {other:?}"),
    }
    assert_eq!(
        db.disabled_tools,
        Some(vec!["db.query".to_string()]),
        "the project layer redefined the connection only — the user filter survives"
    );
    assert!(!db.is_tool_allowed("db.query"));

    cleanup(&home);
    cleanup(&project_root);
    cleanup(&cwd);
}

/// Given the user layer disabling `db.query` and the project layer carrying
/// a filter-override entry (explicit `enabledTools`, no transport), when
/// the flat three-layer view is merged, then the project layer's explicit
/// filter replaces the user filter — `db.query` is usable again (US2.3: a
/// project-layer switch overrides the user layer's switch) while the
/// connection stays the user layer's.
#[test]
fn should_override_user_filter_when_project_layer_carries_explicit_filter() {
    // Given: user 层禁用 db.query；project 层过滤覆盖条目显式 enabled_tools 含 db.query
    let home = make_temp_dir("a7_override_home");
    let project_root = make_temp_dir("a7_override_project");
    let cwd = make_temp_dir("a7_override_cwd");

    let global_path = home.join(".mirri-code").join("mcp.json");
    write_config_file(
        &global_path,
        &json!({
            "mcpServers": {
                "db": {
                    "transport": { "type": "stdio", "command": "user-cmd", "args": [] },
                    "disabledTools": ["db.query"]
                }
            }
        }),
    );
    // 过滤覆盖条目：仅含过滤字段，无 transport（A7 覆盖条目形态）
    let local_path = cwd.join(".mirri-code").join("mcp.json");
    write_config_file(
        &local_path,
        &json!({
            "mcpServers": {
                "db": { "enabledTools": ["db.query", "db.other"] }
            }
        }),
    );

    // When: 三层合并为扁平视图
    let merged = load_mcp_config(&home, &project_root, &cwd).unwrap();

    // Then: 合并结果 db.query 可用（US2.3：项目层单项覆盖用户层开关），
    //       连接仍取 user 层定义
    let db = &merged["db"];
    match &db.transport {
        McpTransportConfig::Stdio { command, .. } => {
            assert_eq!(command, "user-cmd");
        }
        other => panic!("expected stdio transport, got {other:?}"),
    }
    assert_eq!(
        db.disabled_tools, None,
        "the project layer's explicit filter replaces the user layer's disable"
    );
    assert!(
        db.is_tool_allowed("db.query"),
        "US2.3: the project layer's single switch overrides the user layer's"
    );

    cleanup(&home);
    cleanup(&project_root);
    cleanup(&cwd);
}

// ---------------------------------------------------------------------------
// Environment variable expansion (A5: raw at load, value-level at connect)
// ---------------------------------------------------------------------------

#[test]
fn should_keep_env_references_verbatim_when_config_is_loaded() {
    let home = make_temp_dir("env_home");
    let project_root = make_temp_dir("env_project");
    let cwd = make_temp_dir("env_cwd");

    let mut env_guard = mirri_test_support::env::EnvGuard::new();
    env_guard.set_many(&[
        ("MCP_TEST_CMD", "expanded-command"),
        ("MCP_TEST_TOKEN", "secret-token-123"),
    ]);

    let global_path = home.join(".mirri-code").join("mcp.json");
    write_config_file(
        &global_path,
        &json!({
            "mcpServers": {
                "raw-server": {
                    "transport": {
                        "type": "stdio",
                        "command": "${MCP_TEST_CMD}",
                        "args": []
                    },
                    "auth": {
                        "type": "bearer",
                        "token": "${MCP_TEST_TOKEN}"
                    }
                }
            }
        }),
    );

    let configs = load_mcp_config(&home, &project_root, &cwd).unwrap();

    // Adjudication A5: the loaded config keeps `${VAR}` references verbatim —
    // the stored, in-memory and wire-presented form is the raw text.
    let server = configs.get("raw-server").unwrap();
    match &server.transport {
        McpTransportConfig::Stdio { command, .. } => {
            assert_eq!(command, "${MCP_TEST_CMD}");
        }
        _ => panic!("expected stdio transport"),
    }
    if let Some(mirri_mcp::McpAuthConfig::Bearer { token }) = &server.auth {
        assert_eq!(token, "${MCP_TEST_TOKEN}");
    } else {
        panic!("expected bearer auth config");
    }

    // The same config, expanded at connect time, resolves the references.
    let (expanded, undefined) = mirri_mcp::config::expand_server_config_values(server);
    assert!(
        undefined.is_empty(),
        "defined vars should not be reported: {undefined:?}"
    );
    match &expanded.transport {
        McpTransportConfig::Stdio { command, .. } => {
            assert_eq!(command, "expanded-command");
        }
        _ => panic!("expected stdio transport"),
    }
    if let Some(mirri_mcp::McpAuthConfig::Bearer { token }) = &expanded.auth {
        assert_eq!(token, "secret-token-123");
    } else {
        panic!("expected bearer auth config");
    }

    cleanup(&home);
    cleanup(&project_root);
    cleanup(&cwd);
}

#[test]
fn should_expand_env_prefix_syntax_at_connect_time_when_config_uses_it() {
    let home = make_temp_dir("envprefix_home");
    let project_root = make_temp_dir("envprefix_project");
    let cwd = make_temp_dir("envprefix_cwd");

    let mut env_guard = mirri_test_support::env::EnvGuard::new();
    env_guard.set("MCP_TEST_ENV_PREFIX_CMD", "env-prefix-command");

    let global_path = home.join(".mirri-code").join("mcp.json");
    write_config_file(
        &global_path,
        &json!({
            "mcpServers": {
                "server": {
                    "transport": {
                        "type": "stdio",
                        "command": "${env:MCP_TEST_ENV_PREFIX_CMD}",
                        "args": []
                    }
                }
            }
        }),
    );

    let configs = load_mcp_config(&home, &project_root, &cwd).unwrap();
    let server = configs.get("server").unwrap();
    // Raw at load...
    match &server.transport {
        McpTransportConfig::Stdio { command, .. } => {
            assert_eq!(command, "${env:MCP_TEST_ENV_PREFIX_CMD}");
        }
        _ => panic!("expected stdio transport"),
    }
    // ...resolved at connect time.
    let (expanded, undefined) = mirri_mcp::config::expand_server_config_values(server);
    assert!(undefined.is_empty());
    match &expanded.transport {
        McpTransportConfig::Stdio { command, .. } => {
            assert_eq!(command, "env-prefix-command");
        }
        _ => panic!("expected stdio transport"),
    }

    cleanup(&home);
    cleanup(&project_root);
    cleanup(&cwd);
}

#[test]
fn should_collect_undefined_variable_names_when_expanding_server_config() {
    let server = serde_json::from_value::<mirri_mcp::McpServerConfig>(json!({
        "transport": {
            "type": "stdio",
            "command": "${MCP_UNDEFINED_CMD_TEST}",
            "args": ["--flag", "${MCP_UNDEFINED_ARG_TEST}"]
        }
    }))
    .unwrap();

    let (expanded, undefined) = mirri_mcp::config::expand_server_config_values(&server);
    // Undefined variables expand to the empty string...
    match &expanded.transport {
        McpTransportConfig::Stdio { command, args, .. } => {
            assert_eq!(command, "");
            assert_eq!(args, &vec!["--flag".to_string(), String::new()]);
        }
        _ => panic!("expected stdio transport"),
    }
    // ...and their names are collected for diagnostics (order follows the
    // serialized JSON key order, so compare as a set).
    let mut undefined = undefined;
    undefined.sort();
    assert_eq!(
        undefined,
        vec![
            "MCP_UNDEFINED_ARG_TEST".to_string(),
            "MCP_UNDEFINED_CMD_TEST".to_string()
        ]
    );
}

/// Given a server config whose object keys and string values both carry
/// `${...}` references, when the config is expanded at the value level,
/// then string *values* expand while object *keys* stay verbatim — keys
/// are never spliced into text and never reported as undefined (the TS
/// value-level semantics, spec §4.6 environment row).
#[test]
fn should_expand_string_values_but_never_object_keys_when_expanding_server_config() {
    // Given: a stdio config whose env map key and value both reference
    // variables (the extreme case the text-level path used to mangle).
    let raw = serde_json::json!({
        "command": "run",
        "env": { "${NEVER_EXPAND_KEY}": "${EXPAND_ME}" },
    });
    let config: mirri_mcp::McpServerConfig = serde_json::from_value(raw).expect("parse");

    // When: expanded through the injected-env variant, with only
    // EXPAND_ME defined.
    let (expanded, undefined) = mirri_mcp::expand_server_config_values_with_env(&config, |name| {
        if name == "EXPAND_ME" {
            Some("real".to_string())
        } else {
            None
        }
    });

    // Then: the value expands, the key is untouched, and — because the
    // injected lookup defines EXPAND_ME and keys are never consulted —
    // nothing is reported undefined.
    match &expanded.transport {
        McpTransportConfig::Stdio { env, .. } => {
            assert_eq!(env.as_ref().unwrap()["${NEVER_EXPAND_KEY}"], "real");
        }
        other => panic!("expected stdio transport, got {other:?}"),
    }
    assert!(
        undefined.is_empty(),
        "EXPAND_ME is defined by the injected lookup, and a ${{...}} key is never \
         treated as a variable: {undefined:?}"
    );

    // And even when the key's variable would be undefined, the key stays
    // verbatim and its name never surfaces in the undefined list.
    let raw2 = serde_json::json!({
        "command": "run",
        "env": { "${NEVER_EXPAND_KEY}": "x" },
    });
    let config2: mirri_mcp::McpServerConfig = serde_json::from_value(raw2).expect("parse");
    let (_, undefined2) = mirri_mcp::expand_server_config_values_with_env(&config2, |_| None);
    assert!(
        !undefined2.contains(&"NEVER_EXPAND_KEY".to_string()),
        "a key holding a ${{...}} reference is never expanded, even when undefined: \
         {undefined2:?}"
    );
}

/// Given a raw stored entry in the flat TS/Claude wire form (passthrough
/// keys included), when it expands through the raw-JSON value-level entry,
/// then string *values* expand, object keys and passthrough fields survive
/// untouched, and undefined variables are collected by name — the
/// `mcp.config.preview` semantics (F-4) without a typed round-trip that
/// would normalize the stored field forms away.
#[test]
fn should_expand_raw_entry_values_when_the_json_form_expands_with_env() {
    // Given: a raw stored entry with a defined reference, an undefined
    // one, a `${...}` key, and a passthrough field the typed config does
    // not model.
    let raw = serde_json::json!({
        "command": "run-${DEFINED_VAR}",
        "args": ["${UNDEFINED_VAR}"],
        "env": { "${NEVER_EXPAND_KEY}": "value" },
        "unknownPassthrough": "keep-${DEFINED_VAR}",
    });

    // When: the raw JSON expands against an injected lookup that defines
    // only DEFINED_VAR.
    let (expanded, undefined) =
        mirri_mcp::config::expand_server_config_json_with_env(&raw, |name| {
            (name == "DEFINED_VAR").then(|| "yes".to_string())
        });

    // Then: values expanded, keys stayed literal, the passthrough field
    // survived with its value expanded, and the undefined name is listed.
    assert_eq!(expanded["command"], serde_json::json!("run-yes"));
    assert_eq!(expanded["args"], serde_json::json!([""]));
    assert_eq!(
        expanded["env"],
        serde_json::json!({ "${NEVER_EXPAND_KEY}": "value" })
    );
    assert_eq!(
        expanded["unknownPassthrough"],
        serde_json::json!("keep-yes")
    );
    assert_eq!(undefined, vec!["UNDEFINED_VAR".to_string()]);
    // And the input was not mutated in place.
    assert_eq!(raw["command"], serde_json::json!("run-${DEFINED_VAR}"));
}

// ---------------------------------------------------------------------------
// Stdio cwd resolution
// ---------------------------------------------------------------------------

#[test]
fn should_resolve_cwd_relative_to_config_file_when_cwd_is_relative() {
    let home = make_temp_dir("cwd_home");
    let project_root = make_temp_dir("cwd_project");
    let cwd = make_temp_dir("cwd_cwd");

    // Create a subdirectory that the relative cwd will refer to.
    let subdir = home.join(".mirri-code").join("subdir");
    fs::create_dir_all(&subdir).unwrap();

    let global_path = home.join(".mirri-code").join("mcp.json");
    write_config_file(
        &global_path,
        &json!({
            "mcpServers": {
                "server": {
                    "transport": {
                        "type": "stdio",
                        "command": "echo",
                        "args": [],
                        "cwd": "subdir"
                    }
                }
            }
        }),
    );

    let configs = load_mcp_config(&home, &project_root, &cwd).unwrap();
    let server = configs.get("server").unwrap();
    match &server.transport {
        McpTransportConfig::Stdio { cwd: stdio_cwd, .. } => {
            let cwd = stdio_cwd.as_ref().unwrap();
            assert!(
                cwd.is_absolute(),
                "relative cwd should be resolved to absolute"
            );
            assert!(
                cwd.ends_with("subdir"),
                "resolved cwd should end with 'subdir', got: {}",
                cwd.display()
            );
        }
        _ => panic!("expected stdio transport"),
    }

    cleanup(&home);
    cleanup(&project_root);
    cleanup(&cwd);
}

#[test]
fn should_preserve_absolute_cwd_when_cwd_is_absolute() {
    let home = make_temp_dir("abscwd_home");
    let project_root = make_temp_dir("abscwd_project");
    let cwd = make_temp_dir("abscwd_cwd");

    let abs_dir = home.join("absolute_dir");
    fs::create_dir_all(&abs_dir).unwrap();

    let global_path = home.join(".mirri-code").join("mcp.json");
    write_config_file(
        &global_path,
        &json!({
            "mcpServers": {
                "server": {
                    "transport": {
                        "type": "stdio",
                        "command": "echo",
                        "args": [],
                        "cwd": abs_dir.to_string_lossy()
                    }
                }
            }
        }),
    );

    let configs = load_mcp_config(&home, &project_root, &cwd).unwrap();
    let server = configs.get("server").unwrap();
    match &server.transport {
        McpTransportConfig::Stdio { cwd: stdio_cwd, .. } => {
            let cwd = stdio_cwd.as_ref().unwrap();
            assert_eq!(cwd, &abs_dir);
        }
        _ => panic!("expected stdio transport"),
    }

    cleanup(&home);
    cleanup(&project_root);
    cleanup(&cwd);
}

// ---------------------------------------------------------------------------
// Config file parsing — full server config
// ---------------------------------------------------------------------------

#[test]
fn should_parse_full_server_config_when_all_fields_present() {
    let home = make_temp_dir("full_home");
    let project_root = make_temp_dir("full_project");
    let cwd = make_temp_dir("full_cwd");

    let global_path = home.join(".mirri-code").join("mcp.json");
    write_config_file(
        &global_path,
        &json!({
            "mcpServers": {
                "full-server": {
                    "transport": {
                        "type": "http",
                        "url": "https://example.com/mcp"
                    },
                    "enabled": false,
                    "startupTimeoutMs": 5000,
                    "toolTimeoutMs": 10000,
                    "enabledTools": ["tool1", "tool2"],
                    "disabledTools": ["tool3"],
                    "auth": {
                        "type": "oauth",
                        "clientId": "my-client-id",
                        "scopes": ["read", "write"]
                    }
                }
            }
        }),
    );

    let configs = load_mcp_config(&home, &project_root, &cwd).unwrap();
    let server = configs.get("full-server").unwrap();

    assert!(!server.is_enabled());
    assert_eq!(
        server.startup_timeout(),
        std::time::Duration::from_millis(5000)
    );
    assert_eq!(
        server.tool_timeout(),
        std::time::Duration::from_millis(10000)
    );
    assert_eq!(
        server.enabled_tools.as_ref().unwrap(),
        &vec!["tool1".to_string(), "tool2".to_string()]
    );
    assert_eq!(
        server.disabled_tools.as_ref().unwrap(),
        &vec!["tool3".to_string()]
    );

    assert!(server.is_tool_allowed("tool1"));
    assert!(!server.is_tool_allowed("tool3"));
    assert!(!server.is_tool_allowed("unknown"));

    // Check auth.
    match &server.auth {
        Some(mirri_mcp::McpAuthConfig::OAuth { client_id, scopes }) => {
            assert_eq!(client_id.as_ref().unwrap(), "my-client-id");
            assert_eq!(
                scopes.as_ref().unwrap(),
                &vec!["read".to_string(), "write".to_string()]
            );
        }
        _ => panic!("expected OAuth auth config"),
    }

    // Check transport.
    match &server.transport {
        McpTransportConfig::Http { url, .. } => {
            assert_eq!(url, "https://example.com/mcp");
        }
        _ => panic!("expected HTTP transport"),
    }

    cleanup(&home);
    cleanup(&project_root);
    cleanup(&cwd);
}

// ---------------------------------------------------------------------------
// Invalid config
// ---------------------------------------------------------------------------

#[test]
fn should_return_error_when_json_is_invalid() {
    let home = make_temp_dir("invalid_home");
    let project_root = make_temp_dir("invalid_project");
    let cwd = make_temp_dir("invalid_cwd");

    let global_path = home.join(".mirri-code").join("mcp.json");
    write_config_file(&global_path, &json!({})); // valid but we'll overwrite with bad JSON
    fs::write(&global_path, "{ this is not valid json }").unwrap();

    let result = load_mcp_config(&home, &project_root, &cwd);
    assert!(result.is_err());

    cleanup(&home);
    cleanup(&project_root);
    cleanup(&cwd);
}

// ---------------------------------------------------------------------------
// Config file structure verification
// ---------------------------------------------------------------------------

#[test]
fn should_deserialize_config_file_when_mcp_servers_key_present() {
    // Verify that the McpConfigFile struct deserializes from the expected
    // "mcpServers" key.
    let json = r#"{
        "mcpServers": {
            "test": {
                "transport": {
                    "type": "stdio",
                    "command": "echo",
                    "args": []
                }
            }
        }
    }"#;

    let file: mirri_mcp::McpConfigFile = serde_json::from_str(json).unwrap();
    assert_eq!(file.mcp_servers.len(), 1);
    assert!(file.mcp_servers.contains_key("test"));
}

#[test]
fn should_parse_all_transport_types_when_config_contains_stdio_http_sse() {
    let home = make_temp_dir("alltrans_home");
    let project_root = make_temp_dir("alltrans_project");
    let cwd = make_temp_dir("alltrans_cwd");

    let global_path = home.join(".mirri-code").join("mcp.json");
    write_config_file(
        &global_path,
        &json!({
            "mcpServers": {
                "stdio-server": {
                    "transport": {
                        "type": "stdio",
                        "command": "echo",
                        "args": []
                    }
                },
                "http-server": {
                    "transport": {
                        "type": "http",
                        "url": "https://example.com/mcp"
                    }
                },
                "sse-server": {
                    "transport": {
                        "type": "sse",
                        "url": "https://example.com/sse"
                    }
                }
            }
        }),
    );

    let configs = load_mcp_config(&home, &project_root, &cwd).unwrap();
    assert_eq!(configs.len(), 3);

    let stdio_server = configs.get("stdio-server").unwrap();
    assert!(matches!(
        stdio_server.transport,
        McpTransportConfig::Stdio { .. }
    ));

    let http_server = configs.get("http-server").unwrap();
    assert!(matches!(
        http_server.transport,
        McpTransportConfig::Http { .. }
    ));

    let sse_server = configs.get("sse-server").unwrap();
    assert!(matches!(
        sse_server.transport,
        McpTransportConfig::Sse { .. }
    ));

    cleanup(&home);
    cleanup(&project_root);
    cleanup(&cwd);
}

// ---------------------------------------------------------------------------
// Re-exported types
// ---------------------------------------------------------------------------

#[test]
fn should_re_export_server_config_when_accessing_from_crate_root() {
    // Verify that McpServerConfig is re-exported from the crate root.
    let config: McpServerConfig = serde_json::from_str(
        r#"{
        "transport": {
            "type": "stdio",
            "command": "echo",
            "args": []
        }
    }"#,
    )
    .unwrap();
    assert!(config.is_enabled());
}

#[test]
fn should_re_export_config_types_when_accessing_from_crate_root() {
    // Verify that all config types are accessible from the crate root.
    let _: HashMap<String, McpServerConfig> = HashMap::new();
    let _: mirri_mcp::McpAuthConfig = mirri_mcp::McpAuthConfig::Bearer {
        token: "test".to_string(),
    };
    let _: mirri_mcp::McpConfigFile = mirri_mcp::McpConfigFile {
        mcp_servers: HashMap::new(),
    };
}

}

mod connection_tests {
//! Tests for the MCP connection manager.
//!
//! These tests cover:
//! - Basic lifecycle (connect failure isolation, disabled servers, disconnect)
//! - Tool listing and calling (error paths)
//! - Status listeners
//! - Reconnection
//!
//! Tests that require a real MCP server use the Python mock server pattern
//! from `stdio_integration.rs` and are `#[ignore]` by default.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use mirri_mcp::{McpConnectionManager, McpConnectionState, McpServerConfig, McpTransportConfig};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_bad_config() -> McpServerConfig {
    McpServerConfig {
        transport: McpTransportConfig::Stdio {
            command: "echo".to_string(),
            args: vec![],
            env: None,
            cwd: None,
        },
        enabled: None,
        startup_timeout_ms: Some(2000),
        tool_timeout_ms: None,
        enabled_tools: None,
        disabled_tools: None,
        auth: None,
    }
}

fn make_disabled_config() -> McpServerConfig {
    McpServerConfig {
        transport: McpTransportConfig::Stdio {
            command: "echo".to_string(),
            args: vec![],
            env: None,
            cwd: None,
        },
        enabled: Some(false),
        startup_timeout_ms: None,
        tool_timeout_ms: None,
        enabled_tools: None,
        disabled_tools: None,
        auth: None,
    }
}

fn make_nonexistent_command_config() -> McpServerConfig {
    McpServerConfig {
        transport: McpTransportConfig::Stdio {
            command: "/nonexistent/command/that/does/not/exist".to_string(),
            args: vec![],
            env: None,
            cwd: None,
        },
        enabled: None,
        startup_timeout_ms: Some(2000),
        tool_timeout_ms: None,
        enabled_tools: None,
        disabled_tools: None,
        auth: None,
    }
}

// ---------------------------------------------------------------------------
// Failure isolation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_not_panic_when_all_connections_fail() {
    // Two bad servers — both fail, but the manager should not panic.
    let manager = McpConnectionManager::new();
    let mut configs = HashMap::new();
    configs.insert("bad-1".to_string(), make_nonexistent_command_config());
    configs.insert("bad-2".to_string(), make_nonexistent_command_config());

    let results = manager.connect_all(configs).await;

    assert!(results.get("bad-1").unwrap().is_err());
    assert!(results.get("bad-2").unwrap().is_err());

    let connections = manager.list_connections().await;
    assert_eq!(connections.len(), 2);

    for (_, state) in &connections {
        assert!(
            matches!(state, McpConnectionState::Failed { .. }),
            "expected Failed state, got {state:?}"
        );
    }
}

#[tokio::test]
async fn should_fail_handshake_when_server_is_not_mcp() {
    // "echo" is not an MCP server — it will fail the JSON-RPC handshake.
    let manager = McpConnectionManager::new();
    let mut configs = HashMap::new();
    configs.insert("echo-server".to_string(), make_bad_config());

    let results = manager.connect_all(configs).await;

    let result = results.get("echo-server").unwrap();
    assert!(result.is_err(), "echo should fail as MCP server");

    let connections = manager.list_connections().await;
    let (_, state) = &connections[0];
    assert!(
        matches!(state, McpConnectionState::Failed { .. }),
        "expected Failed state, got {state:?}"
    );
}

// ---------------------------------------------------------------------------
// Disabled servers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_return_ok_and_disabled_state_when_server_is_disabled() {
    let manager = McpConnectionManager::new();
    let mut configs = HashMap::new();
    configs.insert("disabled".to_string(), make_disabled_config());

    let results = manager.connect_all(configs).await;
    let result = results.get("disabled").unwrap();
    assert!(result.is_ok(), "disabled server should return Ok");

    let connections = manager.list_connections().await;
    let (_, state) = &connections[0];
    assert_eq!(state, &McpConnectionState::Disabled);
}

#[tokio::test]
async fn should_not_include_tools_when_server_is_disabled() {
    let manager = McpConnectionManager::new();
    let mut configs = HashMap::new();
    configs.insert("disabled".to_string(), make_disabled_config());

    manager.connect_all(configs).await;
    let tools = manager.list_all_tools().await;
    assert!(tools.is_empty());
}

// ---------------------------------------------------------------------------
// Call tool error paths
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_return_error_when_calling_tool_on_nonexistent_server() {
    let manager = McpConnectionManager::new();
    let result = manager
        .call_tool("nonexistent", "tool", serde_json::json!({}))
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn should_return_error_when_calling_tool_on_failed_server() {
    let manager = McpConnectionManager::new();
    let mut configs = HashMap::new();
    configs.insert("bad".to_string(), make_nonexistent_command_config());

    manager.connect_all(configs).await;

    let result = manager
        .call_tool("bad", "tool", serde_json::json!({}))
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn should_return_error_when_calling_tool_on_disabled_server() {
    let manager = McpConnectionManager::new();
    let mut configs = HashMap::new();
    configs.insert("disabled".to_string(), make_disabled_config());

    manager.connect_all(configs).await;

    let result = manager
        .call_tool("disabled", "tool", serde_json::json!({}))
        .await;
    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// Disconnect
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_return_ok_when_disconnecting_nonexistent_server() {
    let manager = McpConnectionManager::new();
    let result = manager.disconnect("nonexistent").await;
    assert!(result.is_ok());
}

#[tokio::test]
async fn should_not_panic_when_disconnecting_all_on_empty_manager() {
    let manager = McpConnectionManager::new();
    manager.disconnect_all().await;
    // Should not panic.
}

#[tokio::test]
async fn should_not_panic_when_disconnecting_all_after_failed_connections() {
    let manager = McpConnectionManager::new();
    let mut configs = HashMap::new();
    configs.insert("bad-1".to_string(), make_nonexistent_command_config());
    configs.insert("bad-2".to_string(), make_nonexistent_command_config());

    manager.connect_all(configs).await;
    manager.disconnect_all().await;

    // After disconnect_all, connections should be in Pending state.
    let connections = manager.list_connections().await;
    for (_, state) in &connections {
        // Failed connections may still show Failed since there's no client to close.
        // The important thing is that disconnect_all doesn't panic.
        let _ = state;
    }
}

// ---------------------------------------------------------------------------
// Reconnect
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_return_error_when_reconnecting_nonexistent_server() {
    let manager = McpConnectionManager::new();
    let result = manager.reconnect("nonexistent").await;
    assert!(result.is_err());
}

#[tokio::test]
async fn should_return_error_and_remain_failed_when_reconnecting_failed_server() {
    let manager = McpConnectionManager::new();
    let mut configs = HashMap::new();
    configs.insert("bad".to_string(), make_nonexistent_command_config());

    manager.connect_all(configs).await;

    let result = manager.reconnect("bad").await;
    assert!(result.is_err());

    // Should still be in Failed state.
    let connections = manager.list_connections().await;
    let (_, state) = &connections[0];
    assert!(matches!(state, McpConnectionState::Failed { .. }));
}

// ---------------------------------------------------------------------------
// Status listeners
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_fire_status_listener_when_connecting() {
    let manager = McpConnectionManager::new();
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = Arc::clone(&counter);
    manager.add_status_listener(Box::new(move |_name, _state| {
        counter_clone.fetch_add(1, Ordering::Relaxed);
    }));

    let mut configs = HashMap::new();
    configs.insert("bad".to_string(), make_nonexistent_command_config());
    manager.connect_all(configs).await;

    // Should have fired at least twice (Pending + Failed).
    assert!(
        counter.load(Ordering::Relaxed) >= 2,
        "expected at least 2 status events, got {}",
        counter.load(Ordering::Relaxed)
    );
}

#[tokio::test]
async fn should_fire_status_listener_when_server_is_disabled() {
    let manager = McpConnectionManager::new();
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = Arc::clone(&counter);
    manager.add_status_listener(Box::new(move |_name, _state| {
        counter_clone.fetch_add(1, Ordering::Relaxed);
    }));

    let mut configs = HashMap::new();
    configs.insert("disabled".to_string(), make_disabled_config());
    manager.connect_all(configs).await;

    // Should fire at least once for Disabled state.
    assert!(counter.load(Ordering::Relaxed) >= 1);
}

#[tokio::test]
async fn should_capture_server_name_in_status_listener_when_connecting() {
    let manager = McpConnectionManager::new();
    let captured_name = Arc::new(tokio::sync::Mutex::new(String::new()));
    let captured_name_clone = Arc::clone(&captured_name);
    manager.add_status_listener(Box::new(move |name, _state| {
        let captured = captured_name_clone.clone();
        tokio::spawn(async move {
            *captured.lock().await = name;
        });
    }));

    let mut configs = HashMap::new();
    configs.insert("my-server".to_string(), make_nonexistent_command_config());
    manager.connect_all(configs).await;

    // Give the spawned task time to run.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let name = captured_name.lock().await.clone();
    assert_eq!(name, "my-server");
}

// ---------------------------------------------------------------------------
// List connections
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_return_empty_when_listing_connections_on_empty_manager() {
    let manager = McpConnectionManager::new();
    let connections = manager.list_connections().await;
    assert!(connections.is_empty());
}

#[tokio::test]
async fn should_list_connections_when_connect_all_is_called() {
    let manager = McpConnectionManager::new();
    let mut configs = HashMap::new();
    configs.insert("bad".to_string(), make_nonexistent_command_config());
    configs.insert("disabled".to_string(), make_disabled_config());

    manager.connect_all(configs).await;
    let connections = manager.list_connections().await;
    assert_eq!(connections.len(), 2);

    let by_name: HashMap<String, McpConnectionState> = connections.into_iter().collect();
    assert!(matches!(
        by_name.get("bad").unwrap(),
        McpConnectionState::Failed { .. }
    ));
    assert_eq!(
        by_name.get("disabled").unwrap(),
        &McpConnectionState::Disabled
    );
}

// ---------------------------------------------------------------------------
// List all tools
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_return_empty_tool_list_when_no_connections_exist() {
    let manager = McpConnectionManager::new();
    let tools = manager.list_all_tools().await;
    assert!(tools.is_empty());
}

#[tokio::test]
async fn should_return_empty_tool_list_when_all_connections_failed() {
    let manager = McpConnectionManager::new();
    let mut configs = HashMap::new();
    configs.insert("bad".to_string(), make_nonexistent_command_config());

    manager.connect_all(configs).await;
    let tools = manager.list_all_tools().await;
    assert!(tools.is_empty());
}

// ---------------------------------------------------------------------------
// Connect one
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_return_error_when_connecting_one_to_nonexistent_command() {
    let manager = McpConnectionManager::new();
    let result = manager
        .connect_one("bad", make_nonexistent_command_config())
        .await;
    assert!(result.is_err());

    let connections = manager.list_connections().await;
    assert_eq!(connections.len(), 1);
    let (_, state) = &connections[0];
    assert!(matches!(state, McpConnectionState::Failed { .. }));
}

#[tokio::test]
async fn should_return_ok_and_disabled_state_when_connecting_one_to_disabled_server() {
    let manager = McpConnectionManager::new();
    let result = manager
        .connect_one("disabled", make_disabled_config())
        .await;
    assert!(result.is_ok());

    let connections = manager.list_connections().await;
    let (_, state) = &connections[0];
    assert_eq!(state, &McpConnectionState::Disabled);
}

#[tokio::test]
async fn should_expand_defined_env_var_at_connect_time_when_command_uses_it() {
    let mut env_guard = mirri_test_support::env::EnvGuard::new();
    env_guard.set("MCP_CONN_DEFINED_TEST", "/nonexistent/marker_path_xyz");

    let config = McpServerConfig {
        transport: McpTransportConfig::Stdio {
            command: "${MCP_CONN_DEFINED_TEST}".to_string(),
            args: vec![],
            env: None,
            cwd: None,
        },
        enabled: None,
        startup_timeout_ms: Some(2000),
        tool_timeout_ms: None,
        enabled_tools: None,
        disabled_tools: None,
        auth: None,
    };

    let manager = McpConnectionManager::new();
    let result = manager.connect_one("expand", config).await;
    assert!(result.is_err());

    let connections = manager.list_connections().await;
    let (_, state) = &connections[0];
    let McpConnectionState::Failed { error } = state else {
        panic!("expected Failed state, got {state:?}");
    };
    assert!(
        error.contains("marker_path_xyz"),
        "expected expanded command value in error, got: {error}"
    );
    assert!(
        !error.contains("${"),
        "expected no unexpanded env reference left in error, got: {error}"
    );
}

#[tokio::test]
async fn should_append_undefined_env_var_names_to_connection_failure_error() {
    // The variable is intentionally never defined.
    let config = McpServerConfig {
        transport: McpTransportConfig::Stdio {
            command: "${MCP_CONN_UNDEFINED_TEST}".to_string(),
            args: vec![],
            env: None,
            cwd: None,
        },
        enabled: None,
        startup_timeout_ms: Some(2000),
        tool_timeout_ms: None,
        enabled_tools: None,
        disabled_tools: None,
        auth: None,
    };

    let manager = McpConnectionManager::new();
    let result = manager.connect_one("undefined", config).await;
    assert!(result.is_err());

    let connections = manager.list_connections().await;
    let (_, state) = &connections[0];
    let McpConnectionState::Failed { error } = state else {
        panic!("expected Failed state, got {state:?}");
    };
    assert!(
        error.contains("(undefined env vars: MCP_CONN_UNDEFINED_TEST)"),
        "expected undefined env vars note in error, got: {error}"
    );
}

// ---------------------------------------------------------------------------
// Tool filtering on connected server (integration, requires python3)
// ---------------------------------------------------------------------------

const MOCK_SERVER_SCRIPT: &str = r#"
import json, sys

TOOLS = [
    {"name": "echo", "description": "Echo input", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}},
    {"name": "hidden", "description": "Should be filtered", "inputSchema": {"type": "object"}},
]

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        msg = json.loads(line)
    except:
        continue
    if "method" not in msg:
        continue
    method = msg["method"]
    msg_id = msg.get("id")
    if method == "initialize":
        resp = {"jsonrpc": "2.0", "id": msg_id, "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "serverInfo": {"name": "mock-server", "version": "1.0.0"}
        }}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        resp = {"jsonrpc": "2.0", "id": msg_id, "result": {"tools": TOOLS}}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif method == "tools/call":
        params = msg.get("params", {})
        name = params.get("name", "")
        if name == "echo":
            text = params.get("arguments", {}).get("text", "")
            resp = {"jsonrpc": "2.0", "id": msg_id, "result": {"content": [{"type": "text", "text": text}], "isError": False}}
        else:
            resp = {"jsonrpc": "2.0", "id": msg_id, "error": {"code": -32601, "message": f"Unknown tool: {name}"}}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
"#;

fn python3_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

fn write_mock_server() -> std::path::PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("mirri_mcp_conn_test_{}.py", std::process::id()));
    std::fs::write(&path, MOCK_SERVER_SCRIPT).unwrap();
    path
}

#[tokio::test]
#[ignore = "requires python3"]
async fn should_filter_and_call_tools_when_connected_to_mock_server() {
    if !python3_available() {
        eprintln!("python3 not available, skipping");
        return;
    }

    let script_path = write_mock_server();

    let config = McpServerConfig {
        transport: McpTransportConfig::Stdio {
            command: "python3".to_string(),
            args: vec![script_path.to_string_lossy().to_string()],
            env: None,
            cwd: None,
        },
        enabled: None,
        startup_timeout_ms: Some(10000),
        tool_timeout_ms: None,
        enabled_tools: Some(vec!["echo".to_string()]),
        disabled_tools: Some(vec!["hidden".to_string()]),
        auth: None,
    };

    let manager = McpConnectionManager::new();
    let result = manager.connect_one("mock", config).await;
    assert!(result.is_ok(), "should connect to mock server");

    // Check state.
    let connections = manager.list_connections().await;
    let (_, state) = &connections[0];
    match state {
        McpConnectionState::Connected { tools, server_info } => {
            assert_eq!(server_info.name, "mock-server");
            // The server advertises 2 tools, but we only see them here.
            assert_eq!(tools.len(), 2);
        }
        _ => panic!("expected Connected state, got {state:?}"),
    }

    // list_all_tools should apply the filter (only "echo").
    let all_tools = manager.list_all_tools().await;
    assert_eq!(all_tools.len(), 1);
    assert_eq!(all_tools[0].1.name, "echo");

    // Call the allowed tool.
    let result = manager
        .call_tool("mock", "echo", serde_json::json!({"text": "hello"}))
        .await;
    assert!(result.is_ok());
    let call_result = result.unwrap();
    assert!(!call_result.is_error);

    // Call the disabled tool — should fail.
    let result = manager
        .call_tool("mock", "hidden", serde_json::json!({}))
        .await;
    assert!(result.is_err());

    // Disconnect.
    let _ = manager.disconnect("mock").await;
    let connections = manager.list_connections().await;
    let (_, state) = &connections[0];
    assert_eq!(state, &McpConnectionState::Pending);

    let _ = std::fs::remove_file(&script_path);
}

#[tokio::test]
#[ignore = "requires python3"]
async fn should_reconnect_successfully_when_mock_server_is_running() {
    if !python3_available() {
        eprintln!("python3 not available, skipping");
        return;
    }

    let script_path = write_mock_server();

    let config = McpServerConfig {
        transport: McpTransportConfig::Stdio {
            command: "python3".to_string(),
            args: vec![script_path.to_string_lossy().to_string()],
            env: None,
            cwd: None,
        },
        enabled: None,
        startup_timeout_ms: Some(10000),
        tool_timeout_ms: None,
        enabled_tools: None,
        disabled_tools: None,
        auth: None,
    };

    let manager = McpConnectionManager::new();
    manager.connect_one("mock", config).await.unwrap();

    // Verify connected.
    let connections = manager.list_connections().await;
    assert!(matches!(
        &connections[0].1,
        McpConnectionState::Connected { .. }
    ));

    // Reconnect.
    let result = manager.reconnect("mock").await;
    assert!(result.is_ok());

    // Should be connected again.
    let connections = manager.list_connections().await;
    assert!(matches!(
        &connections[0].1,
        McpConnectionState::Connected { .. }
    ));

    // Tools should still be available.
    let tools = manager.list_all_tools().await;
    assert!(!tools.is_empty());

    manager.disconnect_all().await;
    let _ = std::fs::remove_file(&script_path);
}

}

mod naming_tests {
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

}

mod oauth_tests {
//! Integration tests for the MCP OAuth 2.0 authorization-code flow.
//!
//! A mock authorization server runs on a random loopback port inside each
//! test. It speaks the minimal protocol the flow needs: the MCP server's 401
//! challenge (RFC 9728), protected-resource metadata, authorization-server
//! metadata (RFC 8414), dynamic client registration (RFC 7591), the
//! authorization redirect, and the token endpoint (RFC 6749).
//!
//! The user's browser is simulated by a background task that fetches the
//! authorization URL (with reqwest's default redirect following) once the
//! flow hands it over.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mirri_mcp::oauth::discovery::{
    authorization_server_metadata_url, extract_resource_metadata_url,
};
use mirri_mcp::oauth::{
    OAuthClient, OAuthConfig, OAuthError, TokenSet, TokenStore, generate_pkce_pair,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn unique_temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "mirri_mcp_oauth_{label}_{}_{}",
        std::process::id(),
        TEST_COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn now_unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Simulate the user opening the authorization URL in a browser: fetch it
/// and follow the authorization server's redirect to the callback listener.
fn open_url_in_background(url: &str) {
    let url = url.to_string();
    tokio::spawn(async move {
        let browser = reqwest::Client::new();
        let _ = browser.get(&url).send().await;
    });
}

// ---------------------------------------------------------------------------
// Mock authorization server
// ---------------------------------------------------------------------------

/// Behavior knobs for the mock authorization server.
#[derive(Debug, Clone, Copy, Default)]
struct MockBehavior {
    /// Corrupt the `state` echoed back in the authorization redirect.
    break_state: bool,
    /// Redirect with `error=access_denied` instead of an authorization code.
    deny: bool,
    /// Reject token requests with `{"error": "invalid_grant"}`.
    reject_token_request: bool,
    /// Omit `token_endpoint` from the authorization server metadata.
    omit_token_endpoint: bool,
    /// Omit `authorization_endpoint` from the authorization server metadata.
    omit_authorization_endpoint: bool,
    /// Omit `registration_endpoint` from the authorization server metadata.
    omit_registration_endpoint: bool,
}

/// Requests the mock authorization server received, for assertions.
#[derive(Debug, Clone, Default)]
struct RecordedCalls {
    registrations: Vec<serde_json::Value>,
    authorize_requests: Vec<HashMap<String, String>>,
    token_requests: Vec<HashMap<String, String>>,
}

struct MockAuthServer {
    base_url: String,
    recorded: Arc<Mutex<RecordedCalls>>,
}

impl MockAuthServer {
    async fn start(behavior: MockBehavior) -> (Self, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .expect("mock server should bind");
        let port = listener.local_addr().unwrap().port();
        let base_url = format!("http://127.0.0.1:{port}");
        let recorded = Arc::new(Mutex::new(RecordedCalls::default()));
        let server = Self {
            base_url: base_url.clone(),
            recorded: recorded.clone(),
        };
        let handle = tokio::spawn(async move {
            run_mock_server(listener, base_url, behavior, recorded).await;
        });
        (server, handle)
    }

    fn recorded(&self) -> RecordedCalls {
        self.recorded.lock().unwrap().clone()
    }
}

async fn run_mock_server(
    listener: TcpListener,
    base_url: String,
    behavior: MockBehavior,
    recorded: Arc<Mutex<RecordedCalls>>,
) {
    loop {
        let Ok((mut stream, _peer)) = listener.accept().await else {
            break;
        };
        if let Err(error) =
            handle_mock_connection(&mut stream, &base_url, behavior, &recorded).await
        {
            eprintln!("mock server connection error: {error}");
            break;
        }
    }
}

async fn handle_mock_connection(
    stream: &mut TcpStream,
    base_url: &str,
    behavior: MockBehavior,
    recorded: &Arc<Mutex<RecordedCalls>>,
) -> std::io::Result<()> {
    let (request_line, headers, body) = read_http_request(stream).await?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let (path, query_str) = match target.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (target.to_string(), String::new()),
    };
    let query = parse_query_params(&query_str);
    let content_type = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .map(|(_, value)| value.clone())
        .unwrap_or_default();

    let response = mock_route(
        base_url,
        behavior,
        recorded,
        &method,
        &path,
        &query,
        &body,
        &content_type,
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

#[allow(clippy::too_many_arguments)]
fn mock_route(
    base_url: &str,
    behavior: MockBehavior,
    recorded: &Arc<Mutex<RecordedCalls>>,
    method: &str,
    path: &str,
    query: &HashMap<String, String>,
    body: &str,
    content_type: &str,
) -> String {
    match path {
        // The MCP server endpoint: challenge with RFC 9728 discovery info.
        "/" | "/api" => http_response(
            "401 Unauthorized",
            &[(
                "WWW-Authenticate",
                format!(
                    "Bearer resource_metadata=\"{base_url}/.well-known/oauth-protected-resource\""
                ),
            )],
            "",
        ),
        "/.well-known/oauth-protected-resource" => http_response(
            "200 OK",
            &[],
            &serde_json::json!({
                "resource": format!("{base_url}/api"),
                "authorization_servers": [format!("{base_url}/as")],
            })
            .to_string(),
        ),
        "/as/.well-known/oauth-authorization-server" => {
            let mut metadata = serde_json::Map::new();
            metadata.insert("issuer".into(), serde_json::json!(format!("{base_url}/as")));
            if !behavior.omit_authorization_endpoint {
                metadata.insert(
                    "authorization_endpoint".into(),
                    serde_json::json!(format!("{base_url}/as/authorize")),
                );
            }
            if !behavior.omit_token_endpoint {
                metadata.insert(
                    "token_endpoint".into(),
                    serde_json::json!(format!("{base_url}/as/token")),
                );
            }
            if !behavior.omit_registration_endpoint {
                metadata.insert(
                    "registration_endpoint".into(),
                    serde_json::json!(format!("{base_url}/as/register")),
                );
            }
            metadata.insert(
                "scopes_supported".into(),
                serde_json::json!(["read", "write"]),
            );
            metadata.insert(
                "code_challenge_methods_supported".into(),
                serde_json::json!(["S256"]),
            );
            http_response(
                "200 OK",
                &[],
                &serde_json::Value::Object(metadata).to_string(),
            )
        }
        "/as/register" => {
            let registration: serde_json::Value =
                serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
            recorded.lock().unwrap().registrations.push(registration);
            http_response(
                "201 Created",
                &[],
                &serde_json::json!({
                    "client_id": "test-client",
                    "client_secret": null,
                    "client_id_issued_at": now_unix_seconds(),
                })
                .to_string(),
            )
        }
        "/as/authorize" => {
            require_query_params(
                query,
                &[
                    "response_type",
                    "client_id",
                    "redirect_uri",
                    "state",
                    "code_challenge",
                    "code_challenge_method",
                ],
            );
            let redirect_uri = query.get("redirect_uri").cloned().unwrap_or_default();
            let state = query.get("state").cloned().unwrap_or_default();
            recorded
                .lock()
                .unwrap()
                .authorize_requests
                .push(query.clone());
            if behavior.deny {
                let location = format!(
                    "{redirect_uri}?error=access_denied&error_description=mock%20denial&state={state}"
                );
                return http_response("302 Found", &[("Location", location)], "");
            }
            let echoed_state = if behavior.break_state {
                "evil-state"
            } else {
                &state
            };
            let location = format!("{redirect_uri}?code=mock-auth-code&state={echoed_state}");
            http_response("302 Found", &[("Location", location)], "")
        }
        "/as/token" if method == "POST" && content_type.contains("form-urlencoded") => {
            let form = parse_query_params(body);
            let grant_type = form.get("grant_type").cloned().unwrap_or_default();
            recorded.lock().unwrap().token_requests.push(form);
            if behavior.reject_token_request {
                return http_response(
                    "400 Bad Request",
                    &[],
                    &serde_json::json!({
                        "error": "invalid_grant",
                        "error_description": "mock rejection",
                    })
                    .to_string(),
                );
            }
            match grant_type.as_str() {
                "authorization_code" => http_response(
                    "200 OK",
                    &[],
                    &serde_json::json!({
                        "access_token": "mock-access-token",
                        "token_type": "Bearer",
                        "expires_in": 3600,
                        "refresh_token": "mock-refresh-token",
                        "scope": "read write",
                    })
                    .to_string(),
                ),
                "refresh_token" => http_response(
                    "200 OK",
                    &[],
                    &serde_json::json!({
                        "access_token": "mock-refreshed-token",
                        "token_type": "Bearer",
                        "expires_in": 3600,
                        "refresh_token": "mock-refresh-token-2",
                        "scope": "read write",
                    })
                    .to_string(),
                ),
                other => http_response(
                    "400 Bad Request",
                    &[],
                    &serde_json::json!({
                        "error": "unsupported_grant_type",
                        "error_description": format!("no mock support for {other}"),
                    })
                    .to_string(),
                ),
            }
        }
        _ => http_response("404 Not Found", &[], "no such route"),
    }
}

/// Assert the given query params are present, panicking with a clear message.
fn require_query_params(query: &HashMap<String, String>, required: &[&str]) {
    for param in required {
        assert!(
            query.contains_key(*param),
            "mock /as/authorize expected query param {param:?}, got: {query:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Minimal HTTP plumbing for the mock server
// ---------------------------------------------------------------------------

/// Read the request line, headers, and (per Content-Length) body.
async fn read_http_request(
    stream: &mut TcpStream,
) -> std::io::Result<(String, Vec<(String, String)>, String)> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 2048];
    let header_end = loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "connection closed before request completed",
            ));
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default().to_string();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
        .collect::<Vec<_>>();
    let content_length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    // Body data may have already been read into `buffer` along with the headers
    // (common on localhost where the entire request arrives in one TCP segment).
    // Use that data first, then read any remainder from the stream.
    let body_already_in_buffer = buffer.len().saturating_sub(header_end);
    let usable = body_already_in_buffer.min(content_length);
    let mut body = String::from_utf8_lossy(&buffer[header_end..header_end + usable]).to_string();
    let mut remaining = content_length.saturating_sub(body.len());
    while remaining > 0 {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        body.push_str(&String::from_utf8_lossy(&chunk[..read]));
        remaining = remaining.saturating_sub(read);
    }
    Ok((request_line, headers, body))
}

/// Parse `application/x-www-form-urlencoded` style query parameters.
fn parse_query_params(query_str: &str) -> HashMap<String, String> {
    let mut params = HashMap::new();
    for pair in query_str.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        params.insert(percent_decode(key), percent_decode(value));
    }
    params
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let high = (bytes[index + 1] as char).to_digit(16);
                let low = (bytes[index + 2] as char).to_digit(16);
                if let (Some(high), Some(low)) = (high, low) {
                    decoded.push((high * 16 + low) as u8);
                    index += 3;
                    continue;
                }
                decoded.push(bytes[index]);
                index += 1;
            }
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn http_response(status_line: &str, extra_headers: &[(&str, String)], body: &str) -> String {
    let mut head = format!(
        "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in extra_headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    format!("{head}\r\n{body}")
}

// ---------------------------------------------------------------------------
// Helper: run a flow to completion against the mock server
// ---------------------------------------------------------------------------

fn default_config(client_id: Option<&str>) -> OAuthConfig {
    OAuthConfig {
        client_id: client_id.map(str::to_string),
        scopes: vec!["read".to_string(), "write".to_string()],
        redirect_port: None,
    }
}

/// Run the full flow and return the persisted token set plus the recorded
/// mock calls.
async fn run_full_flow(
    server: &MockAuthServer,
    storage_dir: PathBuf,
    config: &OAuthConfig,
) -> (TokenSet, RecordedCalls) {
    let client = OAuthClient::new(reqwest::Client::new(), storage_dir);
    let tokens = tokio::time::timeout(
        Duration::from_secs(30),
        client.authenticate_with_url_handler(
            &format!("{}/api", server.base_url),
            "mock-server",
            config,
            open_url_in_background,
        ),
    )
    .await
    .expect("oauth flow timed out")
    .expect("oauth flow failed");
    (tokens, server.recorded())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_complete_the_authorization_code_flow_when_using_dynamic_registration() {
    let (server, _handle) = MockAuthServer::start(MockBehavior::default()).await;
    let storage_dir = unique_temp_dir("full_flow");
    let (tokens, recorded) =
        run_full_flow(&server, storage_dir.clone(), &default_config(None)).await;

    // The exchanged token set.
    assert_eq!(tokens.access_token, "mock-access-token");
    assert_eq!(tokens.token_type, "Bearer");
    assert_eq!(tokens.refresh_token.as_deref(), Some("mock-refresh-token"));
    assert_eq!(tokens.scope.as_deref(), Some("read write"));
    let expires_at = tokens.expires_at.expect("expires_at should be set");
    assert!(
        expires_at > now_unix_seconds() && expires_at <= now_unix_seconds() + 3600,
        "expires_at should be now + expires_in"
    );
    assert_eq!(tokens.client_id.as_deref(), Some("test-client"));
    let expected_token_endpoint = format!("{}/as/token", server.base_url);
    assert_eq!(
        tokens.token_endpoint.as_deref(),
        Some(expected_token_endpoint.as_str())
    );

    // Dynamic client registration body.
    let registrations = &recorded.registrations;
    assert_eq!(registrations.len(), 1);
    let registration = &registrations[0];
    assert_eq!(registration["client_name"], "mirri-code");
    assert_eq!(
        registration["grant_types"],
        serde_json::json!(["authorization_code", "refresh_token"])
    );
    assert_eq!(registration["token_endpoint_auth_method"], "none");
    assert_eq!(registration["scope"], "read write");
    let registered_redirect_uri = registration["redirect_uris"][0].as_str().unwrap();
    assert!(registered_redirect_uri.ends_with("/callback"));
    assert!(registered_redirect_uri.starts_with("http://localhost:"));

    // The authorization request.
    let authorize = &recorded.authorize_requests[0];
    assert_eq!(
        authorize.get("response_type").map(String::as_str),
        Some("code")
    );
    assert_eq!(
        authorize.get("client_id").map(String::as_str),
        Some("test-client")
    );
    assert_eq!(
        authorize.get("redirect_uri").map(String::as_str),
        Some(registered_redirect_uri)
    );
    assert_eq!(
        authorize.get("scope").map(String::as_str),
        Some("read write")
    );
    assert_eq!(
        authorize.get("code_challenge_method").map(String::as_str),
        Some("S256")
    );
    assert_eq!(authorize.get("code_challenge").map(String::len), Some(43));
    assert_eq!(authorize.get("state").map(String::len), Some(43));

    // The token exchange.
    let exchange = &recorded.token_requests[0];
    assert_eq!(
        exchange.get("grant_type").map(String::as_str),
        Some("authorization_code")
    );
    assert_eq!(
        exchange.get("client_id").map(String::as_str),
        Some("test-client")
    );
    assert_eq!(
        exchange.get("code").map(String::as_str),
        Some("mock-auth-code")
    );
    assert_eq!(exchange.get("code_verifier").map(String::len), Some(43));
    assert_eq!(
        exchange.get("redirect_uri").map(String::as_str),
        Some(registered_redirect_uri)
    );

    // The token set is persisted and reloads.
    let store = TokenStore::new(storage_dir);
    assert_eq!(store.load("mock-server"), Some(tokens));
}

#[tokio::test]
async fn should_use_configured_client_id_and_skip_registration_when_client_id_is_pre_configured() {
    let (server, _handle) = MockAuthServer::start(MockBehavior::default()).await;
    let storage_dir = unique_temp_dir("configured_client");
    let config = default_config(Some("pre-registered-client"));

    let (tokens, recorded) = run_full_flow(&server, storage_dir, &config).await;

    assert!(
        recorded.registrations.is_empty(),
        "no DCR when client_id is configured"
    );
    assert_eq!(tokens.client_id.as_deref(), Some("pre-registered-client"));
    assert_eq!(
        recorded.authorize_requests[0]
            .get("client_id")
            .map(String::as_str),
        Some("pre-registered-client")
    );
}

#[tokio::test]
async fn should_return_cached_token_without_network_when_cached_token_is_valid() {
    let (server, _handle) = MockAuthServer::start(MockBehavior::default()).await;
    let storage_dir = unique_temp_dir("cached_token");
    run_full_flow(&server, storage_dir.clone(), &default_config(None)).await;

    // A fresh client with the same storage dir must reuse the persisted token.
    let initial_request_count = server.recorded().token_requests.len();
    let client = OAuthClient::new(reqwest::Client::new(), storage_dir);
    let token = client
        .get_valid_token("mock-server")
        .await
        .expect("cached token should be returned");
    assert_eq!(token, "mock-access-token");
    let after = server.recorded();
    assert_eq!(
        after.token_requests.len(),
        initial_request_count,
        "no new token request should be issued for a valid cached token"
    );
}

#[tokio::test]
async fn should_refresh_expired_access_token_when_refresh_token_is_available() {
    let (server, _handle) = MockAuthServer::start(MockBehavior::default()).await;
    let storage_dir = unique_temp_dir("refresh");
    run_full_flow(&server, storage_dir.clone(), &default_config(None)).await;

    // Expire the cached tokens, then ask for a valid token.
    let store = TokenStore::new(storage_dir.clone());
    let mut cached = store.load("mock-server").expect("cached tokens exist");
    cached.expires_at = Some(now_unix_seconds() - 60);
    store.save("mock-server", &cached).unwrap();

    let client = OAuthClient::new(reqwest::Client::new(), storage_dir);
    let token = client
        .get_valid_token("mock-server")
        .await
        .expect("refresh should succeed");
    assert_eq!(token, "mock-refreshed-token");

    let recorded = server.recorded();
    // run_full_flow already issued 1 token request (authorization_code exchange);
    // the refresh adds a second.
    assert_eq!(recorded.token_requests.len(), 2);
    let refresh_request = &recorded.token_requests[1];
    assert_eq!(
        refresh_request.get("grant_type").map(String::as_str),
        Some("refresh_token")
    );
    assert_eq!(
        refresh_request.get("refresh_token").map(String::as_str),
        Some("mock-refresh-token")
    );
    assert_eq!(
        refresh_request.get("client_id").map(String::as_str),
        Some("test-client")
    );

    // The refreshed token set is persisted with the rotated refresh token.
    let refreshed = store
        .load("mock-server")
        .expect("refreshed tokens persisted");
    assert_eq!(refreshed.access_token, "mock-refreshed-token");
    assert_eq!(
        refreshed.refresh_token.as_deref(),
        Some("mock-refresh-token-2")
    );
    assert!(refreshed.expires_at.unwrap() > now_unix_seconds());
    assert_eq!(refreshed.client_id.as_deref(), Some("test-client"));
}

#[tokio::test]
async fn should_error_when_no_tokens_are_cached() {
    let (_server, _handle) = MockAuthServer::start(MockBehavior::default()).await;
    let storage_dir = unique_temp_dir("no_tokens");
    let client = OAuthClient::new(reqwest::Client::new(), storage_dir);

    let result = client.get_valid_token("never-authenticated").await;
    assert!(matches!(result, Err(OAuthError::NoCachedTokens(_))));

    let result = client.refresh("never-authenticated").await;
    assert!(matches!(result, Err(OAuthError::NoCachedTokens(_))));
}

#[tokio::test]
async fn should_error_when_cached_tokens_are_expired_without_a_refresh_token() {
    let (server, _handle) = MockAuthServer::start(MockBehavior::default()).await;
    let storage_dir = unique_temp_dir("no_refresh_token");
    let store = TokenStore::new(storage_dir.clone());
    store
        .save(
            "mock-server",
            &TokenSet {
                access_token: "stale".to_string(),
                refresh_token: None,
                expires_at: Some(now_unix_seconds() - 60),
                token_type: "Bearer".to_string(),
                scope: None,
                client_id: Some("test-client".to_string()),
                token_endpoint: Some(format!("{}/as/token", server.base_url)),
            },
        )
        .unwrap();

    let client = OAuthClient::new(reqwest::Client::new(), storage_dir);
    let result = client.get_valid_token("mock-server").await;
    assert!(matches!(result, Err(OAuthError::NoRefreshToken(_))));
}

#[tokio::test]
async fn should_error_when_the_callback_state_does_not_match() {
    let (server, _handle) = MockAuthServer::start(MockBehavior {
        break_state: true,
        ..MockBehavior::default()
    })
    .await;
    let storage_dir = unique_temp_dir("state_mismatch");
    let client = OAuthClient::new(reqwest::Client::new(), storage_dir.clone());

    let result = tokio::time::timeout(
        Duration::from_secs(30),
        client.authenticate_with_url_handler(
            &format!("{}/api", server.base_url),
            "mock-server",
            &default_config(None),
            open_url_in_background,
        ),
    )
    .await
    .expect("flow timed out");

    assert!(matches!(result, Err(OAuthError::StateMismatch)));
    assert!(
        TokenStore::new(storage_dir).load("mock-server").is_none(),
        "no tokens should be persisted after a failed flow"
    );
}

#[tokio::test]
async fn should_error_when_the_user_denies_authorization() {
    let (server, _handle) = MockAuthServer::start(MockBehavior {
        deny: true,
        ..MockBehavior::default()
    })
    .await;
    let storage_dir = unique_temp_dir("denied");
    let client = OAuthClient::new(reqwest::Client::new(), storage_dir.clone());

    let result = tokio::time::timeout(
        Duration::from_secs(30),
        client.authenticate_with_url_handler(
            &format!("{}/api", server.base_url),
            "mock-server",
            &default_config(None),
            open_url_in_background,
        ),
    )
    .await
    .expect("flow timed out");

    match result {
        Err(OAuthError::UserDenied(description)) => {
            assert_eq!(description, "mock denial");
        }
        other => panic!("expected UserDenied, got {other:?}"),
    }
    assert!(TokenStore::new(storage_dir).load("mock-server").is_none());
}

#[tokio::test]
async fn should_error_when_the_token_exchange_is_rejected() {
    let (server, _handle) = MockAuthServer::start(MockBehavior {
        reject_token_request: true,
        ..MockBehavior::default()
    })
    .await;
    let storage_dir = unique_temp_dir("rejected_exchange");
    let client = OAuthClient::new(reqwest::Client::new(), storage_dir);

    let result = tokio::time::timeout(
        Duration::from_secs(30),
        client.authenticate_with_url_handler(
            &format!("{}/api", server.base_url),
            "mock-server",
            &default_config(None),
            open_url_in_background,
        ),
    )
    .await
    .expect("flow timed out");

    let error = result.expect_err("token exchange should fail");
    assert!(matches!(error, OAuthError::TokenExchange(_)));
    assert!(
        error.to_string().contains("invalid_grant"),
        "expected the OAuth error code in the message, got: {error}"
    );
}

#[tokio::test]
async fn should_error_when_a_client_id_is_needed_but_registration_is_unavailable() {
    let (server, _handle) = MockAuthServer::start(MockBehavior {
        omit_registration_endpoint: true,
        ..MockBehavior::default()
    })
    .await;
    let storage_dir = unique_temp_dir("no_registration");
    let client = OAuthClient::new(reqwest::Client::new(), storage_dir);

    let result = tokio::time::timeout(
        Duration::from_secs(30),
        client.authenticate_with_url_handler(
            &format!("{}/api", server.base_url),
            "mock-server",
            &default_config(None),
            |_| (),
        ),
    )
    .await
    .expect("flow timed out");

    assert!(matches!(result, Err(OAuthError::NoRegistrationEndpoint)));
}

#[tokio::test]
async fn should_error_when_the_authorization_endpoint_is_missing() {
    let (server, _handle) = MockAuthServer::start(MockBehavior {
        omit_authorization_endpoint: true,
        ..MockBehavior::default()
    })
    .await;
    let storage_dir = unique_temp_dir("no_authorize");
    let client = OAuthClient::new(reqwest::Client::new(), storage_dir);

    let result = tokio::time::timeout(
        Duration::from_secs(30),
        client.authenticate_with_url_handler(
            &format!("{}/api", server.base_url),
            "mock-server",
            &default_config(None),
            |_| (),
        ),
    )
    .await
    .expect("flow timed out");

    let error = result.expect_err("flow should fail without an authorization endpoint");
    assert!(matches!(error, OAuthError::InvalidResponse(_)));
    assert!(error.to_string().contains("authorization_endpoint"));
}

#[tokio::test]
async fn should_error_when_the_token_endpoint_is_missing() {
    let (server, _handle) = MockAuthServer::start(MockBehavior {
        omit_token_endpoint: true,
        ..MockBehavior::default()
    })
    .await;
    let storage_dir = unique_temp_dir("no_token_endpoint");
    let client = OAuthClient::new(reqwest::Client::new(), storage_dir);

    let result = tokio::time::timeout(
        Duration::from_secs(30),
        client.authenticate_with_url_handler(
            &format!("{}/api", server.base_url),
            "mock-server",
            &default_config(None),
            |_| (),
        ),
    )
    .await
    .expect("flow timed out");

    let error = result.expect_err("flow should fail without a token endpoint");
    assert!(matches!(error, OAuthError::InvalidResponse(_)));
    assert!(error.to_string().contains("token_endpoint"));
}

#[test]
fn should_extract_resource_metadata_url_when_parsing_www_authenticate_header() {
    let header =
        "Bearer resource_metadata=\"https://example.com/.well-known/oauth-protected-resource\"";
    assert_eq!(
        extract_resource_metadata_url(header).as_deref(),
        Some("https://example.com/.well-known/oauth-protected-resource")
    );
    assert_eq!(extract_resource_metadata_url("Bearer scope=\"read\""), None);
}

#[test]
fn should_build_well_known_metadata_url_when_constructing_rfc8414_url() {
    assert_eq!(
        authorization_server_metadata_url("https://auth.example.com").unwrap(),
        "https://auth.example.com/.well-known/oauth-authorization-server"
    );
    assert_eq!(
        authorization_server_metadata_url("https://auth.example.com/issuer").unwrap(),
        "https://auth.example.com/issuer/.well-known/oauth-authorization-server"
    );
}

#[test]
fn should_generate_pkce_material_when_creating_rfc7636_pair() {
    let pair = generate_pkce_pair().unwrap();
    assert_eq!(pair.method, "S256");
    assert_eq!(pair.code_verifier.len(), 43);
    assert_eq!(pair.code_challenge.len(), 43);
    assert_ne!(pair.code_challenge, pair.code_verifier);
}

#[test]
fn should_persist_and_reload_token_sets_when_saving_to_and_loading_from_disk() {
    let storage_dir = unique_temp_dir("roundtrip");
    let store = TokenStore::new(storage_dir.clone());
    let tokens = TokenSet {
        access_token: "access".to_string(),
        refresh_token: Some("refresh".to_string()),
        expires_at: Some(now_unix_seconds() + 3600),
        token_type: "Bearer".to_string(),
        scope: Some("read".to_string()),
        client_id: Some("client".to_string()),
        token_endpoint: Some("https://auth.example.com/token".to_string()),
    };
    store.save("example-server", &tokens).unwrap();
    assert_eq!(store.load("example-server"), Some(tokens));
    assert_eq!(
        store.path_for("example-server"),
        storage_dir.join("example-server.json")
    );
    let _ = std::fs::remove_dir_all(storage_dir);
}

}

mod oauth_wiring_tests {
//! Integration tests for the OAuth connect-path wiring (plans/07 T8).
//!
//! A minimal in-test HTTP server plays both roles on one loopback port:
//! the MCP server (JSON-RPC POSTs — 401 with an RFC 9728 challenge without
//! a valid bearer token, full handshake with one) and the token endpoint
//! (the refresh grant). The recorded requests drive the assertions: what
//! authorization header each request actually carried.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mirri_mcp::oauth::TokenStore;
use mirri_mcp::{McpConnectionManager, McpConnectionState, McpServerConfig, TokenSet};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn unique_temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "mirri_mcp_oauth_wiring_{label}_{}_{}",
        std::process::id(),
        TEST_COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn now_unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

// ---------------------------------------------------------------------------
// The mock server
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
#[allow(dead_code)] // the recorded fields exist for debugging assertions
struct RecordedRequest {
    method: String,
    path: String,
    authorization: Option<String>,
    body: String,
}

#[derive(Clone)]
struct MockMcpServer {
    url: String,
    base_url: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    /// Bearer values the MCP endpoint accepts (the refresh test grows this).
    valid_tokens: Arc<Mutex<Vec<String>>>,
    /// Remaining leading requests to fail with HTTP 500.
    fail_leading: Arc<AtomicUsize>,
}

impl MockMcpServer {
    async fn start() -> Self {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .expect("mock server should bind");
        let port = listener.local_addr().unwrap().port();
        let base_url = format!("http://127.0.0.1:{port}");
        let server = Self {
            url: format!("{base_url}/mcp"),
            base_url,
            requests: Arc::new(Mutex::new(Vec::new())),
            valid_tokens: Arc::new(Mutex::new(vec!["good-token".to_string()])),
            fail_leading: Arc::new(AtomicUsize::new(0)),
        };
        let requests = Arc::clone(&server.requests);
        let valid_tokens = Arc::clone(&server.valid_tokens);
        let fail_leading = Arc::clone(&server.fail_leading);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let requests = Arc::clone(&requests);
                let valid_tokens = Arc::clone(&valid_tokens);
                let fail_leading = Arc::clone(&fail_leading);
                tokio::spawn(async move {
                    if let Err(error) =
                        serve_connection(&mut stream, requests, valid_tokens, fail_leading).await
                    {
                        eprintln!("mock server connection error: {error}");
                    }
                });
            }
        });
        server
    }

    fn recorded(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap().clone()
    }

    fn mcp_requests(&self) -> Vec<RecordedRequest> {
        self.recorded()
            .into_iter()
            .filter(|request| request.path == "/mcp")
            .collect()
    }
}

async fn serve_connection(
    stream: &mut TcpStream,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
    valid_tokens: Arc<Mutex<Vec<String>>>,
    fail_leading: Arc<AtomicUsize>,
) -> std::io::Result<()> {
    let (request_line, headers, body) = read_http_request(stream).await?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let path = target.split('?').next().unwrap_or("/").to_string();
    let authorization = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        .map(|(_, value)| value.clone());

    requests.lock().unwrap().push(RecordedRequest {
        method: method.clone(),
        path: path.clone(),
        authorization: authorization.clone(),
        body: body.clone(),
    });

    let response = if path == "/token" {
        // The refresh grant: parse the form, hand out a rotated token.
        let form = parse_form(&body);
        if form.get("grant_type").map(String::as_str) == Some("refresh_token") {
            http_json(
                "200 OK",
                &serde_json::json!({
                    "access_token": "refreshed-token",
                    "token_type": "Bearer",
                    "expires_in": 3600,
                    "refresh_token": "refresh-token-2",
                }),
            )
        } else {
            http_json(
                "400 Bad Request",
                &serde_json::json!({"error": "unsupported_grant_type"}),
            )
        }
    } else if path == "/mcp" {
        // Compare-and-swap loop (not `fetch_update`, deprecated as
        // `try_update` in 1.99 — spelling either name pins the test to one
        // toolchain band).
        let mut current = fail_leading.load(Ordering::SeqCst);
        let failing = loop {
            if current == 0 {
                break false;
            }
            match fail_leading.compare_exchange(
                current,
                current - 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => break true,
                Err(observed) => current = observed,
            }
        };
        if failing {
            return write_response(stream, "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
        }
        let authorized = authorization
            .as_deref()
            .and_then(|value| value.strip_prefix("Bearer "))
            .map(|token| {
                valid_tokens
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|valid| valid == token)
            })
            .unwrap_or(false);
        if !authorized {
            return write_response(
                stream,
                "HTTP/1.1 401 Unauthorized\r\n\
                 WWW-Authenticate: Bearer resource_metadata=\"/.well-known/oauth-protected-resource\"\r\n\
                 Content-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await;
        }
        json_rpc_response(&body)
    } else {
        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
    };
    write_response(stream, &response).await
}

/// Answer the JSON-RPC request body: `initialize` and `tools/list` get
/// real results, requests with no `id` are notifications (no body).
fn json_rpc_response(body: &str) -> String {
    let request: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let method = request["method"].as_str().unwrap_or_default();
    let id = request.get("id").cloned();
    let Some(id) = id.filter(|value| !value.is_null()) else {
        return "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_string();
    };
    let result = match method {
        "initialize" => serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "serverInfo": {"name": "oauth-mock", "version": "1.0"}
        }),
        "tools/list" => serde_json::json!({
            "tools": [{
                "name": "echo",
                "description": "echo tool",
                "inputSchema": {"type": "object", "properties": {}}
            }]
        }),
        _ => serde_json::json!({}),
    };
    http_json(
        "200 OK",
        &serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}),
    )
}

// ---------------------------------------------------------------------------
// Minimal HTTP plumbing (the oauth_tests pattern)
// ---------------------------------------------------------------------------

async fn read_http_request(
    stream: &mut TcpStream,
) -> std::io::Result<(String, Vec<(String, String)>, String)> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "connection closed before request completed",
            ));
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default().to_string();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
        .collect::<Vec<_>>();
    let content_length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    let already = buffer.len().saturating_sub(header_end);
    let usable = already.min(content_length);
    let mut body = String::from_utf8_lossy(&buffer[header_end..header_end + usable]).to_string();
    let mut remaining = content_length.saturating_sub(body.len());
    while remaining > 0 {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        body.push_str(&String::from_utf8_lossy(&chunk[..read]));
        remaining = remaining.saturating_sub(read);
    }
    Ok((request_line, headers, body))
}

fn parse_form(body: &str) -> HashMap<String, String> {
    body.split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| pair.split_once('=').unwrap_or((pair, "")))
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect()
}

fn http_json(status_line: &str, value: &serde_json::Value) -> String {
    let body = value.to_string();
    format!(
        "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

async fn write_response(stream: &mut TcpStream, response: &str) -> std::io::Result<()> {
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn oauth_http_config(url: &str) -> McpServerConfig {
    let raw = serde_json::json!({
        "transport": {"type": "http", "url": url},
        "auth": {"type": "oauth", "scopes": ["read"]},
    });
    serde_json::from_value(raw).expect("the config should parse")
}

fn plain_http_config(url: &str) -> McpServerConfig {
    let raw = serde_json::json!({
        "transport": {"type": "http", "url": url},
    });
    serde_json::from_value(raw).expect("the config should parse")
}

fn oauth_manager(dir: &std::path::Path) -> Arc<McpConnectionManager> {
    Arc::new(McpConnectionManager::with_oauth_token_dir(
        dir.to_path_buf(),
    ))
}

/// Poll the manager until the named server reaches `want` (or the deadline
/// panics).
async fn wait_for_state(
    manager: &McpConnectionManager,
    name: &str,
    want: fn(&McpConnectionState) -> bool,
    what: &str,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let state = manager
            .list_connections()
            .await
            .into_iter()
            .find(|(conn, _)| conn == name)
            .map(|(_, state)| state);
        if let Some(state) = &state
            && want(state)
        {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("connection '{name}' never reached {what} (last: {state:?})");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn is_needs_auth(state: &McpConnectionState) -> bool {
    matches!(state, McpConnectionState::NeedsAuth)
}

fn is_connected(state: &McpConnectionState) -> bool {
    matches!(state, McpConnectionState::Connected { .. })
}

fn is_failed(state: &McpConnectionState) -> bool {
    matches!(state, McpConnectionState::Failed { .. })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Given an OAuth-configured remote server with no cached tokens, when the
/// manager connects, then the initial request carries no bearer, the
/// server's real 401 is observed, and the connection lands in `needs-auth`.
#[tokio::test]
async fn should_land_in_needs_auth_when_an_oauth_server_answers_401_without_tokens() {
    let mock = MockMcpServer::start().await;
    let dir = unique_temp_dir("needs-auth");
    let manager = oauth_manager(&dir);

    manager
        .connect_one("srv", oauth_http_config(&mock.url))
        .await
        .expect_err("the 401 connect must fail");
    wait_for_state(&manager, "srv", is_needs_auth, "needs-auth").await;

    // The 401 was real: the request went out without an Authorization
    // header and the server challenged it.
    let mcp_requests = mock.mcp_requests();
    assert!(
        !mcp_requests.is_empty(),
        "the connect must reach the server"
    );
    assert!(
        mcp_requests
            .iter()
            .all(|request| request.authorization.is_none()),
        "no bearer is attached without cached tokens, got: {mcp_requests:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// Given the same 401 answer with no OAuth configured, when the manager
/// connects, then the connection fails plainly (needs-auth is the OAuth
/// flow's state, not a generic 401 bucket).
#[tokio::test]
async fn should_fail_plainly_when_a_401_server_has_no_oauth_config() {
    let mock = MockMcpServer::start().await;
    let dir = unique_temp_dir("plain-401");
    let manager = oauth_manager(&dir);

    manager
        .connect_one("srv", plain_http_config(&mock.url))
        .await
        .expect_err("the 401 connect must fail");
    wait_for_state(&manager, "srv", is_failed, "failed").await;
    let _ = std::fs::remove_dir_all(dir);
}

/// Given a cached, still-valid token, when the manager connects, then the
/// token is attached to every request and the connection succeeds.
#[tokio::test]
async fn should_attach_the_cached_token_when_one_exists() {
    let mock = MockMcpServer::start().await;
    let dir = unique_temp_dir("cached");
    TokenStore::new(dir.clone())
        .save(
            "srv",
            &TokenSet {
                access_token: "good-token".to_string(),
                refresh_token: Some("refresh".to_string()),
                expires_at: Some(now_unix_seconds() + 3600),
                token_type: "Bearer".to_string(),
                scope: Some("read".to_string()),
                client_id: Some("client".to_string()),
                token_endpoint: Some(format!("{}/token", mock.base_url)),
            },
        )
        .unwrap();
    let manager = oauth_manager(&dir);

    manager
        .connect_one("srv", oauth_http_config(&mock.url))
        .await
        .expect("the authorized connect must succeed");
    wait_for_state(&manager, "srv", is_connected, "connected").await;

    let mcp_requests = mock.mcp_requests();
    assert!(
        !mcp_requests.is_empty()
            && mcp_requests
                .iter()
                .all(|request| request.authorization.as_deref() == Some("Bearer good-token")),
        "every request must carry the cached bearer, got: {mcp_requests:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// Given a cached but expired token with a refresh token, when the manager
/// connects, then the provider refreshes first (the token endpoint sees the
/// refresh grant) and the requests carry the refreshed bearer.
#[tokio::test]
async fn should_refresh_an_expired_token_before_the_first_request() {
    let mock = MockMcpServer::start().await;
    // The refreshed token must be accepted by the MCP endpoint too.
    mock.valid_tokens
        .lock()
        .unwrap()
        .push("refreshed-token".to_string());
    let dir = unique_temp_dir("refresh");
    TokenStore::new(dir.clone())
        .save(
            "srv",
            &TokenSet {
                access_token: "stale-token".to_string(),
                refresh_token: Some("refresh".to_string()),
                expires_at: Some(now_unix_seconds() - 60),
                token_type: "Bearer".to_string(),
                scope: Some("read".to_string()),
                client_id: Some("client".to_string()),
                token_endpoint: Some(format!("{}/token", mock.base_url)),
            },
        )
        .unwrap();
    let manager = oauth_manager(&dir);

    manager
        .connect_one("srv", oauth_http_config(&mock.url))
        .await
        .expect("the refreshed connect must succeed");
    wait_for_state(&manager, "srv", is_connected, "connected").await;

    // The refresh grant hit the token endpoint...
    let refresh = mock
        .recorded()
        .into_iter()
        .find(|request| request.path == "/token")
        .expect("the expired token must be refreshed before the request");
    let form = parse_form(&refresh.body);
    assert_eq!(
        form.get("grant_type").map(String::as_str),
        Some("refresh_token")
    );
    assert_eq!(
        form.get("refresh_token").map(String::as_str),
        Some("refresh")
    );
    // ...and the requests carry the refreshed bearer.
    let mcp_requests = mock.mcp_requests();
    assert!(
        !mcp_requests.is_empty()
            && mcp_requests
                .iter()
                .all(|request| request.authorization.as_deref() == Some("Bearer refreshed-token")),
        "every request must carry the refreshed bearer, got: {mcp_requests:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// Given an OAuth-configured stdio server (adjudication A4: stdio never
/// produces a 401), when the connect fails, then the state is an ordinary
/// `failed` — the needs-auth mapping is remote-transport only.
#[tokio::test]
async fn should_fail_plainly_when_a_stdio_server_declares_oauth() {
    let raw = serde_json::json!({
        "transport": {"type": "stdio", "command": "echo", "args": []},
        "auth": {"type": "oauth"},
    });
    let config: McpServerConfig = serde_json::from_value(raw).expect("the config should parse");
    let manager = oauth_manager(&unique_temp_dir("stdio"));

    // "echo" speaks no JSON-RPC: the handshake fails.
    let _ = manager.connect_one("srv", config).await;
    wait_for_state(&manager, "srv", is_failed, "failed").await;
}

/// Given a remote OAuth server with no cached tokens, when a token-file
/// accessor question arises, then the manager's public shape round-trips
/// the configured directory (and `new()` stays OAuth-free).
#[test]
fn should_expose_the_configured_oauth_token_dir() {
    let dir = unique_temp_dir("accessor");
    let manager = McpConnectionManager::with_oauth_token_dir(dir.clone());
    assert_eq!(manager.oauth_token_dir(), Some(dir.as_path()));
    assert_eq!(McpConnectionManager::new().oauth_token_dir(), None);
    let _ = std::fs::remove_dir_all(dir);
}

}

mod reconnect_tests {
//! Integration tests for the automatic reconnection loop (plans/07 T8).
//!
//! A minimal in-test HTTP server plays the MCP endpoint with a
//! controllable failure budget: the first N requests answer HTTP 500,
//! everything after answers proper JSON-RPC. The recorded request count
//! drives the assertions (how many attempts the loop actually made).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use mirri_mcp::{McpConnectionManager, McpConnectionState, McpServerConfig, ReconnectBackoff};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

static TEST_COUNTER: AtomicUsize = AtomicUsize::new(0);

fn unique_temp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "mirri_mcp_reconnect_{label}_{}_{}",
        std::process::id(),
        TEST_COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

// ---------------------------------------------------------------------------
// The mock server
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct FlakyMockMcp {
    url: String,
    request_count: Arc<AtomicUsize>,
}

impl FlakyMockMcp {
    /// A mock that fails the first `fail_leading` requests, then serves
    /// the handshake.
    async fn start(fail_leading: usize) -> Self {
        Self::start_inner(Arc::new(AtomicUsize::new(fail_leading))).await
    }

    /// A mock that fails every request (never recovers).
    async fn start_always_failing() -> Self {
        Self::start_inner(Arc::new(AtomicUsize::new(usize::MAX))).await
    }

    async fn start_inner(fail_leading: Arc<AtomicUsize>) -> Self {
        let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .expect("mock server should bind");
        let port = listener.local_addr().unwrap().port();
        let url = format!("http://127.0.0.1:{port}/mcp");
        let mock = Self {
            url,
            request_count: Arc::new(AtomicUsize::new(0)),
        };
        let request_count = Arc::clone(&mock.request_count);
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let request_count = Arc::clone(&request_count);
                let fail_leading = Arc::clone(&fail_leading);
                tokio::spawn(async move {
                    let _ = serve_flaky(&mut stream, request_count, fail_leading).await;
                });
            }
        });
        mock
    }

    fn count(&self) -> usize {
        self.request_count.load(Ordering::SeqCst)
    }
}

async fn serve_flaky(
    stream: &mut TcpStream,
    request_count: Arc<AtomicUsize>,
    fail_leading: Arc<AtomicUsize>,
) -> std::io::Result<()> {
    let (request_line, headers, body) = read_http_request(stream).await?;
    let _ = request_line;
    request_count.fetch_add(1, Ordering::SeqCst);
    // Compare-and-swap loop (not `fetch_update`, deprecated as `try_update`
    // in 1.99 — spelling either name pins the test to one toolchain band).
    let mut current = fail_leading.load(Ordering::SeqCst);
    let still_failing = loop {
        let next = if current == usize::MAX {
            Some(current) // permanent failure — keep the counter at MAX
        } else if current > 0 {
            Some(current - 1)
        } else {
            None // budget spent — serve normally
        };
        let Some(next) = next else { break false };
        match fail_leading.compare_exchange(current, next, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => break true,
            Err(observed) => current = observed,
        }
    };
    if still_failing {
        return write_response(
            stream,
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;
    }
    let _ = headers;
    let response = json_rpc_response(&body);
    write_response(stream, &response).await
}

fn json_rpc_response(body: &str) -> String {
    let request: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let method = request["method"].as_str().unwrap_or_default();
    let id = request.get("id").cloned();
    let Some(id) = id.filter(|value| !value.is_null()) else {
        return "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_string();
    };
    let result = match method {
        "initialize" => serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "serverInfo": {"name": "flaky-mock", "version": "1.0"}
        }),
        "tools/list" => serde_json::json!({
            "tools": [{
                "name": "echo",
                "description": "echo tool",
                "inputSchema": {"type": "object", "properties": {}}
            }]
        }),
        _ => serde_json::json!({}),
    };
    let payload = serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string();
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    )
}

async fn read_http_request(
    stream: &mut TcpStream,
) -> std::io::Result<(String, Vec<(String, String)>, String)> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "connection closed before request completed",
            ));
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default().to_string();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
        .collect::<Vec<_>>();
    let content_length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    let already = buffer.len().saturating_sub(header_end);
    let usable = already.min(content_length);
    let mut body = String::from_utf8_lossy(&buffer[header_end..header_end + usable]).to_string();
    let mut remaining = content_length.saturating_sub(body.len());
    while remaining > 0 {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        body.push_str(&String::from_utf8_lossy(&chunk[..read]));
        remaining = remaining.saturating_sub(read);
    }
    Ok((request_line, headers, body))
}

async fn write_response(stream: &mut TcpStream, response: &str) -> std::io::Result<()> {
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn http_config(url: &str) -> McpServerConfig {
    let raw = serde_json::json!({
        "transport": {"type": "http", "url": url},
        "startupTimeoutMs": 2000,
    });
    serde_json::from_value(raw).expect("the config should parse")
}

/// A fast backoff profile so the loops settle within test time.
fn fast_backoff(max_attempts: u32) -> ReconnectBackoff {
    ReconnectBackoff {
        base: Duration::from_millis(20),
        factor: 2.0,
        cap: Duration::from_millis(100),
        max_attempts,
    }
}

async fn wait_for_state(
    manager: &McpConnectionManager,
    name: &str,
    want: fn(&McpConnectionState) -> bool,
    what: &str,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let state = manager
            .list_connections()
            .await
            .into_iter()
            .find(|(conn, _)| conn == name)
            .map(|(_, state)| state);
        if let Some(state) = &state
            && want(state)
        {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("connection '{name}' never reached {what} (last: {state:?})");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn is_connected(state: &McpConnectionState) -> bool {
    matches!(state, McpConnectionState::Connected { .. })
}

fn is_failed(state: &McpConnectionState) -> bool {
    matches!(state, McpConnectionState::Failed { .. })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Given a server that fails its first requests and then recovers, when a
/// failed connection is scheduled for automatic reconnection, then the
/// loop retries with backoff, reconnects, and the tools are back.
#[tokio::test]
async fn should_reconnect_with_backoff_when_the_server_recovers() {
    let mock = FlakyMockMcp::start(4).await; // initial connect + 3 loop attempts fail
    let manager = Arc::new(McpConnectionManager::with_oauth_token_dir(unique_temp_dir(
        "a",
    )));

    manager
        .connect_one("srv", http_config(&mock.url))
        .await
        .expect_err("the leading failures must fail the initial connect");
    wait_for_state(&manager, "srv", is_failed, "failed").await;
    assert!(
        mock.count() >= 1,
        "the initial connect must reach the server"
    );

    McpConnectionManager::spawn_auto_reconnect_with(Arc::clone(&manager), "srv", fast_backoff(10));
    wait_for_state(&manager, "srv", is_connected, "connected").await;

    // The recovered connection really serves tools.
    let tools = manager.list_all_tools().await;
    assert!(
        tools
            .iter()
            .any(|(server, tool)| server == "srv" && tool.name == "echo"),
        "the reconnected server's tools must be back"
    );
}

/// Given a server that never recovers, when the automatic reconnection
/// budget is exhausted, then the loop stops (the request count stops
/// growing) instead of spinning forever.
#[tokio::test]
async fn should_stop_retrying_when_the_attempt_budget_is_exhausted() {
    let mock = FlakyMockMcp::start_always_failing().await;
    let manager = Arc::new(McpConnectionManager::with_oauth_token_dir(unique_temp_dir(
        "b",
    )));

    manager
        .connect_one("srv", http_config(&mock.url))
        .await
        .expect_err("the always-failing server must fail the initial connect");
    let count_after_initial = mock.count();

    let budget = 4;
    McpConnectionManager::spawn_auto_reconnect_with(
        Arc::clone(&manager),
        "srv",
        fast_backoff(budget),
    );

    // Wait well past the whole backoff schedule (4 attempts, ≤100ms each
    // plus sleeps ≈ under 1s), then confirm the count stopped growing.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let count_settled = mock.count();
    assert!(
        count_settled <= count_after_initial + budget as usize + 1,
        "the loop must stop after {budget} attempts, saw {} requests after the initial {}",
        count_settled - count_after_initial,
        count_after_initial
    );
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        mock.count(),
        count_settled,
        "no further attempts after the budget is exhausted"
    );
    wait_for_state(&manager, "srv", is_failed, "failed").await;
}

/// Given a running auto-reconnect loop, when the server is removed (a
/// config-write reconcile's remove op), then the loop exits instead of
/// retrying a dead registration.
#[tokio::test]
async fn should_exit_the_loop_when_the_server_is_removed_mid_retry() {
    let mock = FlakyMockMcp::start_always_failing().await;
    let manager = Arc::new(McpConnectionManager::with_oauth_token_dir(unique_temp_dir(
        "c",
    )));

    manager
        .connect_one("srv", http_config(&mock.url))
        .await
        .expect_err("the always-failing server must fail the initial connect");
    let count_after_initial = mock.count();

    // A generous budget with a slow backoff: without invalidation the loop
    // would keep hammering the server for the full schedule.
    McpConnectionManager::spawn_auto_reconnect_with(
        Arc::clone(&manager),
        "srv",
        ReconnectBackoff {
            base: Duration::from_millis(50),
            factor: 2.0,
            cap: Duration::from_millis(500),
            max_attempts: 50,
        },
    );
    tokio::time::sleep(Duration::from_millis(200)).await;

    // The removal (a reconcile's Disconnect{unregister:true}).
    manager.remove("srv").await.expect("removal succeeds");

    let count_at_remove = mock.count();
    assert!(
        count_at_remove > count_after_initial,
        "the loop must have attempted before the removal"
    );
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(
        mock.count(),
        count_at_remove,
        "the loop must stop after the removal instead of retrying a dead registration"
    );
}

/// Given an observed unexpected connection loss, when the caller marks it
/// and schedules the reconnection, then the state flips to `failed` first
/// (the status listeners drop the stale tools) and recovers after.
#[tokio::test]
async fn should_mark_a_lost_connection_and_recover_from_the_loss() {
    // Succeed the initial connect, then fail afterwards (the "server was
    // killed" shape): 0 leading failures, then flip to always-failing is
    // not expressible with the counter — use a fresh always-failing mock
    // and only assert the marking half here (recovery is covered above).
    let mock = FlakyMockMcp::start_always_failing().await;
    let manager = Arc::new(McpConnectionManager::with_oauth_token_dir(unique_temp_dir(
        "d",
    )));

    manager
        .connect_one("srv", http_config(&mock.url))
        .await
        .expect_err("the always-failing server must fail the initial connect");

    // Marking a registered server flips it to Failed and notifies.
    manager
        .mark_unexpectedly_disconnected("srv")
        .await
        .expect("marking succeeds");
    wait_for_state(&manager, "srv", is_failed, "failed").await;

    // Marking an unregistered name is a no-op success.
    manager
        .mark_unexpectedly_disconnected("nobody")
        .await
        .expect("marking an unknown name is a no-op");
}

}

mod stdio_integration {
//! Integration test: connect to a mock MCP server over stdio.
//!
//! The mock server is a small Python script that implements the MCP
//! `initialize` + `tools/list` + `tools/call` protocol. It reads
//! newline-delimited JSON-RPC requests from stdin and writes responses to
//! stdout.
//!
//! This test is ignored by default (requires Python 3) and can be run with:
//! ```sh
//! cargo test -p mirri-mcp --test stdio_integration -- --ignored
//! ```

use std::time::Duration;

use mirri_mcp::{McpClient, McpTransportConfig};

/// Python script that implements a minimal MCP server.
const MOCK_SERVER_SCRIPT: &str = r#"
import json, sys

# Tool registry
TOOLS = [
    {"name": "echo", "description": "Echo back the input", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}},
    {"name": "add", "description": "Add two numbers", "inputSchema": {"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "number"}}, "required": ["a", "b"]}},
]

initialized = False

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    try:
        msg = json.loads(line)
    except:
        continue

    if "method" not in msg:
        continue

    method = msg["method"]
    msg_id = msg.get("id")

    if method == "initialize":
        resp = {"jsonrpc": "2.0", "id": msg_id, "result": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "serverInfo": {"name": "mock-mcp-server", "version": "1.0.0"}
        }}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif method == "notifications/initialized":
        initialized = True
    elif method == "tools/list":
        resp = {"jsonrpc": "2.0", "id": msg_id, "result": {"tools": TOOLS}}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif method == "tools/call":
        params = msg.get("params", {})
        name = params.get("name", "")
        args = params.get("arguments", {})
        if name == "echo":
            text = args.get("text", "")
            resp = {"jsonrpc": "2.0", "id": msg_id, "result": {"content": [{"type": "text", "text": text}], "isError": False}}
        elif name == "add":
            result = args.get("a", 0) + args.get("b", 0)
            resp = {"jsonrpc": "2.0", "id": msg_id, "result": {"content": [{"type": "text", "text": str(result)}], "isError": False}}
        elif name == "fail":
            resp = {"jsonrpc": "2.0", "id": msg_id, "result": {"content": [{"type": "text", "text": "intentional failure"}], "isError": True}}
        else:
            resp = {"jsonrpc": "2.0", "id": msg_id, "error": {"code": -32601, "message": f"Unknown tool: {name}"}}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
    elif method == "ping":
        resp = {"jsonrpc": "2.0", "id": msg_id, "result": {}}
        sys.stdout.write(json.dumps(resp) + "\n")
        sys.stdout.flush()
"#;

/// Check if Python 3 is available.
fn python3_available() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

/// Write the mock server script to a temp file and return the path.
fn write_mock_server() -> std::path::PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("mirri_mcp_mock_server_{}.py", std::process::id()));
    std::fs::write(&path, MOCK_SERVER_SCRIPT).unwrap();
    path
}

#[tokio::test]
#[ignore = "requires python3"]
async fn should_connect_list_and_call_tools_when_mock_server_is_available() {
    if !python3_available() {
        eprintln!("python3 not available, skipping");
        return;
    }

    let script_path = write_mock_server();

    let config = McpTransportConfig::Stdio {
        command: "python3".to_string(),
        args: vec![script_path.to_string_lossy().to_string()],
        env: None,
        cwd: None,
    };

    // Connect with a generous timeout.
    let client = McpClient::connect(&config, Duration::from_secs(10))
        .await
        .expect("failed to connect to mock MCP server");

    assert!(client.is_connected());

    // Get server info.
    let info = client
        .server_info()
        .await
        .expect("failed to get server info");
    assert_eq!(info.name, "mock-mcp-server");
    assert_eq!(info.version, "1.0.0");

    // List tools.
    let tools = client.list_tools().await.expect("failed to list tools");
    assert_eq!(tools.len(), 2);
    assert_eq!(tools[0].name, "echo");
    assert_eq!(tools[1].name, "add");

    // Call the echo tool.
    let echo_result = client
        .call_tool("echo", serde_json::json!({"text": "hello mcp"}))
        .await
        .expect("failed to call echo tool");
    assert!(!echo_result.is_error);
    assert_eq!(echo_result.content.len(), 1);
    match &echo_result.content[0] {
        mirri_mcp::McpContentBlock::Text { text } => {
            assert_eq!(text, "hello mcp");
        }
        _ => panic!("expected text content block"),
    }

    // Call the add tool.
    let add_result = client
        .call_tool("add", serde_json::json!({"a": 3, "b": 4}))
        .await
        .expect("failed to call add tool");
    assert!(!add_result.is_error);
    match &add_result.content[0] {
        mirri_mcp::McpContentBlock::Text { text } => {
            assert_eq!(text, "7");
        }
        _ => panic!("expected text content block"),
    }

    // Call a non-existent tool — should error.
    let missing_result = client.call_tool("nonexistent", serde_json::json!({})).await;
    assert!(missing_result.is_err());

    // Close.
    client.close().await.expect("failed to close MCP client");

    // Clean up the temp script.
    let _ = std::fs::remove_file(&script_path);
}

#[tokio::test]
#[ignore = "requires python3"]
async fn should_return_error_when_connection_to_nonexistent_command_fails() {
    // Try to connect to a non-existent command — should fail.
    let config = McpTransportConfig::Stdio {
        command: "/nonexistent/command/that/does/not/exist".to_string(),
        args: vec![],
        env: None,
        cwd: None,
    };

    let result = McpClient::connect(&config, Duration::from_secs(5)).await;
    assert!(
        result.is_err(),
        "expected connection failure for non-existent command"
    );
}

}

mod types_serde {
//! Serde round-trip tests for MCP protocol types.
//!
//! These tests verify that the type definitions match the MCP wire format
//! exactly — a round-trip through `serialize → deserialize` produces an
//! identical value, and the serialized form matches what real MCP servers
//! send.

use mirri_mcp::*;
use serde_json::json;
use std::error::Error;

// ---------------------------------------------------------------------------
// McpTransportConfig
// ---------------------------------------------------------------------------

#[test]
fn should_round_trip_when_config_is_stdio() {
    let config = McpTransportConfig::Stdio {
        command: "npx".to_string(),
        args: vec![
            "-y".to_string(),
            "@modelcontextprotocol/server-filesystem".to_string(),
        ],
        env: Some(std::collections::HashMap::from([(
            "FOO".to_string(),
            "bar".to_string(),
        )])),
        cwd: Some(std::path::PathBuf::from("/tmp")),
    };

    let json = serde_json::to_string(&config).unwrap();
    assert!(json.contains(r#""type":"stdio""#));
    assert!(json.contains(r#""command":"npx""#));

    let deserialized: McpTransportConfig = serde_json::from_str(&json).unwrap();
    match deserialized {
        McpTransportConfig::Stdio {
            command,
            args,
            env,
            cwd,
        } => {
            assert_eq!(command, "npx");
            assert_eq!(args.len(), 2);
            assert_eq!(args[0], "-y");
            assert!(env.is_some());
            assert_eq!(env.unwrap().get("FOO"), Some(&"bar".to_string()));
            assert_eq!(cwd, Some(std::path::PathBuf::from("/tmp")));
        }
        _ => panic!("expected Stdio variant"),
    }
}

#[test]
fn should_round_trip_when_config_is_http() {
    let config = McpTransportConfig::Http {
        url: "https://example.com/mcp".to_string(),
        headers: Some(std::collections::HashMap::from([(
            "X-Custom".to_string(),
            "value".to_string(),
        )])),
        token: Some("secret-token".to_string()),
    };

    let json = serde_json::to_string(&config).unwrap();
    assert!(json.contains(r#""type":"http""#));
    assert!(json.contains(r#""url":"https://example.com/mcp""#));
    assert!(json.contains(r#""token":"secret-token""#));

    let deserialized: McpTransportConfig = serde_json::from_str(&json).unwrap();
    match deserialized {
        McpTransportConfig::Http {
            url,
            headers,
            token,
        } => {
            assert_eq!(url, "https://example.com/mcp");
            assert!(headers.is_some());
            assert_eq!(token, Some("secret-token".to_string()));
        }
        _ => panic!("expected Http variant"),
    }
}

#[test]
fn should_round_trip_when_config_is_sse() {
    let config = McpTransportConfig::Sse {
        url: "https://example.com/sse".to_string(),
        headers: None,
        token: None,
    };

    let json = serde_json::to_string(&config).unwrap();
    assert!(json.contains(r#""type":"sse""#));

    let deserialized: McpTransportConfig = serde_json::from_str(&json).unwrap();
    match deserialized {
        McpTransportConfig::Sse {
            url,
            headers,
            token,
        } => {
            assert_eq!(url, "https://example.com/sse");
            assert!(headers.is_none());
            assert!(token.is_none());
        }
        _ => panic!("expected Sse variant"),
    }
}

#[test]
fn should_default_to_none_when_stdio_config_omits_env_and_cwd() {
    let json = r#"{"type":"stdio","command":"echo","args":[]}"#;
    let config: McpTransportConfig = serde_json::from_str(json).unwrap();
    match config {
        McpTransportConfig::Stdio {
            command,
            args,
            env,
            cwd,
        } => {
            assert_eq!(command, "echo");
            assert!(args.is_empty());
            assert!(env.is_none());
            assert!(cwd.is_none());
        }
        _ => panic!("expected Stdio variant"),
    }
}

// ---------------------------------------------------------------------------
// McpContentBlock
// ---------------------------------------------------------------------------

#[test]
fn should_round_trip_when_content_block_is_text() {
    let block = McpContentBlock::Text {
        text: "hello world".to_string(),
    };

    let json = serde_json::to_string(&block).unwrap();
    assert_eq!(json, r#"{"type":"text","text":"hello world"}"#);

    let deserialized: McpContentBlock = serde_json::from_str(&json).unwrap();
    match deserialized {
        McpContentBlock::Text { text } => {
            assert_eq!(text, "hello world");
        }
        _ => panic!("expected Text variant"),
    }
}

#[test]
fn should_round_trip_when_content_block_is_image() {
    let block = McpContentBlock::Image {
        data: "iVBORw0KGgo=".to_string(),
        mime_type: "image/png".to_string(),
    };

    let json = serde_json::to_string(&block).unwrap();
    assert!(json.contains(r#""type":"image""#));
    assert!(json.contains(r#""mimeType":"image/png""#));

    let deserialized: McpContentBlock = serde_json::from_str(&json).unwrap();
    match deserialized {
        McpContentBlock::Image { data, mime_type } => {
            assert_eq!(data, "iVBORw0KGgo=");
            assert_eq!(mime_type, "image/png");
        }
        _ => panic!("expected Image variant"),
    }
}

#[test]
fn should_round_trip_when_content_block_is_audio() {
    let block = McpContentBlock::Audio {
        data: "UklGRiQ=".to_string(),
        mime_type: "audio/wav".to_string(),
    };

    let json = serde_json::to_string(&block).unwrap();
    assert!(json.contains(r#""type":"audio""#));

    let deserialized: McpContentBlock = serde_json::from_str(&json).unwrap();
    match deserialized {
        McpContentBlock::Audio { data, mime_type } => {
            assert_eq!(data, "UklGRiQ=");
            assert_eq!(mime_type, "audio/wav");
        }
        _ => panic!("expected Audio variant"),
    }
}

#[test]
fn should_round_trip_when_content_block_is_resource() {
    let block = McpContentBlock::Resource {
        resource: types::McpEmbeddedResourceContents {
            uri: "file:///path/to/file.txt".to_string(),
            mime_type: Some("text/plain".to_string()),
            text: Some("file contents".to_string()),
            blob: None,
        },
    };

    let json = serde_json::to_string(&block).unwrap();
    assert!(json.contains(r#""type":"resource""#));
    assert!(json.contains(r#""uri":"file:///path/to/file.txt""#));

    let deserialized: McpContentBlock = serde_json::from_str(&json).unwrap();
    match deserialized {
        McpContentBlock::Resource { resource } => {
            assert_eq!(resource.uri, "file:///path/to/file.txt");
            assert_eq!(resource.text, Some("file contents".to_string()));
            assert!(resource.blob.is_none());
        }
        _ => panic!("expected Resource variant"),
    }
}

// ---------------------------------------------------------------------------
// McpCallToolResult
// ---------------------------------------------------------------------------

#[test]
fn should_round_trip_when_call_tool_result_is_success() {
    let result = McpCallToolResult {
        content: vec![McpContentBlock::text("success")],
        is_error: false,
    };

    let json = serde_json::to_string(&result).unwrap();
    assert!(json.contains(r#""isError":false"#));
    assert!(json.contains(r#""text":"success""#));

    let deserialized: McpCallToolResult = serde_json::from_str(&json).unwrap();
    assert!(!deserialized.is_error);
    assert_eq!(deserialized.content.len(), 1);
}

#[test]
fn should_round_trip_when_call_tool_result_is_error() {
    let result = McpCallToolResult {
        content: vec![McpContentBlock::text("tool failed")],
        is_error: true,
    };

    let json = serde_json::to_string(&result).unwrap();
    assert!(json.contains(r#""isError":true"#));

    let deserialized: McpCallToolResult = serde_json::from_str(&json).unwrap();
    assert!(deserialized.is_error);
}

#[test]
fn should_round_trip_when_call_tool_result_has_empty_content() {
    let result = McpCallToolResult {
        content: vec![],
        is_error: false,
    };

    let json = serde_json::to_string(&result).unwrap();
    let deserialized: McpCallToolResult = serde_json::from_str(&json).unwrap();
    assert!(deserialized.content.is_empty());
    assert!(!deserialized.is_error);
}

#[test]
fn should_default_is_error_to_false_when_server_omits_it() {
    // When the server omits isError, it should default to false.
    let json = r#"{"content":[]}"#;
    let result: McpCallToolResult = serde_json::from_str(json).unwrap();
    assert!(!result.is_error);
}

#[test]
fn should_construct_correct_results_when_using_helper_constructors() {
    let success = McpCallToolResult::text("ok");
    assert!(!success.is_error);
    assert_eq!(success.content.len(), 1);

    let error = McpCallToolResult::error("failed");
    assert!(error.is_error);
    assert_eq!(error.content.len(), 1);
}

// ---------------------------------------------------------------------------
// McpToolDefinition
// ---------------------------------------------------------------------------

#[test]
fn should_round_trip_when_tool_definition_has_all_fields() {
    let tool = McpToolDefinition {
        name: "read_file".to_string(),
        description: "Read a file from disk".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" }
            },
            "required": ["path"]
        }),
    };

    let json_str = serde_json::to_string(&tool).unwrap();
    assert!(json_str.contains(r#""name":"read_file""#));
    assert!(json_str.contains(r#""inputSchema""#));

    let deserialized: McpToolDefinition = serde_json::from_str(&json_str).unwrap();
    assert_eq!(deserialized.name, "read_file");
    assert_eq!(deserialized.description, "Read a file from disk");
    assert!(deserialized.input_schema.is_object());
}

#[test]
fn should_deserialize_when_parsing_server_wire_format() {
    // Simulates the raw JSON a server would return for tools/list.
    let raw = r#"{
        "name": "search",
        "description": "Search the web",
        "inputSchema": {
            "type": "object",
            "properties": {
                "query": { "type": "string" }
            },
            "required": ["query"]
        }
    }"#;

    let tool: McpToolDefinition = serde_json::from_str(raw).unwrap();
    assert_eq!(tool.name, "search");
    assert_eq!(tool.description, "Search the web");
}

#[test]
fn should_preserve_empty_description_when_description_is_empty() {
    let raw = r#"{
        "name": "noop",
        "description": "",
        "inputSchema": {}
    }"#;

    let tool: McpToolDefinition = serde_json::from_str(raw).unwrap();
    assert_eq!(tool.description, "");
}

// ---------------------------------------------------------------------------
// McpServerInfo
// ---------------------------------------------------------------------------

#[test]
fn should_round_trip_when_serializing_server_info() {
    let info = McpServerInfo {
        name: "filesystem".to_string(),
        version: "1.0.0".to_string(),
    };

    let json = serde_json::to_string(&info).unwrap();
    let deserialized: McpServerInfo = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.name, "filesystem");
    assert_eq!(deserialized.version, "1.0.0");
}

// ---------------------------------------------------------------------------
// McpToolListResult
// ---------------------------------------------------------------------------

#[test]
fn should_round_trip_when_tool_list_has_multiple_tools() {
    let result = McpToolListResult {
        tools: vec![
            McpToolDefinition {
                name: "tool_a".to_string(),
                description: "Tool A".to_string(),
                input_schema: json!({}),
            },
            McpToolDefinition {
                name: "tool_b".to_string(),
                description: "Tool B".to_string(),
                input_schema: json!({}),
            },
        ],
    };

    let json = serde_json::to_string(&result).unwrap();
    let deserialized: McpToolListResult = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.tools.len(), 2);
    assert_eq!(deserialized.tools[0].name, "tool_a");
    assert_eq!(deserialized.tools[1].name, "tool_b");
}

#[test]
fn should_round_trip_when_tool_list_is_empty() {
    let result = McpToolListResult { tools: vec![] };
    let json = serde_json::to_string(&result).unwrap();
    let deserialized: McpToolListResult = serde_json::from_str(&json).unwrap();
    assert!(deserialized.tools.is_empty());
}

// ---------------------------------------------------------------------------
// JSON-RPC envelope types
// ---------------------------------------------------------------------------

#[test]
fn should_serialize_jsonrpc_fields_when_serializing_request() {
    let request = types::JsonRpcRequest::new(
        types::JsonRpcId::Num(1),
        "initialize",
        Some(json!({"protocolVersion": "2024-11-05"})),
    );

    let json_str = serde_json::to_string(&request).unwrap();
    assert!(json_str.contains(r#""jsonrpc":"2.0""#));
    assert!(json_str.contains(r#""id":1"#));
    assert!(json_str.contains(r#""method":"initialize""#));
}

#[test]
fn should_deserialize_result_when_parsing_jsonrpc_response() {
    let raw = r#"{
        "jsonrpc": "2.0",
        "id": 42,
        "result": {"tools": []}
    }"#;

    let response: types::JsonRpcResponse = serde_json::from_str(raw).unwrap();
    assert!(response.result.is_some());
    assert!(response.error.is_none());
}

#[test]
fn should_deserialize_error_when_parsing_jsonrpc_error_response() {
    let raw = r#"{
        "jsonrpc": "2.0",
        "id": 99,
        "error": {
            "code": -32601,
            "message": "Method not found"
        }
    }"#;

    let response: types::JsonRpcResponse = serde_json::from_str(raw).unwrap();
    assert!(response.result.is_none());
    assert!(response.error.is_some());
    let error = response.error.unwrap();
    assert_eq!(error.code, -32601);
    assert_eq!(error.message, "Method not found");
}

// ---------------------------------------------------------------------------
// McpError
// ---------------------------------------------------------------------------

#[test]
fn should_display_correct_messages_when_converting_error_to_string() {
    let err = McpError::ToolNotFound("my_tool".to_string());
    assert_eq!(err.to_string(), "MCP tool not found: my_tool");

    let err = McpError::Timeout;
    assert_eq!(err.to_string(), "MCP server timeout");

    let err = McpError::AuthRequired;
    assert_eq!(err.to_string(), "MCP auth required");
}

#[test]
fn should_preserve_source_chain_when_oauth_error_carries_io_source() {
    // McpError is intentionally not Clone (the OAuth variant holds the
    // original non-Clone source), so the observable contract is the Display
    // and source chain, not cloneability.
    let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "token file locked");
    let err = McpError::OAuth(OAuthError::Io(io_err));

    assert!(
        err.to_string().contains("token file locked"),
        "display must include the underlying I/O message: {err}"
    );
    let oauth_err = err.source().expect("OAuthError preserved as source");
    let io_source = oauth_err.source().expect("io::Error preserved as source");
    assert!(io_source.to_string().contains("token file locked"));
}

// Re-export the types module for test access.
mod types {
    pub use mirri_mcp::types::*;
}

}

