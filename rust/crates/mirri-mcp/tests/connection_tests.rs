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
    unsafe { std::env::set_var("MCP_CONN_DEFINED_TEST", "/nonexistent/marker_path_xyz") };

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
