//! SSE transport — deprecated Server-Sent Events transport for older MCP
//! servers.
//!
//! The SSE transport (pre-Streamable HTTP) works as follows:
//! 1. The client opens a long-lived GET connection to the SSE endpoint
//! 2. The server sends an `endpoint` event with a URL for POSTing requests
//! 3. The client POSTs JSON-RPC requests to that endpoint
//! 4. Responses arrive as SSE `message` events on the GET stream
//!
//! This implementation supports:
//! - Bearer token authentication
//! - Custom headers
//! - Configurable timeout per request
//!
//! Note: This transport is deprecated in favor of Streamable HTTP. It is kept
//! for compatibility with older MCP servers.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::{Mutex, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::error::{McpError, McpResult};
use crate::transport::{McpTransport, TransportFuture};
use crate::types::{JsonRpcId, JsonRpcResponse};

/// SSE transport implementation.
pub struct SseTransport {
    url: String,
    client: reqwest::Client,
    headers: Vec<(String, String)>,
    /// Dynamic token provider — when present, the `Authorization` header is
    /// fetched fresh for each request so token refresh happens transparently
    /// (the OAuth wiring; same contract as the HTTP transport).
    token_provider: Option<Arc<dyn crate::transport::TokenProvider>>,
    /// The POST endpoint received from the server's `endpoint` event.
    /// `None` until the initial SSE connection delivers it.
    post_endpoint: Mutex<Option<String>>,
    /// Pending request id → response channel.
    pending: Mutex<HashMap<String, oneshot::Sender<JsonRpcResponse>>>,
    connected: Arc<AtomicBool>,
    /// Cancellation token for the SSE reader task.
    cancel: CancellationToken,
}

impl SseTransport {
    /// Create a new SSE transport.
    ///
    /// The actual SSE connection is established lazily on the first
    /// `send_request` call, because the POST endpoint must be discovered
    /// from the initial SSE stream first.
    pub fn new(
        url: String,
        custom_headers: Option<HashMap<String, String>>,
        token: Option<String>,
    ) -> Self {
        let mut headers: Vec<(String, String)> = Vec::new();
        headers.push(("Accept".to_string(), "text/event-stream".to_string()));

        if let Some(token) = token {
            headers.push(("Authorization".to_string(), format!("Bearer {token}")));
        }

        if let Some(custom) = custom_headers {
            for (key, value) in custom {
                headers.push((key, value));
            }
        }

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(600)) // long-lived SSE stream
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        debug!(url = %url, "SSE MCP transport created");

