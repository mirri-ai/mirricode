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
