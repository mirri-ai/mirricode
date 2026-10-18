//! `mirri-remote` — a WebSocket client implementation of the
//! [`EngineAdapter`] trait against the unified JSON-RPC 2.0 protocol of
//! `mirri-server`'s `/api/v1/rpc` endpoint.
//!
//! One connection carries all three protocol directions, exactly like the
//! browser client (`apps/mirri-web/src/api/daemon/json-rpc-client.ts`):
//!
//! - **forward RPC** — `call_method` → request/response with integer ids;
//! - **event stream** — `event.*` notifications (possibly batched into
//!   array frames — the client flattens them, [`frames::parse_client_frames`],
//!   because the strict server codec rejects arrays by design) fanned out to
//!   every `subscribe_events` subscriber;
//! - **reverse RPC** — server-minted integer-id requests (`approval.ask` /
//!   `question.ask` within the negotiated allowlist), answered through the
//!   [`EngineAdapter::on_reverse_rpc`] handler.
//!
//! The adapter mirrors the **embedded** engine's trait semantics
//! (`mirri-embedded`): `subscribe_events` fans out to all subscribers with
//! no cursor and no replay, a slow subscriber is dropped for that subscriber
//! (never blocking the connection task), and auto-subscription handles the
//! per-session wire protocol (`session.subscribe`) internally. A session
//! created after connect is subscribed to automatically before
//! `session.create` / `session.resume` / `session.fork` returns, so the
//! subscribe-before-create ordering is guaranteed by construction.
//!
//! ## Known limitation
//!
//! Sessions created by **other clients** on the same remote engine after this
//! adapter connected are only discovered lazily through responses to
//! `session.create` / `session.resume` / `session.fork`. The wire has no
//! subscribe-all; adding one is a server-protocol extension, out of scope.

pub mod frames;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest, http::HeaderValue};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream,
    tungstenite::{Error as WsError, http::StatusCode},
};
use uuid::Uuid;

use mirri_transport::adapter::{EngineAdapter, EngineError, ReverseRpcHandler};
use mirri_transport::frame::{
    JsonRpcError, JsonRpcFrame, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, RequestId,
};

use crate::frames::parse_client_frames;

/// How long the client waits for the server's `server_hello` and its
/// `client_hello` response before giving up (server mirror:
/// `HANDSHAKE_TIMEOUT` in `mirri-server`).
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// The heartbeats the server advertises when `server_hello` omits one.
const HEARTBEAT_DEFAULT_MS: u64 = 15_000;

/// Liveness multiplier: a connection silent for `3 × heartbeatMs` is dead
/// (server mirror: `HEARTBEAT_TIMEOUT_MULTIPLIER`).
const HEARTBEAT_TIMEOUT_MULTIPLIER: u64 = 3;

/// Capacity of the per-subscriber event channel handed out by
/// [`EngineAdapter::subscribe_events`] and of the outbound frame queue —
/// mirrors `mirri-embedded`'s `EVENT_CHANNEL_CAPACITY`.
const EVENT_CHANNEL_CAPACITY: usize = 256;

/// Subprotocol prefix carrying the bearer token on the upgrade
/// (server mirror: `BEARER_SUBPROTOCOL_PREFIX`).
const BEARER_SUBPROTOCOL_PREFIX: &str = "mirri-code.bearer.";

/// The client type reported in `client_hello` (the TS browser client reports
/// `"web"`; this is the Rust WS client).
const CLIENT_TYPE: &str = "mirri-remote";

/// Methods whose successful response creates (or reopens) a session: the
/// adapter auto-subscribes to the returned session id before completing the
/// call, preserving the embedded engine's subscribe-before-use ordering.
const SESSION_CREATING_METHODS: [&str; 3] = ["session.create", "session.resume", "session.fork"];

/// A frame queued for the connection task's writer.
enum Outbound {
    /// A JSON-RPC frame to serialize and send.
    Frame(JsonRpcFrame),
    /// A WebSocket Close request (dispose).
    Close,
}

