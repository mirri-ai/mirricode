//! HTTP/WebSocket client implementation of [`MirriHarness`].
//!
//! [`HttpHarness`] talks to the Rust `mirri-server` (or any server that
//! speaks the same wire protocol — the TypeScript server included) over:
//!
//! - the REST routes under `/api/v1/` for every request/response operation,
//!   decoding the `{code, msg, data, request_id}` envelope (`code: 0` means
//!   success; non-zero codes follow JSON-RPC 2.0 conventions);
//! - the multiplexed WebSocket endpoint at `/api/v1/ws` for event
//!   subscription: each [`MirriHarness::subscribe`] call opens one
//!   connection, completes the `server_hello`/`client_hello` handshake,
//!   subscribes to the requested session, and pumps `session_event` frames
//!   into a `tokio::sync::broadcast` channel until the receiver is dropped.

use std::time::Duration;

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use mirri_core::SessionEvent;
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
use uuid::Uuid;

use crate::types::{
    HealthStatus, SendMessageResult, ServerMeta, SessionDetails, SessionSummary, ToolSummary,
};
use crate::{HarnessError, MirriHarness};

/// How long the HTTP client waits for one request/response round trip. The
/// engine loop of `message.send` runs synchronously inside the request, so
/// the timeout must be generous enough for scripted turns.
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the WebSocket client waits for the handshake, the `subscribed`
/// acknowledgment, and each subsequent frame.
const WS_FRAME_TIMEOUT: Duration = Duration::from_secs(10);

/// Capacity of the broadcast channel bridging WebSocket frames to
/// [`MirriHarness::subscribe`] receivers. Like the server's own bus, a slow
/// subscriber observes `RecvError::Lagged` instead of blocking the pump.
const EVENT_BUFFER_CAPACITY: usize = 64;

/// JSON-RPC application error code the server uses for "not found".
const RPC_CODE_NOT_FOUND: i32 = -32001;

/// WebSocket frame types of the multiplexed protocol (`mirri-server` ws.rs).
const FRAME_SERVER_HELLO: &str = "server_hello";
const FRAME_CLIENT_HELLO: &str = "client_hello";
const FRAME_SUBSCRIBED: &str = "subscribed";
const FRAME_SESSION_EVENT: &str = "session_event";
const FRAME_ERROR: &str = "error";

/// A [`MirriHarness`] backed by the server's HTTP REST API and multiplexed
/// WebSocket endpoint.
#[derive(Debug, Clone)]
pub struct HttpHarness {
    client: reqwest::Client,
    base_url: String,
    /// Bearer token sent on every HTTP request and the WebSocket upgrade;
    /// `None` runs against an open (unauthenticated) server.
    auth_token: Option<String>,
}

impl HttpHarness {
    /// Create a harness for an open-mode server (no authentication).
    pub fn new(base_url: String) -> Self {
        Self::with_client(base_url, None)
    }

    /// Create a harness that presents `Authorization: Bearer <token>` on
    /// every request (including the WebSocket upgrade).
    pub fn with_auth(base_url: String, token: String) -> Self {
        Self::with_client(base_url, Some(token))
    }

    /// Common constructor: build the time-bounded client, attaching the
    /// bearer token when provided.
    fn with_client(base_url: String, auth_token: Option<String>) -> Self {
        let mut builder = reqwest::Client::builder().timeout(HTTP_TIMEOUT);
        if let Some(token) = &auth_token {
            let mut headers = reqwest::header::HeaderMap::new();
            headers.insert(
                reqwest::header::AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {token}"))
                    .expect("bearer token is a valid header value"),
            );
            builder = builder.default_headers(headers);
        }
        let client = builder.build().expect("build http client");
        Self {
            client,
            base_url,
            auth_token,
        }
    }

    /// The base URL with a trailing slash removed, for URL building.
    fn base(&self) -> &str {
        self.base_url.trim_end_matches('/')
    }

