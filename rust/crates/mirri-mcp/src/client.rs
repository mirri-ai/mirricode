//! MCP client — the primary user-facing API for connecting to an MCP server,
//! discovering tools, and calling them.
//!
//! The client wraps a [`crate::transport::McpTransport`] and implements the
//! JSON-RPC 2.0 protocol on top of it:
//!
//! 1. `connect` — establishes the transport, sends `initialize`, sends
//!    `notifications/initialized`
//! 2. `list_tools` — sends `tools/list` and deserializes the result
//! 3. `call_tool` — sends `tools/call` with the tool name and arguments
//! 4. `server_info` — returns the cached server identity from the handshake
//! 5. `close` — cleanly shuts down the transport
//!
//! Each request is assigned a monotonic id and serialized via serde. The
//! transport handles the actual I/O.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::Mutex as TokioMutex;

use tracing::{debug, instrument};

use crate::error::{McpError, McpResult};
use crate::transport::{McpTransport, McpTransportConfig};
use crate::types::{
    ClientInfo, InitializeParams, InitializeResult, JsonRpcId, JsonRpcRequest, JsonRpcResponse,
    McpCallToolResult, McpServerInfo, McpToolDefinition, McpToolListResult,
};

/// Default timeout for the `initialize` handshake.
const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Default timeout for `tools/list` and `tools/call`.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// The MCP protocol version this client speaks.
const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// Client identity sent during `initialize`.
const CLIENT_NAME: &str = "mirri-code";
const CLIENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// MCP client — connects to one MCP server, discovers tools, and calls them.
///
/// A client owns exactly one transport. Once `close` is called the client is
/// no longer usable. The client is `Send + Sync` so it can be shared across
/// async tasks via `Arc`.
pub struct McpClient {
    /// The underlying transport. Guarded by a tokio mutex so the guard
    /// can be held across `.await` points (the transport's `send_request`
    /// returns a boxed future).
    transport: TokioMutex<Box<dyn McpTransport>>,
    /// Monotonic request id counter.
    next_id: AtomicU64,
    /// Server info cached from the `initialize` handshake.
    server_info: Mutex<Option<McpServerInfo>>,
    /// Whether `close` has been called.
    closed: std::sync::atomic::AtomicBool,
}

impl McpClient {
    /// Create and connect to an MCP server.
    ///
    /// This establishes the transport, performs the JSON-RPC `initialize`
    /// handshake, and sends the `notifications/initialized` notification.
    /// If the handshake does not complete within `timeout`, returns
    /// [`McpError::Timeout`].
    ///
    /// If `timeout` is `Duration::ZERO`, a default of 30 seconds is used.
    #[instrument(skip(config), fields(transport = ?config.transport_type_name()))]
    pub async fn connect(config: &McpTransportConfig, timeout: Duration) -> McpResult<Self> {
        Self::connect_with_token_provider(config, timeout, None).await
    }

    /// Connect with a dynamic token provider attached to remote transports.
    ///
    /// The OAuth wiring path: the transport fetches a fresh
    /// `Authorization: Bearer` token from `token_provider` before each
    /// request so an expired token is refreshed transparently. A `None`
    /// provider behaves exactly like [`Self::connect`].
    pub async fn connect_with_token_provider(
        config: &McpTransportConfig,
        timeout: Duration,
        token_provider: Option<Arc<dyn crate::transport::TokenProvider>>,
    ) -> McpResult<Self> {
        let connect_timeout = if timeout.is_zero() {
            DEFAULT_CONNECT_TIMEOUT
        } else {
            timeout
        };

        debug!("connecting to MCP server");

        let transport =
            crate::transport::create_transport_with_token_provider(config, token_provider).await?;

        let mut client = McpClient {
            transport: TokioMutex::new(transport),
            next_id: AtomicU64::new(1),
            server_info: Mutex::new(None),
            closed: std::sync::atomic::AtomicBool::new(false),
        };

        // Perform the initialize handshake.
        client.initialize(connect_timeout).await?;

        Ok(client)
    }

    /// Perform the JSON-RPC `initialize` handshake.
    async fn initialize(&mut self, timeout: Duration) -> McpResult<()> {
        let params = InitializeParams {
            protocol_version: MCP_PROTOCOL_VERSION.to_string(),
            capabilities: serde_json::json!({}),
            client_info: ClientInfo {
                name: CLIENT_NAME.to_string(),
                version: CLIENT_VERSION.to_string(),
            },
        };

        let response = self
            .send_request(
                "initialize",
                Some(serde_json::to_value(&params).map_err(|e| {
                    McpError::Protocol(format!("failed to serialize initialize params: {e}"))
                })?),
                timeout,
            )
            .await?;

        let init_result: InitializeResult =
            serde_json::from_value(response.result.unwrap_or_default()).map_err(|e| {
                McpError::Protocol(format!("failed to deserialize initialize result: {e}"))
            })?;

        debug!(
            server_name = %init_result.server_info.name,
            server_version = %init_result.server_info.version,
            protocol_version = %init_result.protocol_version,
            "MCP server initialized"
        );

        // Cache server info.
        {
            let mut info = self.server_info.lock().unwrap();
            *info = Some(init_result.server_info);
        }

        // Send the `notifications/initialized` notification. This is a
        // JSON-RPC notification (no id, no response expected). The
        // transport's `send_notification` writes it without registering a
        // pending response channel.
        let _ = self
            .send_notification("notifications/initialized", None)
            .await;

        Ok(())
    }