        SseTransport {
            url,
            client,
            headers,
            token_provider: None,
            post_endpoint: Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
            connected: Arc::new(AtomicBool::new(false)),
            cancel: CancellationToken::new(),
        }
    }

    /// Create an SSE transport with a dynamic token provider.
    ///
    /// When set, the `Authorization: Bearer <token>` header is fetched from
    /// the provider before each request, enabling transparent token refresh
    /// (the OAuth wiring).
    pub fn with_token_provider(
        url: String,
        custom_headers: Option<HashMap<String, String>>,
        token_provider: Arc<dyn crate::transport::TokenProvider>,
    ) -> Self {
        let mut transport = Self::new(url, custom_headers, None);
        transport.token_provider = Some(token_provider);
        transport
    }

    fn dynamic_token(&self) -> TransportFuture<'_, Option<String>> {
        Box::pin(async move {
            match &self.token_provider {
                Some(provider) => Ok(Some(provider.get_token().await?)),
                None => Ok(None),
            }
        })
    }

    /// Establish the SSE connection and wait for the `endpoint` event.
    async fn ensure_connected(&self) -> McpResult<()> {
        if self.connected.load(Ordering::Relaxed) {
            return Ok(());
        }

        // Build the GET request.
        let mut req = self.client.get(&self.url);
        for (key, value) in &self.headers {
            req = req.header(key, value);
        }
        if let Some(token) = self.dynamic_token().await? {
            req = req.header("Authorization", format!("Bearer {token}"));
        }

        // Send the request and get a streaming response.
        let response = req.send().await.map_err(McpError::from_reqwest)?;

        let status = response.status();
        if status.as_u16() == 401 || status.as_u16() == 403 {
            return Err(McpError::AuthRequired);
        }

        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(McpError::ConnectionFailed(format!(
                "SSE connection failed: HTTP {}: {}",
                status.as_u16(),
                body
            )));
        }

        // We need to read the stream to get the `endpoint` event and
        // subsequent `message` events. However, reqwest's streaming API
        // requires us to hold the response body open. We'll spawn a task
        // that reads the stream.
        //
        // For the initial connection, we need to read until we get the
        // endpoint event. We'll use a channel to receive it.
        let (endpoint_tx, endpoint_rx) = oneshot::channel::<String>();

        let cancel = self.cancel.clone();
        let connected = Arc::clone(&self.connected);
        let pending = Arc::new(Mutex::new(HashMap::<
            String,
            oneshot::Sender<JsonRpcResponse>,
        >::new()));

        // We need to share the pending map between the transport and the
        // reader task. But we can't move `self.pending` into a task because
        // it's a field. Instead, we'll use a trick: create a shared map
        // and swap it in.
        //
        // Actually, let's restructure: the reader task owns the response
        // stream and routes messages. The transport holds a shared Arc to
        // the pending map.

        tokio::spawn(async move {
            sse_reader_task(response, endpoint_tx, pending, connected, cancel).await;
        });

        // Wait for the endpoint event.
        let post_url = tokio::time::timeout(Duration::from_secs(30), endpoint_rx)
            .await
            .map_err(|_| McpError::Timeout)?
            .map_err(|_| {
                McpError::ConnectionFailed("SSE stream closed before endpoint event".into())
            })?;

        *self.post_endpoint.lock().await = Some(post_url);
        self.connected.store(true, Ordering::Relaxed);

        Ok(())
    }

    /// Make the key for a request id.
    fn make_key(id: &JsonRpcId) -> String {
        match id {
            JsonRpcId::Num(n) => n.to_string(),
            JsonRpcId::Str(s) => s.clone(),
            JsonRpcId::Null => "null".to_string(),
        }
    }
}

impl McpTransport for SseTransport {
    fn send_request(
        &mut self,
        request_json: &str,
        timeout: Duration,
    ) -> TransportFuture<'_, JsonRpcResponse> {
        let request_json = request_json.to_string();
        Box::pin(async move {
            // Ensure SSE connection is established.
            self.ensure_connected().await?;

            if !self.connected.load(Ordering::Relaxed) {
                return Err(McpError::Disconnected);
            }

            // Parse the request to get its id.
            let request: crate::types::JsonRpcRequest = serde_json::from_str(&request_json)
                .map_err(|e| McpError::Protocol(format!("invalid request JSON: {e}")))?;

            let key = Self::make_key(&request.id);

            // Register pending response.
            let (tx, rx) = oneshot::channel();
            {
                let mut pending = self.pending.lock().await;
                pending.insert(key.clone(), tx);
            }

            // POST the request to the endpoint.
            let post_url = {
                let endpoint = self.post_endpoint.lock().await;
                endpoint
                    .clone()
                    .ok_or_else(|| McpError::Transport("SSE POST endpoint not available".into()))?
            };

            let mut post_req = self.client.post(&post_url).body(request_json.clone());
            post_req = post_req.header("Content-Type", "application/json");
            for (key, value) in &self.headers {
                post_req = post_req.header(key, value);
            }
            if let Some(token) = self.dynamic_token().await? {
                post_req = post_req.header("Authorization", format!("Bearer {token}"));
            }
            post_req = post_req.timeout(timeout);

            let post_response = post_req.send().await.map_err(McpError::from_reqwest)?;

            if !post_response.status().is_success() && post_response.status().as_u16() != 202 {
                let status = post_response.status();
                let body = post_response.text().await.unwrap_or_default();
                // Clean up pending entry.
                let mut pending = self.pending.lock().await;
                pending.remove(&key);
                return Err(McpError::Transport(format!(
                    "SSE POST failed: HTTP {}: {}",
                    status.as_u16(),
                    body
                )));
            }

            // Wait for the response on the SSE stream.
            match tokio::time::timeout(timeout, rx).await {
                Ok(Ok(response)) => Ok(response),
                Ok(Err(_)) => {
                    self.connected.store(false, Ordering::Relaxed);
                    Err(McpError::Disconnected)
                }
                Err(_) => {
                    let mut pending = self.pending.lock().await;
                    pending.remove(&key);
                    Err(McpError::Timeout)
                }
            }
        })
    }

    fn close(&mut self) -> TransportFuture<'_, ()> {
        Box::pin(async move {
            self.connected.store(false, Ordering::Relaxed);
            self.cancel.cancel();

            // Clear pending requests.
            let mut pending = self.pending.lock().await;
            pending.clear();

            debug!("SSE MCP transport closed");
            Ok(())
        })
    }

    fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }
}

