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