    /// Discover the tools available on the server.
    ///
    /// Sends `tools/list` and returns the tool definitions. Uses the
    /// default request timeout.
    pub async fn list_tools(&self) -> McpResult<Vec<McpToolDefinition>> {
        let response = self
            .send_request("tools/list", None, DEFAULT_REQUEST_TIMEOUT)
            .await?;

        let list_result: McpToolListResult =
            serde_json::from_value(response.result.unwrap_or_default()).map_err(|e| {
                McpError::Protocol(format!("failed to deserialize tools/list result: {e}"))
            })?;

        Ok(list_result.tools)
    }

    /// Call a tool by name.
    ///
    /// `arguments` should be a JSON object matching the tool's input schema.
    /// If the tool returns `isError: true`, the result is still returned
    /// (not an `Err`) — the caller should check `is_error` on the result.
    /// An `Err` is returned only for transport/protocol failures.
    pub async fn call_tool(
        &self,
        name: &str,
        arguments: serde_json::Value,
    ) -> McpResult<McpCallToolResult> {
        let params = serde_json::json!({
            "name": name,
            "arguments": arguments,
        });

        let response = self
            .send_request("tools/call", Some(params), DEFAULT_REQUEST_TIMEOUT)
            .await?;

        // Check for JSON-RPC error.
        if let Some(error) = &response.error {
            if error.code == -32601 {
                return Err(McpError::ToolNotFound(name.to_string()));
            }
            return Err(McpError::ToolExecutionFailed(format!(
                "{} (code {})",
                error.message, error.code
            )));
        }

        let result: McpCallToolResult = serde_json::from_value(response.result.unwrap_or_default())
            .map_err(|e| {
                McpError::Protocol(format!("failed to deserialize tools/call result: {e}"))
            })?;

        Ok(result)
    }

    /// Get the server's identity (name + version) from the cached
    /// `initialize` response.
    ///
    /// Returns `Disconnected` if the client hasn't been connected yet.
    pub async fn server_info(&self) -> McpResult<McpServerInfo> {
        let info = self.server_info.lock().unwrap();
        info.clone().ok_or(McpError::Disconnected)
    }

    /// Close the connection and release all resources.
    ///
    /// After this call, the client is no longer usable.
    pub async fn close(&self) -> McpResult<()> {
        self.closed.store(true, Ordering::Relaxed);
        let mut transport = self.transport.lock().await;
        transport.close().await?;
        debug!("MCP client closed");
        Ok(())
    }

    /// Check whether the connection is still alive.
    pub fn is_connected(&self) -> bool {
        if self.closed.load(Ordering::Relaxed) {
            return false;
        }
        // Use try_lock to avoid blocking — if the lock is contended
        // (a request is in-flight), assume connected.
        match self.transport.try_lock() {
            Ok(transport) => transport.is_connected(),
            Err(_) => true,
        }
    }

    /// Capture a snapshot of the child process's stderr, if the transport
    /// supports it (stdio only). Returns an empty string for HTTP/SSE.
    pub fn stderr_snapshot(&self) -> String {
        match self.transport.try_lock() {
            Ok(transport) => transport.stderr_snapshot(),
            Err(_) => String::new(),
        }
    }

    // -- Internal JSON-RPC helpers -------------------------------------------

    /// Allocate the next request id.
    fn next_request_id(&self) -> JsonRpcId {
        JsonRpcId::Num(self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    /// Send a JSON-RPC request and return the response.
    ///
    /// The request is serialized, sent through the transport, and the
    /// response is checked for JSON-RPC-level errors.
    async fn send_request(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
        timeout: Duration,
    ) -> McpResult<JsonRpcResponse> {
        let id = self.next_request_id();
        let request = JsonRpcRequest::new(id, method, params);
        let request_json = serde_json::to_string(&request)
            .map_err(|e| McpError::Protocol(format!("failed to serialize request: {e}")))?;

        let response = {
            let mut transport = self.transport.lock().await;
            transport.send_request(&request_json, timeout).await?
        };

        // If the response has a JSON-RPC error, convert it to McpError.
        if let Some(error) = &response.error {
            return Err(McpError::Protocol(format!(
                "JSON-RPC error: {} (code {})",
                error.message, error.code
            )));
        }

        Ok(response)
    }

    /// Send a JSON-RPC notification (no id, no response expected).
    ///
    /// Uses the transport's `send_notification` method which writes the
    /// notification without registering a pending response channel.
    async fn send_notification(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> McpResult<()> {
        let notification = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params.unwrap_or(serde_json::Value::Null),
        });
        let notification_json = serde_json::to_string(&notification)
            .map_err(|e| McpError::Protocol(format!("failed to serialize notification: {e}")))?;

        let mut transport = self.transport.lock().await;
        transport.send_notification(&notification_json).await
    }
}

/// Extension methods for [`McpTransportConfig`] used internally.
impl McpTransportConfig {
    /// Return a string label for the transport type, for logging.
    fn transport_type_name(&self) -> &'static str {
        match self {
            McpTransportConfig::Stdio { .. } => "stdio",
            McpTransportConfig::Http { .. } => "http",
            McpTransportConfig::Sse { .. } => "sse",
        }
    }
}

impl std::fmt::Debug for McpClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpClient")
            .field("connected", &self.is_connected())
            .field("server_info", &self.server_info.lock().unwrap())
            .finish()
    }
}
