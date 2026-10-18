//! MCP protocol types.
//!
//! These types mirror the wire shapes defined by the Model Context Protocol
//! specification (2024-11-05 and later drafts). They are intentionally
//! decoupled from any SDK so the crate can evolve without churning callers.
//!
//! All types derive `serde::Serialize` / `Deserialize` and use `snake_case`
//! field names to match the JSON-RPC wire format exactly (the MCP spec uses
//! snake_case on the wire).

use std::fmt;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Tool definition
// ---------------------------------------------------------------------------

/// A tool advertised by an MCP server via `tools/list`.
///
/// `input_schema` is kept as a raw [`serde_json::Value`] because JSON Schema
/// is far richer than any Rust type system can express, and callers typically
/// forward it verbatim to an LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolDefinition {
    /// Unique tool name within the server.
    pub name: String,
    /// Human-readable description; may be empty if the server omits it.
    #[serde(default)]
    pub description: String,
    /// JSON Schema describing the tool's `arguments` object.
    #[serde(rename = "inputSchema")]
    pub input_schema: serde_json::Value,
}

// ---------------------------------------------------------------------------
// Content blocks (tool call results)
// ---------------------------------------------------------------------------

/// Inline resource contents nested under an `EmbeddedResource` content block.
///
/// Exactly one of `text` or `blob` is populated, per the MCP schema's
/// `TextResourceContents | BlobResourceContents` union.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpEmbeddedResourceContents {
    /// URI identifying the resource (e.g. `file:///path/to/file.txt`).
    pub uri: String,
    /// Optional MIME type hint.
    #[serde(rename = "mimeType", default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// Present when the resource is textual.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// Present when the resource is binary (base64-encoded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob: Option<String>,
}

/// A single content block returned by `tools/call`.
///
/// The `type` field is the serde tag — each variant maps to one MCP content
/// type. Unknown variants are captured by `Unknown` so forward compatibility
/// is preserved when servers add new content types.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum McpContentBlock {
    /// Plain text result.
    #[serde(rename = "text")]
    Text {
        /// The text content.
        text: String,
    },
    /// Image result (base64-encoded).
    #[serde(rename = "image")]
    Image {
        /// Base64-encoded image data.
        data: String,
        /// MIME type of the image (e.g. `image/png`).
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    /// Audio result (base64-encoded).
    #[serde(rename = "audio")]
    Audio {
        /// Base64-encoded audio data.
        data: String,
        /// MIME type of the audio (e.g. `audio/wav`).
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
    /// Embedded resource reference.
    #[serde(rename = "resource")]
    Resource {
        /// The resource contents.
        resource: McpEmbeddedResourceContents,
    },
}

impl fmt::Display for McpContentBlock {
    /// Convenience: extract the text from a `Text` block, or a JSON
    /// representation of any other block. Useful for logging.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            McpContentBlock::Text { text } => write!(f, "{}", text),
            other => write!(f, "{}", serde_json::to_string(other).unwrap_or_default()),
        }
    }
}

impl McpContentBlock {
    /// Create a simple text content block.
    pub fn text<S: Into<String>>(content: S) -> Self {
        McpContentBlock::Text {
            text: content.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Tool call result
// ---------------------------------------------------------------------------

/// The result of calling a tool via `tools/call`.
///
/// When `is_error` is `true` the tool ran but reported a failure — the
/// `content` blocks typically contain an error message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpCallToolResult {
    /// Content blocks returned by the tool.
    pub content: Vec<McpContentBlock>,
    /// `true` when the tool itself signals an error.
    #[serde(rename = "isError", default)]
    pub is_error: bool,
}

impl McpCallToolResult {
    /// Build a successful text result.
    pub fn text<S: Into<String>>(content: S) -> Self {
        McpCallToolResult {
            content: vec![McpContentBlock::text(content)],
            is_error: false,
        }
    }

    /// Build an error result with a text message.
    pub fn error<S: Into<String>>(message: S) -> Self {
        McpCallToolResult {
            content: vec![McpContentBlock::text(message)],
            is_error: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Server info
// ---------------------------------------------------------------------------

/// Server identity returned by the `initialize` handshake.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerInfo {
    /// Server name (e.g. `filesystem`).
    pub name: String,
    /// Server version (e.g. `1.0.0`).
    pub version: String,
}

// ---------------------------------------------------------------------------
// Tool list result
// ---------------------------------------------------------------------------

/// The raw `tools/list` response envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpToolListResult {
    /// The tools advertised by the server.
    pub tools: Vec<McpToolDefinition>,
}

// ---------------------------------------------------------------------------
// JSON-RPC envelope (internal but exposed for tests)
// ---------------------------------------------------------------------------

/// A JSON-RPC 2.0 request id. The spec allows string, number, or null.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum JsonRpcId {
    /// Numeric id (most common).
    Num(u64),
    /// String id.
    Str(String),
    /// Null id (used in notifications per spec, though we don't send those).
    Null,
}

/// JSON-RPC 2.0 request envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// Request id.
    pub id: JsonRpcId,
    /// Method name (e.g. `"initialize"`, `"tools/list"`).
    pub method: String,
    /// Method parameters. Omitted when the method takes no params.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

/// JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    /// Numeric error code (negative for protocol errors, positive for
    /// server-defined errors).
    pub code: i32,
    /// Short error message.
    pub message: String,
    /// Optional additional data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// JSON-RPC 2.0 response envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    /// Always `"2.0"`.
    pub jsonrpc: String,
    /// Matches the request id.
    pub id: JsonRpcId,
    /// Present on success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    /// Present on error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcRequest {
    /// Create a new request with the given id, method, and optional params.
    pub fn new(
        id: JsonRpcId,
        method: impl Into<String>,
        params: Option<serde_json::Value>,
    ) -> Self {
        JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id,
            method: method.into(),
            params,
        }
    }
}

/// Parameters for the `initialize` JSON-RPC method.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeParams {
    /// Client identity.
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    /// Client capabilities.
    pub capabilities: serde_json::Value,
    /// Client info.
    #[serde(rename = "clientInfo")]
    pub client_info: ClientInfo,
}

/// Client info sent during initialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientInfo {
    /// Client name.
    pub name: String,
    /// Client version.
    pub version: String,
}

/// The result of the `initialize` method.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeResult {
    /// Server name and version.
    #[serde(rename = "serverInfo")]
    pub server_info: McpServerInfo,
    /// Protocol version negotiated.
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
}