/// Background task that reads the SSE event stream and routes messages.
///
/// This task:
/// 1. Parses SSE events (lines starting with `event:` and `data:`)
/// 2. When it sees an `endpoint` event, sends the URL to `endpoint_tx`
/// 3. When it sees a `message` event, parses the JSON-RPC response and
///    routes it to the matching pending request
async fn sse_reader_task(
    response: reqwest::Response,
    endpoint_tx: oneshot::Sender<String>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<JsonRpcResponse>>>>,
    connected: Arc<AtomicBool>,
    cancel: CancellationToken,
) {
    use futures::StreamExt;
    use tokio_util::io::StreamReader;

    // Convert the response body into an async read stream.
    let byte_stream = response.bytes_stream();
    let mut reader =
        StreamReader::new(byte_stream.map(|result| result.map_err(std::io::Error::other)));

    let mut buffer = String::new();
    let mut event_type = String::new();
    let mut data_lines: Vec<String> = Vec::new();
    let mut endpoint_tx_opt = Some(endpoint_tx);

    let mut buf = [0u8; 4096];
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                debug!("SSE reader task cancelled");
                break;
            }
            result = reader.read(&mut buf) => {
                match result {
                    Ok(0) => {
                        debug!("SSE stream ended");
                        break;
                    }
                    Ok(n) => {
                        let chunk = String::from_utf8_lossy(&buf[..n]);
                        buffer.push_str(&chunk);

                        // Process complete lines.
                        while let Some(pos) = buffer.find('\n') {
                            let line = buffer[..pos].trim_end_matches('\r').to_string();
                            buffer = buffer[pos + 1..].to_string();

                            if line.is_empty() {
                                // End of event.
                                if !data_lines.is_empty() {
                                    let data = data_lines.join("\n");
                                    data_lines.clear();

                                    if event_type == "endpoint" {
                                        if let Some(tx) = endpoint_tx_opt.take() {
                                            let _ = tx.send(data.clone());
                                        }
                                    } else if event_type == "message" || event_type.is_empty() {
                                        // Try to parse as JSON-RPC response.
                                        if let Ok(rpc_response) = serde_json::from_str::<JsonRpcResponse>(&data) {
                                            let key = match &rpc_response.id {
                                                JsonRpcId::Num(n) => n.to_string(),
                                                JsonRpcId::Str(s) => s.clone(),
                                                JsonRpcId::Null => "null".to_string(),
                                            };
                                            let mut pending_guard = pending.lock().await;
                                            if let Some(sender) = pending_guard.remove(&key) {
                                                let _ = sender.send(rpc_response);
                                            }
                                        }
                                    }
                                }
                                event_type.clear();
                            } else if let Some(event) = line.strip_prefix("event:") {
                                event_type = event.trim().to_string();
                            } else if let Some(data) = line.strip_prefix("data:") {
                                data_lines.push(data.trim().to_string());
                            }
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "error reading SSE stream");
                        break;
                    }
                }
            }
        }
    }

    connected.store(false, Ordering::Relaxed);
    let mut pending_guard = pending.lock().await;
    pending_guard.clear();
}

use tokio::io::AsyncReadExt;
