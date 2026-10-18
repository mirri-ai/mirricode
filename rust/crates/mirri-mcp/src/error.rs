//! MCP error types.
//!
//! All errors produced by the MCP client layer are represented by [`McpError`].
//! The variants map 1:1 to the failure modes documented in the task spec:
//! connection failures, transport I/O, protocol-level mismatches, tool
//! resolution, tool execution, timeouts, disconnects, and auth requirements.

/// Every error that can arise from interacting with an MCP server.
///
/// Each variant carries enough context for the caller to decide whether to
/// retry, surface to the user, or treat the condition as terminal.
///
/// Not `Clone`: the `OAuth` variant carries the original [`OAuthError`], whose
/// `Http`/`Io` sub-variants hold non-`Clone` sources (`#[from]`) to preserve
/// error chains.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// The initial handshake (JSON-RPC `initialize`) failed or the transport
    /// could not be established at all.
    #[error("MCP connection failed: {0}")]
    ConnectionFailed(String),

    /// Low-level I/O failure on the transport (broken pipe, process exited,
    /// HTTP connection reset, etc.).
    #[error("MCP transport error: {0}")]
    Transport(String),

    /// The server returned a well-formed JSON-RPC response that violated the
    /// MCP protocol contract (unexpected method, missing field, bad type).
    #[error("MCP protocol error: {0}")]
    Protocol(String),

    /// A configuration-file failure on the management surface
    /// (`mcp.config.*`): an `mcp.json` layer that cannot be read, parsed,
    /// or persisted.
    #[error("MCP config error: {0}")]
    Config(String),

    /// The wire payload failed validation on the management surface: an
    /// illegal server config, or a tool-filter patch targeting a server no
    /// layer defines.
    #[error("MCP invalid input: {0}")]
    InvalidInput(String),

    /// A tool name was passed to [`crate::client::McpClient::call_tool`] that
    /// the server does not advertise.
    #[error("MCP tool not found: {0}")]
    ToolNotFound(String),

    /// The server executed the tool but the tool itself reported a failure
    /// (the MCP `isError: true` flag), or the server returned a JSON-RPC
    /// error for the `tools/call` request.
    #[error("MCP tool execution failed: {0}")]
    ToolExecutionFailed(String),

    /// An operation did not complete within the configured timeout.
    #[error("MCP server timeout")]
    Timeout,

    /// The transport closed unexpectedly after a successful connection.
    #[error("MCP server disconnected")]
    Disconnected,

    /// The server requires authentication (HTTP 401 / 403) before tools can be
    /// listed or called.
    #[error("MCP auth required")]
    AuthRequired,

    /// An OAuth 2.0 flow failure: discovery, dynamic client registration,
    /// the browser callback, token exchange, or token refresh.
    #[error("MCP OAuth error: {0}")]
    OAuth(#[from] crate::oauth::OAuthError),
}

/// Convenience alias used throughout the crate.
pub type McpResult<T> = Result<T, McpError>;

impl McpError {
    /// Convert a generic `std::io::Error` into the most specific `McpError`.
    ///
    /// Timeout classifications are left to the caller — `io::Error` does not
    /// carry enough metadata to distinguish a timeout from a broken pipe
    /// reliably, so we classify everything as `Transport` unless the kind is
    /// `TimedOut`.
    #[allow(dead_code)]
    pub(crate) fn from_io(error: std::io::Error) -> Self {
        if error.kind() == std::io::ErrorKind::TimedOut {
            McpError::Timeout
        } else {
            McpError::Transport(error.to_string())
        }
    }

    /// Convert a `reqwest::Error` into the most specific `McpError`.
    ///
    /// HTTP 401/403 surfaces that reach reqwest as error are mapped to
    /// `AuthRequired`; timeouts to `Timeout`; everything else to `Transport`.
    pub(crate) fn from_reqwest(error: reqwest::Error) -> Self {
        if error.is_timeout() {
            McpError::Timeout
        } else if let Some(status) = error.status() {
            if status.as_u16() == 401 || status.as_u16() == 403 {
                McpError::AuthRequired
            } else {
                McpError::Transport(format!("HTTP {}: {}", status.as_u16(), error))
            }
        } else {
            McpError::Transport(error.to_string())
        }
    }
}
