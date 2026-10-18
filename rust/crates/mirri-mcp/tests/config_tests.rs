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

    // SAFETY: Test-only env vars.
    unsafe { std::env::set_var("MCP_TEST_CMD", "expanded-command") };
    unsafe { std::env::set_var("MCP_TEST_TOKEN", "secret-token-123") };

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

    // SAFETY: Test-only env var.
    unsafe { std::env::set_var("MCP_TEST_ENV_PREFIX_CMD", "env-prefix-command") };

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
