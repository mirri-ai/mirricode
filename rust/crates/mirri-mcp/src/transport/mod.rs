//! Transport configuration and the async trait that every transport
//! implementation must satisfy.
//!
//! The transport layer is the boundary between the JSON-RPC protocol logic
//! (in [`crate::client`]) and the concrete I/O mechanism (stdio subprocess,
//! HTTP, SSE). Each transport knows how to:
//!
//! 1. Establish a connection (spawn process, open HTTP connection, etc.)
//! 2. Send a JSON-RPC request and read back the matching response
//! 3. Capture diagnostic output (stderr) when available
//! 4. Close cleanly

pub mod http;
pub mod sse;
pub mod stdio;

use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use crate::error::McpResult;
use crate::types::JsonRpcResponse;

/// A boxed future returned by transport methods.
///
/// We use `Pin<Box<dyn Future>>` instead of `impl Future` so the trait
/// remains object-safe (dyn-compatible) and can be used as `Box<dyn
/// McpTransport>`.
pub type TransportFuture<'a, T> =
    Pin<Box<dyn std::future::Future<Output = McpResult<T>> + Send + 'a>>;

/// Configuration for connecting to an MCP server.
///
/// The `type` tag selects the transport; the remaining fields are
/// transport-specific. This enum is serde-tagged so it can be loaded directly
/// from a config file (e.g. `mcp.json`).
///
/// `PartialEq` is derived so the config hot-reload watcher can detect when
/// a server's transport config changed on disk.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type")]
pub enum McpTransportConfig {
    /// Connect by spawning a child process and communicating over its
    /// stdin/stdout.
    #[serde(rename = "stdio")]
    Stdio {
        /// Executable to run.
        command: String,
        /// Command-line arguments. Defaults to an empty vector.
        #[serde(default)]
        args: Vec<String>,
        /// Environment variable overrides (merged on top of the parent env).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        env: Option<HashMap<String, String>>,
        /// Working directory for the child process.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<PathBuf>,
    },

    /// Connect via the Streamable HTTP transport (POST requests, optional
    /// SSE streaming for server-to-client messages).
    #[serde(rename = "http")]
    Http {
        /// Server endpoint URL.
        url: String,
        /// Additional HTTP headers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        headers: Option<HashMap<String, String>>,
        /// Bearer token for `Authorization` header.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
    },

    /// Connect via the deprecated SSE transport (GET for event stream,
    /// POST for requests). Kept for backwards compatibility with older
    /// MCP servers.
    #[serde(rename = "sse")]
    Sse {
        /// SSE endpoint URL.
        url: String,
        /// Additional HTTP headers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        headers: Option<HashMap<String, String>>,
        /// Bearer token for `Authorization` header.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<String>,
    },
}

/// A trait for dynamically providing bearer tokens (e.g. from OAuth).
///
/// Implemented by [`crate::oauth::OAuthClient`] and
/// [`crate::oauth::ServerTokenProvider`], and by any caller that wants
/// to supply tokens from an external source. The transport awaits
/// `get_token()` before each request to set the `Authorization` header —
/// async so a token refresh can run on the ambient runtime instead of a
/// nested blocking one.
pub trait TokenProvider: Send + Sync {
    /// Return a valid bearer token, refreshing if necessary.
    fn get_token(&self) -> TransportFuture<'_, String>;
}

/// The async transport contract.
///
/// A transport is responsible for sending a serialized JSON-RPC request and
/// returning the deserialized response. Implementations may buffer multiple
/// in-flight requests (the HTTP transport does this naturally) or serialize
/// them one at a time (the stdio transport uses a request queue + reader
/// task).
///
/// The transport also owns any diagnostic state (e.g. stderr tail buffer)
/// that callers can inspect after a failure.
pub trait McpTransport: Send + Sync {
    /// Send a JSON-RPC request and wait for the response.
    ///
    /// The `timeout` bounds the entire round-trip. If it elapses the
    /// implementation should return [`crate::error::McpError::Timeout`].
    fn send_request(
        &mut self,
        request_json: &str,
        timeout: Duration,
    ) -> TransportFuture<'_, JsonRpcResponse>;

    /// Send a JSON-RPC notification (no `id`, no response expected).
    ///
    /// Writes the notification to the transport without registering a
    /// pending response channel. The default implementation falls back to
    /// `send_request` with a short timeout, ignoring errors — transports
    /// that can write without expecting a response should override this.
    fn send_notification(&mut self, notification_json: &str) -> TransportFuture<'_, ()> {
        let _ = notification_json;
        Box::pin(async move { Ok(()) })
    }

    /// Return a snapshot of the child process's stderr, if applicable.
    ///
    /// For transports without stderr (HTTP, SSE) this returns an empty
    /// string.
    fn stderr_snapshot(&self) -> String {
        String::new()
    }

    /// Close the transport. After this call, `send_request` must not be
    /// called.
    fn close(&mut self) -> TransportFuture<'_, ()>;

    /// Whether the transport is currently connected and usable.
    fn is_connected(&self) -> bool;
}

/// Factory: create a transport from a config.
///
/// This is the single entry point used by [`crate::client::McpClient::connect`].
pub async fn create_transport(config: &McpTransportConfig) -> McpResult<Box<dyn McpTransport>> {
    create_transport_with_token_provider(config, None).await
}

/// Factory variant carrying a dynamic token provider (OAuth wiring): the
/// HTTP and SSE transports fetch a fresh `Authorization: Bearer` token from
/// the provider before each request so refreshes happen transparently. A
/// stdio config ignores the provider (stdio never produces a 401 —
/// adjudication A4: OAuth is remote-transport only).
pub async fn create_transport_with_token_provider(
    config: &McpTransportConfig,
    token_provider: Option<Arc<dyn TokenProvider>>,
) -> McpResult<Box<dyn McpTransport>> {
    let transport: Box<dyn McpTransport> = match config {
        McpTransportConfig::Stdio {
            command,
            args,
            env,
            cwd,
        } => Box::new(crate::transport::stdio::StdioTransport::new(
            command.clone(),
            args.clone(),
            env.clone(),
            cwd.clone(),
        )?),
        McpTransportConfig::Http {
            url,
            headers,
            token,
        } => Box::new(match token_provider {
            Some(provider) => crate::transport::http::HttpTransport::with_token_provider(
                url.clone(),
                headers.clone(),
                provider,
            ),
            None => crate::transport::http::HttpTransport::new(
                url.clone(),
                headers.clone(),
                token.clone(),
            ),
        }),
        McpTransportConfig::Sse {
            url,
            headers,
            token,
        } => Box::new(match token_provider {
            Some(provider) => crate::transport::sse::SseTransport::with_token_provider(
                url.clone(),
                headers.clone(),
                provider,
            ),
            None => crate::transport::sse::SseTransport::new(
                url.clone(),
                headers.clone(),
                token.clone(),
            ),
        }),
    };
    Ok(transport)
}