    /// Build an `/api/v1/...` URL; path segments are percent-encoded, so
    /// session ids, request ids, and tool names with special characters stay
    /// valid URL path segments.
    fn api_url(&self, path_segments: &[&str]) -> Result<String, HarnessError> {
        let mut url = reqwest::Url::parse(self.base())
            .map_err(|e| HarnessError::Http(format!("invalid base url: {e}")))?;
        url.path_segments_mut()
            .map_err(|_| HarnessError::Http("base url cannot carry /api/v1 paths".to_string()))?
            .extend(["api", "v1"])
            .extend(path_segments);
        Ok(url.to_string())
    }

    /// The multiplexed WebSocket endpoint URL (`ws://`/`wss://` variant of
    /// the base URL).
    fn ws_url(&self) -> Result<String, HarnessError> {
        let mut url = reqwest::Url::parse(self.base())
            .map_err(|e| HarnessError::Http(format!("invalid base url: {e}")))?;
        let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
        url.set_scheme(scheme)
            .map_err(|_| HarnessError::Http("base url cannot carry a ws scheme".to_string()))?;
        url.set_path("/api/v1/ws");
        url.set_query(None);
        Ok(url.to_string())
    }

    /// Send a request, decode the `{code, msg, data, request_id}` envelope,
    /// and return the `data` payload of a successful call.
    ///
    /// The server always answers with HTTP 200 and signals the business
    /// outcome through `envelope.code`, so the code is authoritative; a
    /// non-2xx status (e.g. a proxy) without a success envelope is reported
    /// as a transport error.
    async fn send(&self, request: reqwest::RequestBuilder) -> Result<Value, HarnessError> {
        let response = request
            .send()
            .await
            .map_err(|e| HarnessError::Http(e.to_string()))?;
        let status = response.status();
        let envelope: Value = response
            .json()
            .await
            .map_err(|e| HarnessError::Http(format!("invalid response envelope: {e}")))?;

        let code = envelope.get("code").and_then(Value::as_i64).unwrap_or(-1) as i32;
        let msg = envelope
            .get("msg")
            .and_then(Value::as_str)
            .unwrap_or("unknown error")
            .to_string();

        if code == 0 {
            if !status.is_success() {
                return Err(HarnessError::Http(format!(
                    "HTTP {status} without a success envelope: {msg}"
                )));
            }
            return Ok(envelope.get("data").cloned().unwrap_or(Value::Null));
        }

        if code == RPC_CODE_NOT_FOUND {
            return Err(HarnessError::NotFound(msg));
        }
        Err(HarnessError::Api { code, msg })
    }

    /// `GET` helper returning the envelope `data` payload.
    async fn get(&self, path_segments: &[&str]) -> Result<Value, HarnessError> {
        let url = self.api_url(path_segments)?;
        self.send(self.client.get(&url)).await
    }

    /// `POST` helper returning the envelope `data` payload.
    async fn post(&self, path_segments: &[&str], body: Value) -> Result<Value, HarnessError> {
        let url = self.api_url(path_segments)?;
        self.send(self.client.post(&url).json(&body)).await
    }

    /// `DELETE` helper returning the envelope `data` payload.
    async fn delete(&self, path_segments: &[&str]) -> Result<Value, HarnessError> {
        let url = self.api_url(path_segments)?;
        self.send(self.client.delete(&url)).await
    }
}

#[async_trait]
impl MirriHarness for HttpHarness {
    async fn create_session(&self, title: &str) -> Result<String, HarnessError> {
        let data = self.post(&["sessions"], json!({ "title": title })).await?;
        let summary: SessionSummary = serde_json::from_value(data)
            .map_err(|e| HarnessError::Http(format!("invalid session payload: {e}")))?;
        Ok(summary.id)
    }

    async fn list_sessions(&self) -> Result<Vec<SessionSummary>, HarnessError> {
        let data = self.get(&["sessions"]).await?;
        // The REST list serves the TS page envelope `{items, has_more}`
        // (the v2 wire shape `session.list` landed); the harness trait
        // keeps the flat summary vector — the page is unwrapped here.
        #[derive(serde::Deserialize)]
        struct SessionPage {
            items: Vec<SessionSummary>,
        }
        let page: SessionPage = serde_json::from_value(data)
            .map_err(|e| HarnessError::Http(format!("invalid session list payload: {e}")))?;
        Ok(page.items)
    }