/// One pending request awaiting its response, keyed by the request id.
struct PendingCall {
    /// The method the id was minted for (drives auto-subscription).
    method: String,
    /// Resolves the awaiting caller with the mapped response. For
    /// creating-session methods this sender is transplanted into the
    /// follow-up `session.subscribe` entry, so the caller only completes
    /// once the session is subscribed on the wire.
    response_tx: Option<oneshot::Sender<Result<JsonRpcResponse, EngineError>>>,
    /// For follow-up subscribe entries: the original create response to
    /// deliver once the subscribe ack lands.
    deferred_result: Option<JsonRpcResponse>,
}

/// The `server_hello` facts the adapter exposes through
/// [`RemoteAdapter::caps`] — the server identity and its advertised
/// heartbeat interval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteCaps {
    /// The server instance id reported in `server_hello`.
    pub server_id: String,
    /// The server version string reported in `server_hello`.
    pub version: String,
    /// The heartbeat interval in milliseconds the server advertises.
    pub heartbeat_ms: u64,
    /// The server's durable event ring-buffer capacity, when advertised.
    pub max_event_buffer_size: Option<u64>,
    /// Whether the server negotiated notification batching (`eventBatching`).
    pub event_batching: bool,
}

/// Shared state between the [`RemoteAdapter`] handle and its connection task.
struct RemoteAdapterInner {
    /// Whether the connection is alive and ready for requests.
    connected: AtomicBool,
    /// Whether [`EngineAdapter::dispose`] ran (or is running).
    disposed: AtomicBool,
    /// Pending requests by id. Shared with the connection task, which routes
    /// inbound responses into the awaiting one-shots.
    pending: Mutex<HashMap<i64, PendingCall>>,
    /// Id minting for client-issued requests (increments from 1).
    next_id: AtomicI64,
    /// Outbound frames for the connection task's writer; `None` once the
    /// connection is gone or disposed (dropping the sender closes the
    /// channel, which ends the writer).
    write_tx: Mutex<Option<mpsc::Sender<Outbound>>>,
    /// Live event fan-out. The connection task broadcasts every `event.*`
    /// notification; each [`EngineAdapter::subscribe_events`] subscriber
    /// bridges broadcast → mpsc in its own task. `None` once the connection
    /// is gone or disposed, which closes every subscriber's stream.
    events: Mutex<Option<broadcast::Sender<JsonRpcNotification>>>,
    /// The reverse-RPC handler registered through
    /// [`EngineAdapter::on_reverse_rpc`], if any.
    reverse_rpc: RwLock<Option<ReverseRpcHandler>>,
    /// Handle of the connection task; aborted on dispose.
    connection_task: Mutex<Option<JoinHandle<()>>>,
    /// Sessions the adapter already subscribed to, keyed by session id —
    /// dedupes auto-subscription (the server re-acknowledges idempotently,
    /// but repeated wire round trips are noise).
    subscribed_sessions: Mutex<HashSet<String>>,
    /// The `server_hello` facts, readable through [`RemoteAdapter::caps`].
    caps: Mutex<Option<RemoteCaps>>,
    /// The latest advertised heartbeat interval, in milliseconds.
    heartbeat_ms: AtomicU64,
}

/// WebSocket client adapter for the unified JSON-RPC protocol.
///
/// Construct through [`RemoteAdapter::connect`]; the returned handle
/// implements [`EngineAdapter`].
pub struct RemoteAdapter {
    inner: Arc<RemoteAdapterInner>,
}

