//! A minimal MCP stdio server used as a test fixture by the ACP
//! conformance suite.
//!
//! It speaks just enough of the MCP protocol for
//! [`McpConnectionManager`](mirri_mcp::McpConnectionManager) to connect and
//! discover its single tool: `initialize` → `tools/list` → `tools/call`.
//! The tool echoes back its `text` argument.
//!
//! The tool's name is taken from the first command-line argument
//! (defaulting to `echo`), so tests can distinguish multiple instances of
//! this server by giving each a different tool name (the shadowing tests).
//!
//! Not a `[[test]]` target: this binary is spawned as a child process by
//! the engine under test (an MCP server config's `command`), never run
//! directly.

use std::io::{BufRead, Write};

fn main() {
    let tool_name = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "echo".to_string());
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        // Notifications (no id) are never answered.
        let Some(id) = message.get("id").cloned() else {
            continue;
        };
        let method = message.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let response = match method {
            "initialize" => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "serverInfo": {"name": "mirri-acp-echo-mcp", "version": "1.0.0"}
                }
            }),
            "tools/list" => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "tools": [{
                        "name": tool_name,
                        "description": "Echo back the input",
                        "inputSchema": {
                            "type": "object",
                            "properties": {"text": {"type": "string"}},
                            "required": ["text"],
                        },
                    }]
                }
            }),
            "tools/call" => {
                let params = message.get("params").cloned().unwrap_or_default();
                let text = params
                    .pointer("/arguments/text")
                    .and_then(|t| t.as_str())
                    .unwrap_or_default();
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "content": [{"type": "text", "text": text}],
                        "isError": false,
                    }
                })
            }
            "ping" => serde_json::json!({"jsonrpc": "2.0", "id": id, "result": {}}),
            other => serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": -32601, "message": format!("unknown method: {other}")}
            }),
        };
        let mut stdout = std::io::stdout().lock();
        let _ = writeln!(stdout, "{response}");
        let _ = stdout.flush();
    }
}