    async fn get_session(&self, session_id: &str) -> Result<SessionDetails, HarnessError> {
        let data = self.get(&["sessions", session_id]).await?;
        serde_json::from_value(data)
            .map_err(|e| HarnessError::Http(format!("invalid session details payload: {e}")))
    }

    async fn close_session(&self, session_id: &str) -> Result<(), HarnessError> {
        self.delete(&["sessions", session_id]).await?;
        Ok(())
    }

    async fn send_message(
        &self,
        session_id: &str,
        text: &str,
    ) -> Result<SendMessageResult, HarnessError> {
        let data = self
            .post(
                &["sessions", session_id, "messages"],
                json!({ "text": text }),
            )
            .await?;
        let turn = data.get("turn").cloned().ok_or_else(|| {
            HarnessError::Http("message.send response missing 'turn'".to_string())
        })?;
        serde_json::from_value(turn)
            .map_err(|e| HarnessError::Http(format!("invalid turn payload: {e}")))
    }

    async fn list_tools(&self, session_id: &str) -> Result<Vec<ToolSummary>, HarnessError> {
        let data = self.get(&["sessions", session_id, "tools"]).await?;
        serde_json::from_value(data)
            .map_err(|e| HarnessError::Http(format!("invalid tool list payload: {e}")))
    }

    async fn subscribe(
        &self,
        session_id: &str,
    ) -> Result<broadcast::Receiver<SessionEvent>, HarnessError> {
        let url = self.ws_url()?;
        let mut request = url
            .into_client_request()
            .map_err(|e| HarnessError::Http(format!("invalid ws request: {e}")))?;
        if let Some(token) = &self.auth_token {
            request.headers_mut().insert(
                HeaderName::from_static("authorization"),
                HeaderValue::from_str(&format!("Bearer {token}"))
                    .expect("bearer token is a valid header value"),
            );
        }

        let (mut socket, _response) =
            tokio::time::timeout(WS_FRAME_TIMEOUT, connect_async(request))
                .await
                .map_err(|_elapsed| {
                    HarnessError::Http("timed out connecting to the ws endpoint".into())
                })?
                .map_err(|e| HarnessError::Http(format!("ws connect failed: {e}")))?;

        // Handshake: the server greets first, then we identify ourselves.
        let hello = next_frame(&mut socket).await?;
        if hello.get("type").and_then(Value::as_str) != Some(FRAME_SERVER_HELLO) {
            return Err(HarnessError::Http(
                "expected server_hello as the first ws frame".to_string(),
            ));
        }
        send_frame(
            &mut socket,
            json!({ "type": FRAME_CLIENT_HELLO, "clientId": Uuid::new_v4().to_string() }),
        )
        .await?;
        send_frame(
            &mut socket,
            json!({ "type": "subscribe", "sessionId": session_id }),
        )
        .await?;

        // Wait for the `subscribed` acknowledgment; events racing ahead of it
        // are forwarded into the channel too, so none are lost.
        let (tx, rx) = broadcast::channel(EVENT_BUFFER_CAPACITY);
        loop {
            let frame = next_frame(&mut socket).await?;
            match frame.get("type").and_then(Value::as_str) {
                Some(FRAME_SUBSCRIBED) => break,
                Some(FRAME_SESSION_EVENT) => {
                    forward_event(&tx, session_id, &frame);
                }
                Some(FRAME_ERROR) => {
                    let msg = frame
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("websocket error")
                        .to_string();
                    return if msg.contains("not found") {
                        Err(HarnessError::NotFound(msg))
                    } else {
                        Err(HarnessError::Http(format!("ws error frame: {msg}")))
                    };
                }
                // `pong` and unknown frames are part of the protocol; keep
                // waiting for the acknowledgment.
                _ => {}
            }
        }

        // From here on a background pump forwards `session_event` frames
        // until the connection closes or every receiver is dropped.
        let pump_session_id = session_id.to_string();
        tokio::spawn(ws_pump(socket, pump_session_id, tx));
        Ok(rx)
    }

