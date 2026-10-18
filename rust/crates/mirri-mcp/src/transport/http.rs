//! Streamable HTTP transport — JSON-RPC over HTTP POST.
//!
//! The Streamable HTTP transport (MCP spec 2025-03-26 and later) sends each
//! JSON-RPC request as an HTTP POST to the server endpoint. The response body
//! contains the JSON-RPC response. Server-to-client notifications (if any)
//! are delivered via SSE streams in the response body, but for the basic
//! request/response cycle we simply parse the JSON response.
//!
//! This implementation supports:
//! - Bearer token authentication (`Authorization: Bearer <token>`)
//! - Custom headers
//! - Configurable timeout per request
//! - Connection pooling via reqwest's built-in `Client`

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tracing::debug;

use crate::error::{McpError, McpResult};
use crate::transport::{McpTransport, TokenProvider, TransportFuture};
use crate::types::JsonRpcResponse;

/// HTTP transport implementation.
pub struct HttpTransport {
    url: String,
    client: reqwest::Client,
    headers: Vec<(String, String)>,
    /// Dynamic token provider — when present, the `Authorization` header is
    /// fetched fresh before each request so token refresh happens transparently.
    token_provider: Option<Arc<dyn TokenProvider>>,
    connected: Arc<AtomicBool>,
}

impl HttpTransport {
    /// Create a new HTTP transport with a static token.
    ///
    /// The `reqwest::Client` is constructed here so connection pooling is
    /// reused across requests.
    pub fn new(
        url: String,
        custom_headers: Option<HashMap<String, String>>,
        token: Option<String>,
    ) -> Self {
        let mut builder = reqwest::Client::builder().timeout(Duration::from_secs(300));

        // Build the header list once — applied to every request.
        let mut headers: Vec<(String, String)> = Vec::new();

        headers.push(("Content-Type".to_string(), "application/json".to_string()));
        headers.push((
            "Accept".to_string(),
            "application/json, text/event-stream".to_string(),
        ));

        if let Some(token) = token {
            headers.push(("Authorization".to_string(), format!("Bearer {token}")));
        }

        if let Some(custom) = custom_headers {
            for (key, value) in custom {
                headers.push((key, value));
            }
        }

        // We'll handle timeout per-request via the reqwest client's
        // `RequestBuilder::timeout`. Set a generous default on the client.
        builder = builder.timeout(Duration::from_secs(300));

        let client = builder.build().unwrap_or_else(|_| reqwest::Client::new());

        debug!(url = %url, "HTTP MCP transport created");

        HttpTransport {
            url,
            client,
            headers,
            token_provider: None,
            connected: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Create an HTTP transport with a dynamic token provider.
    ///
    /// When set, the `Authorization: Bearer <token>` header is fetched from
    /// the provider before each request, enabling transparent token refresh.
    pub fn with_token_provider(
        url: String,
        custom_headers: Option<HashMap<String, String>>,
        token_provider: Arc<dyn TokenProvider>,
    ) -> Self {
        let mut transport = Self::new(url, custom_headers, None);
        transport.token_provider = Some(token_provider);
        transport
    }
}

impl McpTransport for HttpTransport {
    fn send_request(
        &mut self,
        request_json: &str,
        timeout: Duration,
    ) -> TransportFuture<'_, JsonRpcResponse> {
        let request_json = request_json.to_string();
        Box::pin(async move {
            if !self.connected.load(Ordering::Relaxed) {
                return Err(McpError::Disconnected);
            }

            let mut req = self.client.post(&self.url).body(request_json.clone());

            for (key, value) in &self.headers {
                req = req.header(key, value);
            }

            // When a token provider is attached, fetch a fresh token for each
            // request so transparent refresh can happen.
            if let Some(provider) = &self.token_provider {
                let token = provider.get_token().await?;
                req = req.header("Authorization", format!("Bearer {token}"));
            }

            // Per-request timeout.
            req = req.timeout(timeout);

            let response = req.send().await.map_err(McpError::from_reqwest)?;

            let status = response.status();
            if status.as_u16() == 401 || status.as_u16() == 403 {
                self.connected.store(false, Ordering::Relaxed);
                return Err(McpError::AuthRequired);
            }

            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                return Err(McpError::Transport(format!(
                    "HTTP {}: {}",
                    status.as_u16(),
                    body
                )));
            }

            // Check content type — if it's SSE, we need to parse the event
            // stream for the data lines. For now, handle both JSON and SSE.
            let content_type = response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();

            let body = response
                .text()
                .await
                .map_err(|e| McpError::Transport(format!("failed to read response body: {e}")))?;

            let json_response = if content_type.contains("text/event-stream") {
                // Parse SSE: find the first `data:` line that contains valid JSON.
                parse_sse_response(&body)?
            } else {
                // Direct JSON response.
                body
            };

            let rpc_response: JsonRpcResponse =
                serde_json::from_str(&json_response).map_err(|e| {
                    McpError::Protocol(format!("failed to parse JSON-RPC response: {e}"))
                })?;

            Ok(rpc_response)
        })
    }

    fn close(&mut self) -> TransportFuture<'_, ()> {
        Box::pin(async move {
            self.connected.store(false, Ordering::Relaxed);
            debug!("HTTP MCP transport closed");
            Ok(())
        })
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }
}

/// Parse an SSE event stream body and extract the first JSON-RPC response.
///
/// SSE format: lines starting with `data:` contain the payload. Multiple
/// `data:` lines concatenate into a single event; a blank line delimits
/// events.
fn parse_sse_response(body: &str) -> McpResult<String> {
    let mut data_lines: Vec<&str> = Vec::new();

    for line in body.lines() {
        if let Some(data) = line.strip_prefix("data:") {
            let data = data.trim_start();
            data_lines.push(data);
        } else if line.is_empty() && !data_lines.is_empty() {
            // End of an event — try to parse what we have.
            break;
        }
    }

    if data_lines.is_empty() {
        return Err(McpError::Protocol(
            "SSE response contained no data lines".to_string(),
        ));
    }

    let combined = data_lines.join("\n");
    // Verify it's valid JSON.
    serde_json::from_str::<serde_json::Value>(&combined)
        .map_err(|e| McpError::Protocol(format!("SSE data is not valid JSON: {e}")))?;

    Ok(combined)
}