impl RemoteAdapter {
    /// Connect to a `mirri-server` `/api/v1/rpc` endpoint and complete the
    /// handshake (server_hello → client_hello → accepted).
    ///
    /// `auth_token` is offered as the `mirri-code.bearer.<token>` WebSocket
    /// subprotocol when given (the protocol's bearer transport; the upgrade
    /// is rejected with 401 otherwise). `reverse_rpc_allowlist` declares the
    /// method-prefix domains (`"approval"`, `"question"`, …) this client is
    /// willing to answer through [`EngineAdapter::on_reverse_rpc`].
    ///
    /// On success the connection task is running and the adapter has
    /// auto-subscribed to every session `session.list` reports, so
    /// [`EngineAdapter::subscribe_events`] receives all-sessions events with
    /// no replay — the embedded engine's semantics.
    pub async fn connect(
        url: &str,
        auth_token: Option<&str>,
        reverse_rpc_allowlist: &[&str],
    ) -> Result<Arc<Self>, EngineError> {
        let (mut sink, mut incoming) = connect_socket(url, auth_token).await?.split();

        // 1. server_hello (the server greets immediately after the upgrade).
        let hello_text = tokio::time::timeout(HANDSHAKE_TIMEOUT, next_inbound_text(&mut incoming))
            .await
            .map_err(|_| {
                EngineError::Transport("handshake timed out waiting for server_hello".to_string())
            })??;
        let hello_frames = parse_client_frames(&hello_text).map_err(|error| {
            EngineError::Transport(format!("invalid server_hello frame: {}", error.message))
        })?;
        let hello = match hello_frames.into_iter().next() {
            Some(JsonRpcFrame::Notification(notification))
                if notification.method == "server_hello" =>
            {
                notification
            }
            _ => {
                return Err(EngineError::Transport(
                    "expected server_hello notification".to_string(),
                ));
            }
        };
        let heartbeat_ms = hello
            .params
            .get("heartbeatMs")
            .and_then(Value::as_u64)
            .filter(|ms| *ms > 0)
            .unwrap_or(HEARTBEAT_DEFAULT_MS);
        let caps = RemoteCaps {
            server_id: hello
                .params
                .get("serverId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            version: hello
                .params
                .get("version")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            heartbeat_ms,
            max_event_buffer_size: hello
                .params
                .get("maxEventBufferSize")
                .and_then(Value::as_u64),
            event_batching: hello
                .params
                .get("capabilities")
                .and_then(|caps| caps.get("eventBatching"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
        };

        // 2. client_hello request; the response `{"accepted": true}` is the
        // handshake-completion signal.
        let client_id = Uuid::new_v4().to_string();
        let hello_id: i64 = 1;
        let client_hello = JsonRpcFrame::Request(JsonRpcRequest::new(
            Some(hello_id),
            "client_hello",
            json!({
                "clientId": client_id,
                "clientType": CLIENT_TYPE,
                "capabilities": {
                    "reverseRpc": reverse_rpc_allowlist,
                },
            }),
        ));
        sink.send(Message::Text(client_hello.to_string().into()))
            .await
            .map_err(|error| EngineError::Transport(format!("websocket send failed: {error}")))?;

        // 3. Await the accepted response (10s deadline, mirroring the
        // server's own handshake timeout).
        let accepted = 'handshake: loop {
            let text = tokio::time::timeout(HANDSHAKE_TIMEOUT, next_inbound_text(&mut incoming))
                .await
                .map_err(|_| {
                    EngineError::Transport(
                        "handshake timed out waiting for the accepted response".to_string(),
                    )
                })??;
            let frames = parse_client_frames(&text).map_err(|error| {
                EngineError::Transport(format!("invalid frame during handshake: {}", error.message))
            })?;
            for frame in frames {
                if let JsonRpcFrame::Response(response) = frame
                    && response.id == Some(RequestId::Number(hello_id))
                {
                    let accepted = response.error.is_none()
                        && response.result.as_ref().and_then(|r| r.get("accepted"))
                            == Some(&Value::Bool(true));
                    break 'handshake accepted;
                }
            }
        };
        if !accepted {
            return Err(EngineError::Transport("handshake rejected".to_string()));
        }

        // 4. Shared state + connection task, then auto-subscribe (all-sessions
        // semantics: no cursor, no replay).
        let inner = Arc::new(RemoteAdapterInner {
            connected: AtomicBool::new(false),
            disposed: AtomicBool::new(false),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicI64::new(1),
            write_tx: Mutex::new(None),
            events: Mutex::new(None),
            reverse_rpc: RwLock::new(None),
            connection_task: Mutex::new(None),
            subscribed_sessions: Mutex::new(HashSet::new()),
            caps: Mutex::new(Some(caps)),
            heartbeat_ms: AtomicU64::new(heartbeat_ms),
        });
        let (write_tx, write_rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
        *inner.write_tx.lock().unwrap_or_else(|e| e.into_inner()) = Some(write_tx);
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        *inner.events.lock().unwrap_or_else(|e| e.into_inner()) = Some(events);
        spawn_connection_task(Arc::clone(&inner), sink, incoming, write_rx);
        inner.connected.store(true, Ordering::SeqCst);

        // Auto-subscribe to every session the engine already has.
        match call_internal(&inner, "session.list", json!({}), Some(HANDSHAKE_TIMEOUT)).await {
            Ok(response) if response.error.is_none() => {
                // W5: the engine's list answers the page envelope
                // (`{items, has_more}`) — unwrap the items (an older engine
                // answering the bare array still works through the fallback).
                let sessions = response
                    .result
                    .as_ref()
                    .and_then(|result| result["items"].as_array().or_else(|| result.as_array()));
                for session in sessions.into_iter().flatten() {
                    if let Some(session_id) = session.get("id").and_then(Value::as_str)
                        && let Err(error) =
                            send_subscribe(&inner, session_id, Some(HANDSHAKE_TIMEOUT)).await
                    {
                        tracing::warn!(session_id, %error, "auto-subscribe failed during connect");
                    }
                }
            }
            Ok(response) => {
                tracing::warn!(
                    error = response
                        .error
                        .as_ref()
                        .map(|error| error.message.clone())
                        .unwrap_or_default(),
                    "session.list failed during connect; existing sessions are not auto-subscribed"
                );
            }
            Err(error) => {
                tracing::warn!(%error, "session.list failed during connect");
            }
        }

        if !inner.connected.load(Ordering::SeqCst) {
            return Err(EngineError::Transport(
                "connection closed during connect".to_string(),
            ));
        }
        Ok(Arc::new(RemoteAdapter { inner }))
    }

    /// The `server_hello` facts observed at connect time — the server
    /// identity and its advertised heartbeat interval.
    pub fn caps(&self) -> Option<RemoteCaps> {
        self.inner
            .caps
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

// ---------------------------------------------------------------------------
// EngineAdapter
// ---------------------------------------------------------------------------

#[async_trait]
impl EngineAdapter for RemoteAdapter {
    async fn call_method(&self, method: &str, params: Value) -> Result<Value, EngineError> {
        let inner = &self.inner;
        if !inner.connected.load(Ordering::SeqCst) || inner.disposed.load(Ordering::SeqCst) {
            return Err(EngineError::NotConnected);
        }
        let response = call_internal(inner, method, params, None).await?;
        match response.error {
            Some(error) => {
                if error.code == mirri_proto::rpc::JSON_RPC_METHOD_NOT_FOUND {
                    Err(EngineError::MethodNotFound(method.to_string()))
                } else {
                    Err(EngineError::Rpc {
                        code: error.code,
                        message: error.message,
                        data: error.data,
                    })
                }
            }
            None => Ok(response.result.unwrap_or(Value::Null)),
        }
    }

    async fn subscribe_events(&self) -> Result<mpsc::Receiver<JsonRpcNotification>, EngineError> {
        Ok(subscribe_events_inner(&self.inner))
    }

    async fn on_reverse_rpc(&self, handler: ReverseRpcHandler) -> Result<(), EngineError> {
        *self
            .inner
            .reverse_rpc
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Some(handler);
        Ok(())
    }

    async fn dispose(&self) -> Result<(), EngineError> {
        let inner = &self.inner;
        if inner.disposed.swap(true, Ordering::SeqCst) {
            // Idempotent, like the embedded engine's dispose.
            return Ok(());
        }
        inner.connected.store(false, Ordering::SeqCst);
        fail_all_pending(inner, || EngineError::NotConnected);
        // Best-effort Close frame, then drop the writer and abort the task.
        let _ = send_outbound(inner, Outbound::Close);
        *inner.write_tx.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *inner.events.lock().unwrap_or_else(|e| e.into_inner()) = None;
        if let Some(task) = inner
            .connection_task
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            task.abort();
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

/// Upgrade to the WebSocket endpoint, offering the bearer subprotocol when a
/// token is given.
async fn connect_socket(
    url: &str,
    auth_token: Option<&str>,
) -> Result<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>, EngineError> {
    let mut request = url
        .into_client_request()
        .map_err(|error| EngineError::Transport(format!("invalid websocket url: {error}")))?;
    if let Some(token) = auth_token {
        request.headers_mut().insert(
            tokio_tungstenite::tungstenite::http::header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_str(&format!("{BEARER_SUBPROTOCOL_PREFIX}{token}")).map_err(
                |_| EngineError::Transport("auth token is not a valid header value".to_string()),
            )?,
        );
    }
    match tokio_tungstenite::connect_async(request).await {
        Ok((socket, _response)) => Ok(socket),
        Err(WsError::Http(response)) => {
            let status = response.status();
            let detail = if status == StatusCode::UNAUTHORIZED {
                "missing or invalid bearer token"
            } else {
                "http error"
            };
            Err(EngineError::Transport(format!(
                "websocket upgrade rejected ({status}): {detail}"
            )))
        }
        Err(error) => Err(EngineError::Transport(format!(
            "websocket upgrade failed: {error}"
        ))),
    }
}

/// Read the next inbound text message, skipping non-data frames.
async fn next_inbound_text(
    incoming: &mut futures::stream::SplitStream<
        WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    >,
) -> Result<String, EngineError> {
    loop {
        match incoming.next().await {
            Some(Ok(Message::Text(text))) => return Ok(text.as_str().to_string()),
            Some(Ok(Message::Close(_))) | Some(Err(_)) | None => {
                return Err(EngineError::Transport(
                    "connection closed during handshake".to_string(),
                ));
            }
            Some(Ok(_)) => continue,
        }
    }
}

/// Spawn the connection task: the single reader of the socket, the single
/// writer, the event fan-out, and the liveness watchdog.
fn spawn_connection_task(
    inner: Arc<RemoteAdapterInner>,
    sink: futures::stream::SplitSink<
        WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
        Message,
    >,
    incoming: futures::stream::SplitStream<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>>,
    mut write_rx: mpsc::Receiver<Outbound>,
) {
    let heartbeat_ms = inner.heartbeat_ms.load(Ordering::Relaxed).max(1);
    let liveness_timeout = Duration::from_millis(heartbeat_ms * HEARTBEAT_TIMEOUT_MULTIPLIER);
    let task_inner = Arc::clone(&inner);
    let task = tokio::spawn(async move {
        let inner = task_inner;
        let mut sink = sink;
        let mut incoming = incoming;
        let mut liveness_deadline = tokio::time::Instant::now() + liveness_timeout;
        loop {
            tokio::select! {
                biased;
                inbound = incoming.next() => {
                    match inbound {
                        Some(Ok(Message::Text(text))) => {
                            // Any inbound frame proves the link is alive.
                            liveness_deadline = tokio::time::Instant::now() + liveness_timeout;
                            match parse_client_frames(text.as_str()) {
                                Ok(frames) => {
                                    for frame in frames {
                                        handle_inbound_frame(&inner, frame);
                                    }
                                }
                                Err(error) => {
                                    tracing::debug!(message = %error.message, "dropping unparseable inbound frame");
                                }
                            }
                        }
                        Some(Ok(Message::Close(_))) | Some(Err(_)) | None => {
                            tracing::debug!("server closed the connection");
                            connection_died(&inner);
                            return;
                        }
                        Some(Ok(_)) => {}
                    }
                }
                outbound = write_rx.recv() => {
                    match outbound {
                        Some(Outbound::Frame(frame)) => {
                            if sink.send(Message::Text(frame.to_string().into())).await.is_err() {
                                connection_died(&inner);
                                return;
                            }
                        }
                        Some(Outbound::Close) => {
                            let _ = sink.send(Message::Close(None)).await;
                            connection_died(&inner);
                            return;
                        }
                        None => {
                            // Every writer dropped (dispose took the sender).
                            return;
                        }
                    }
                }
                _ = tokio::time::sleep_until(liveness_deadline) => {
                    tracing::warn!(
                        timeout_ms = liveness_timeout.as_millis() as u64,
                        "connection dead: no server frame within the heartbeat timeout"
                    );
                    connection_died(&inner);
                    return;
                }
            }
        }
    });
    *inner
        .connection_task
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(task);
}

/// The connection is gone (server close, write failure, or liveness expiry):
/// fail every pending call and close the event stream. Subsequent calls see
/// [`EngineError::NotConnected`] through the `connected` flag.
fn connection_died(inner: &Arc<RemoteAdapterInner>) {
    inner.connected.store(false, Ordering::SeqCst);
    fail_all_pending(inner, || {
        EngineError::Transport("connection closed".to_string())
    });
    *inner.events.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Fail every pending call with a freshly constructed `error` (dispose:
/// `NotConnected`; death: `Transport`). A closure is used because
/// [`EngineError`] is not `Clone`.
fn fail_all_pending(inner: &Arc<RemoteAdapterInner>, error: impl Fn() -> EngineError) {
    let mut pending = inner.pending.lock().unwrap_or_else(|e| e.into_inner());
    for (_, call) in pending.drain() {
        if let Some(tx) = call.response_tx {
            let _ = tx.send(Err(error()));
        }
    }
}

/// Enqueue one outbound frame without blocking the caller (the writer task
/// drains the channel). Fails when the writer is gone.
fn send_outbound(inner: &Arc<RemoteAdapterInner>, outbound: Outbound) -> Result<(), EngineError> {
    let tx = inner.write_tx.lock().unwrap_or_else(|e| e.into_inner());
    match tx.as_ref() {
        Some(tx) => tx
            .try_send(outbound)
            .map_err(|_| EngineError::Transport("connection closed".to_string())),
        None => Err(EngineError::NotConnected),
    }
}

/// Send one request and await its raw response. `timeout` is `Some` for
/// internal connect-time calls (session.list / session.subscribe) and `None`
/// for user calls — long-lived methods (e.g. `message.send` in wait mode)
/// legitimately block server-side; dead links are caught by the heartbeat.
async fn call_internal(
    inner: &Arc<RemoteAdapterInner>,
    method: &str,
    params: Value,
    timeout: Option<Duration>,
) -> Result<JsonRpcResponse, EngineError> {
    let id = inner.next_id.fetch_add(1, Ordering::SeqCst);
    let (tx, rx) = oneshot::channel();
    inner
        .pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            id,
            PendingCall {
                method: method.to_string(),
                response_tx: Some(tx),
                deferred_result: None,
            },
        );
    let frame = JsonRpcFrame::Request(JsonRpcRequest::new(Some(id), method, params));
    if let Err(error) = send_outbound(inner, Outbound::Frame(frame)) {
        inner
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
        return Err(error);
    }
    let outcome = match timeout {
        Some(duration) => tokio::time::timeout(duration, rx)
            .await
            .map_err(|_| EngineError::Transport(format!("call timed out: {method}")))?,
        None => rx.await,
    };
    outcome.map_err(|_| EngineError::Transport("connection closed".to_string()))?
}

/// Subscribe to a session over the wire (no cursor — the embedded engine has
/// no replay either). Dedupes on the adapter's subscription set.
async fn send_subscribe(
    inner: &Arc<RemoteAdapterInner>,
    session_id: &str,
    timeout: Option<Duration>,
) -> Result<JsonRpcResponse, EngineError> {
    if inner
        .subscribed_sessions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(session_id)
    {
        return Ok(JsonRpcResponse::ok(0, json!({ "seq": 0 })));
    }
    let id = inner.next_id.fetch_add(1, Ordering::SeqCst);
    let (tx, rx) = oneshot::channel();
    inner
        .pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(
            id,
            PendingCall {
                method: "session.subscribe".to_string(),
                response_tx: Some(tx),
                deferred_result: None,
            },
        );
    let frame = JsonRpcFrame::Request(JsonRpcRequest::new(
        Some(id),
        "session.subscribe",
        json!({ "sessionId": session_id }),
    ));
    if let Err(error) = send_outbound(inner, Outbound::Frame(frame)) {
        inner
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
        return Err(error);
    }
    inner
        .subscribed_sessions
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(session_id.to_string());
    let outcome = match timeout {
        Some(duration) => tokio::time::timeout(duration, rx)
            .await
            .map_err(|_| EngineError::Transport(format!("subscribe timed out: {session_id}")))?,
        None => rx.await,
    };
    outcome.map_err(|_| EngineError::Transport("connection closed".to_string()))?
}

// ---------------------------------------------------------------------------
// Inbound routing
// ---------------------------------------------------------------------------

/// Route one inbound frame.
fn handle_inbound_frame(inner: &Arc<RemoteAdapterInner>, frame: JsonRpcFrame) {
    match frame {
        JsonRpcFrame::Request(request) => handle_reverse_request(inner, request),
        JsonRpcFrame::Response(response) => handle_response(inner, response),
        JsonRpcFrame::Notification(notification) => handle_notification(inner, notification),
    }
}

/// A server-minted request (reverse RPC): run the registered handler and
/// answer `{id, result}` on success or `{id, error: -32603}` on handler
/// failure; without a handler, answer `-32601` (mirrors the TS client).
fn handle_reverse_request(inner: &Arc<RemoteAdapterInner>, request: JsonRpcRequest) {
    let Some(id) = request.id else {
        // The strict codec classifies id-less frames as notifications; this
        // branch is defensive only.
        return;
    };
    let method = request.method.clone();
    let handler = inner
        .reverse_rpc
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let Some(handler) = handler else {
        tracing::debug!(
            ?id,
            method,
            "reverse RPC request without a registered handler"
        );
        let frame = JsonRpcFrame::Response(JsonRpcResponse::err(
            id,
            JsonRpcError::new(
                mirri_proto::rpc::JSON_RPC_METHOD_NOT_FOUND,
                format!("method not found: {method}"),
            ),
        ));
        let _ = send_outbound(inner, Outbound::Frame(frame));
        return;
    };
    // The handler is async and may block on the client: run it on a spawned
    // task so the connection loop keeps reading (heartbeats, events).
    let inner_for_task = Arc::clone(inner);
    tokio::spawn(async move {
        let result = handler(method.clone(), request.params).await;
        let frame = match result {
            Ok(value) => JsonRpcFrame::Response(JsonRpcResponse::ok(id, value)),
            Err(message) => JsonRpcFrame::Response(JsonRpcResponse::err(
                id,
                JsonRpcError::new(mirri_proto::rpc::JSON_RPC_INTERNAL, message),
            )),
        };
        if let Some(tx) = inner_for_task
            .write_tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .cloned()
        {
            let _ = tx.try_send(Outbound::Frame(frame));
        }
    });
}

/// Route one inbound response to its awaiting caller. Responses to
/// creating-session methods auto-subscribe to the returned session before
/// the caller's future is completed (subscribe-before-create ordering).
fn handle_response(inner: &Arc<RemoteAdapterInner>, response: JsonRpcResponse) {
    // The strict codec admits integer ids only, so a well-formed response
    // carries a `Number`; anything else is defensive-unreachable.
    let Some(RequestId::Number(id)) = response.id else {
        return;
    };
    let Some(mut call) = inner
        .pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&id)
    else {
        tracing::warn!(?id, "response to unknown request");
        return;
    };
    let Some(response_tx) = call.response_tx.take() else {
        return;
    };
    // The ack of a follow-up auto-subscribe completes the ORIGINAL caller
    // (the session.create response is delivered only after the session is
    // subscribed on the wire — subscribe-before-create ordering).
    if let Some(original) = call.deferred_result {
        if response.error.is_some() {
            tracing::warn!(?id, "auto-subscribe was not acknowledged by the server");
        }
        let _ = response_tx.send(Ok(original));
        return;
    }
    let session_id = if SESSION_CREATING_METHODS.contains(&call.method.as_str()) {
        response
            .result
            .as_ref()
            .and_then(|result| result.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string)
    } else {
        None
    };
    match session_id {
        Some(session_id) => {
            // The caller waits for the subscribe ack: transplant its sender
            // into a follow-up entry keyed by the subscribe request id.
            let subscribe_id = inner.next_id.fetch_add(1, Ordering::SeqCst);
            inner
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
                    subscribe_id,
                    PendingCall {
                        method: "session.subscribe".to_string(),
                        response_tx: Some(response_tx),
                        deferred_result: Some(response),
                    },
                );
            let frame = JsonRpcFrame::Request(JsonRpcRequest::new(
                Some(subscribe_id),
                "session.subscribe",
                json!({ "sessionId": session_id }),
            ));
            inner
                .subscribed_sessions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(session_id.clone());
            if let Err(error) = send_outbound(inner, Outbound::Frame(frame)) {
                // The writer is gone; fail_all_pending on death completes the
                // caller through the follow-up entry.
                tracing::debug!(session_id, %error, "auto-subscribe request failed");
            }
        }
        None => {
            let _ = response_tx.send(Ok(response));
        }
    }
}

/// Route one inbound notification.
fn handle_notification(inner: &Arc<RemoteAdapterInner>, notification: JsonRpcNotification) {
    match notification.method.as_str() {
        "server_hello" => {
            // Defensive: the hello was consumed during the handshake; a
            // second copy only refreshes the advertised facts.
            if let Some(heartbeat_ms) = notification
                .params
                .get("heartbeatMs")
                .and_then(Value::as_u64)
                .filter(|ms| *ms > 0)
            {
                inner.heartbeat_ms.store(heartbeat_ms, Ordering::Relaxed);
            }
            if let Some(caps) = inner
                .caps
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_mut()
                && let Some(version) = notification.params.get("version").and_then(Value::as_str)
            {
                caps.version = version.to_string();
            }
        }
        "ping" => {
            // The server probes liveness with its own nonce — answer with a
            // pong echoing it immediately.
            let nonce = notification
                .params
                .get("nonce")
                .cloned()
                .unwrap_or(Value::Null);
            let pong = JsonRpcFrame::Notification(JsonRpcNotification::new(
                "pong",
                json!({ "nonce": nonce }),
            ));
            let _ = send_outbound(inner, Outbound::Frame(pong));
        }
        "pong" => {
            // Liveness is tracked per inbound frame; nothing else to do.
        }
        method if method == "event" || method.starts_with("event.") => {
            if let Some(events) = inner
                .events
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
            {
                // Broadcast fan-out never blocks the connection task; a
                // lagged subscriber is resynced by dropping events for it.
                let _ = events.send(notification);
            }
        }
        other => {
            tracing::debug!(method = other, "ignoring unknown server notification");
        }
    }
}

/// Subscribe to the live event fan-out. Each call bridges broadcast → mpsc
/// in its own task, so one slow subscriber never blocks another (mirroring
/// the embedded engine's Vec-of-senders semantics); a subscriber that does
/// not drain fast enough drops events (the documented resync signal) and a
/// closed bridge ends its stream.
fn subscribe_events_inner(inner: &Arc<RemoteAdapterInner>) -> mpsc::Receiver<JsonRpcNotification> {
    let (tx, rx) = mpsc::channel(EVENT_CHANNEL_CAPACITY);
    let sender = inner
        .events
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .cloned();
    let Some(sender) = sender else {
        // The connection is gone or disposed: return an already-closed
        // receiver so consumers observe the stream ending instead of
        // hanging on a silent channel.
        drop(tx);
        return rx;
    };
    tokio::spawn(async move {
        let mut broadcast_rx = sender.subscribe();
        loop {
            match broadcast_rx.recv().await {
                Ok(notification) => {
                    if tx.try_send(notification).is_err() {
                        // Full: the subscriber is too slow — drop the event
                        // for it. Closed: the subscriber is gone — end the
                        // bridge task.
                        if tx.is_closed() {
                            break;
                        }
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });
    rx
}