    async fn resolve_approval(
        &self,
        session_id: &str,
        request_id: &str,
        approved: bool,
    ) -> Result<(), HarnessError> {
        self.post(
            &["sessions", session_id, "approvals", request_id],
            json!({ "approved": approved }),
        )
        .await?;
        Ok(())
    }

    async fn answer_question(
        &self,
        session_id: &str,
        request_id: &str,
        answer: &str,
    ) -> Result<(), HarnessError> {
        self.post(
            &["sessions", session_id, "questions", request_id],
            json!({ "answer": answer }),
        )
        .await?;
        Ok(())
    }

    async fn meta(&self) -> Result<ServerMeta, HarnessError> {
        let data = self.get(&["meta"]).await?;
        serde_json::from_value(data)
            .map_err(|e| HarnessError::Http(format!("invalid meta payload: {e}")))
    }

    async fn health(&self) -> Result<HealthStatus, HarnessError> {
        let data = self.get(&["healthz"]).await?;
        serde_json::from_value(data)
            .map_err(|e| HarnessError::Http(format!("invalid health payload: {e}")))
    }
}

/// The WebSocket stream type `connect_async` produces for plain `ws://`
/// (and `wss://` with TLS) URLs.
type WsStream = WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Read the next JSON text frame, bounded by [`WS_FRAME_TIMEOUT`].
async fn next_frame(socket: &mut WsStream) -> Result<Value, HarnessError> {
    let message = tokio::time::timeout(WS_FRAME_TIMEOUT, socket.next())
        .await
        .map_err(|_elapsed| HarnessError::Http("timed out waiting for a ws frame".to_string()))?
        .ok_or_else(|| HarnessError::Http("ws stream ended".to_string()))?
        .map_err(|e| HarnessError::Http(format!("ws receive error: {e}")))?;
    if message.is_close() {
        return Err(HarnessError::Http(
            "ws connection closed by the server".to_string(),
        ));
    }
    let text = message
        .into_text()
        .map_err(|_| HarnessError::Http("ws frame was not text".to_string()))?;
    serde_json::from_str(&text).map_err(|e| HarnessError::Http(format!("invalid ws frame: {e}")))
}

/// Send one JSON text frame.
async fn send_frame(socket: &mut WsStream, frame: Value) -> Result<(), HarnessError> {
    socket
        .send(Message::Text(frame.to_string().into()))
        .await
        .map_err(|e| HarnessError::Http(format!("ws send error: {e}")))
}

/// Forward one `session_event` frame for the subscribed session into the
/// broadcast channel. Returns `false` when every receiver is gone, so the
/// caller can stop pumping.
fn forward_event(tx: &broadcast::Sender<SessionEvent>, session_id: &str, frame: &Value) -> bool {
    if frame.get("type").and_then(Value::as_str) != Some(FRAME_SESSION_EVENT) {
        return true;
    }
    if frame.get("sessionId").and_then(Value::as_str) != Some(session_id) {
        tracing::warn!(session_id, "ignoring event frame for another session");
        return true;
    }
    let event = match frame.get("event").cloned() {
        Some(event) => event,
        None => return true,
    };
    match serde_json::from_value::<SessionEvent>(event) {
        Ok(event) => tx.send(event).is_ok(),
        Err(e) => {
            tracing::warn!(session_id, error = %e, "dropped unparsable session event");
            true
        }
    }
}

/// Background pump: drain the WebSocket connection and forward the
/// subscribed session's events until the connection closes, the stream
/// errors, or every receiver is dropped.
async fn ws_pump(mut socket: WsStream, session_id: String, tx: broadcast::Sender<SessionEvent>) {
    while let Some(message) = socket.next().await {
        let Ok(message) = message else {
            break;
        };
        if message.is_close() {
            break;
        }
        if !message.is_text() {
            continue;
        }
        let Ok(text) = message.into_text() else {
            continue;
        };
        let Ok(frame) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if !forward_event(&tx, &session_id, &frame) {
            break;
        }
    }
    tracing::debug!(session_id, "ws event stream closed");
}
