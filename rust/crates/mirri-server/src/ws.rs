//! Multiplexed WebSocket endpoint (`/api/v1/ws`).
//!
//! Legacy surface: this endpoint predates the unified `event.*` projection
//! and speaks its own raw-`SessionEvent` frame dialect. It was adjudicated
//! out of the RPC contract unification scope (M4 ruling, see the plan
//! ledger) — new consumers should use the JSON-RPC endpoint in
//! [`crate::ws_rpc`] instead.
//!
//! One connection can subscribe to any number of sessions: every
//! [`SessionEvent`] published on a subscribed session's event bus is
//! forwarded to the connection wrapped in a `session_event` frame that names
//! the session it belongs to. The transport speaks JSON text frames:
//!
//! ## Handshake (strict ordering)
//!
//! 1. server → client, immediately after the upgrade:
//!    `{"type":"server_hello","serverId":"<id>","version":"<version>"}`
//! 2. client → server, within [`HANDSHAKE_TIMEOUT`] or the connection is
//!    dropped: `{"type":"client_hello","clientId":"<client-id>"}`
//!
//! ## Client → server control frames (after the handshake)
//!
//! - `{"type":"subscribe","sessionId":"<id>"}` — start receiving this
//!   session's events (idempotent; one connection may subscribe to many
//!   sessions).
//! - `{"type":"unsubscribe","sessionId":"<id>"}` — stop receiving this
//!   session's events (idempotent).
//! - `{"type":"interrupt","sessionId":"<id>"}` — interrupt the session's
//!   running agent.
//! - `{"type":"ping"}` — liveness probe; an optional top-level `nonce`
//!   string is echoed back in the `pong`.
//!
//! ## Server → client frames
//!
//! - `{"type":"subscribed","sessionId":"<id>"}` — subscribe accepted.
//! - `{"type":"unsubscribed","sessionId":"<id>"}` — unsubscribe accepted.
//! - `{"type":"interrupt_ack","sessionId":"<id>"}` — interrupt accepted.
//! - `{"type":"pong"}` — reply to a client `ping` (echoes the `nonce` when
//!   one was sent).
//! - `{"type":"session_event","sessionId":"<id>","event":{...}}` — one
//!   [`SessionEvent`] in its internally-tagged JSON form; the inner `type`
//!   discriminates (`session_closed`, `message_received`, ...).
//! - `{"type":"error","message":"<reason>"}` — a control frame could not be
//!   honored (e.g. subscribing to a session that does not exist, or a
//!   non-`client_hello` frame during the handshake).
//!
//! When a subscribed session closes, its `session_closed` event is forwarded
//! first and the subscription is dropped; the connection stays open for its
//! other sessions. Unrecognized client frames are logged and ignored. The
//! connection closes when the client disconnects or the handshake fails.

use std::collections::HashMap;
use std::time::Duration;

use axum::{
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    response::Response,
};
use futures::{
    SinkExt, StreamExt,
    stream::{SplitSink, SplitStream},
};
use serde_json::{Map, Value, json};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

use mirri_core::{SessionEvent, SessionId};

use crate::state::ServerState;

/// How long the server waits for the client's `client_hello` after sending
/// `server_hello` before dropping the connection.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Capacity of the internal channel carrying session events from the
/// per-subscription forward tasks to the connection's main loop.
const EVENT_CHANNEL_CAPACITY: usize = 256;

/// Upgrade an HTTP connection to the multiplexed WebSocket endpoint.
pub async fn handle_ws(ws: WebSocketUpgrade, State(state): State<ServerState>) -> Response {
    ws.on_upgrade(move |socket| run_connection(socket, state))
}

/// Drive one WebSocket connection: perform the handshake, then multiplex
/// every subscribed session's events until the client disconnects.
async fn run_connection(socket: WebSocket, state: ServerState) {
    let (mut sink, mut incoming) = socket.split();

    if !send_server_hello(&mut sink, &state).await
        || !await_client_hello(&mut incoming, &mut sink).await
    {
        tracing::debug!("connection closed during handshake");
        return;
    }

    let (event_tx, mut event_rx) = mpsc::channel::<(String, SessionEvent)>(EVENT_CHANNEL_CAPACITY);
    let mut subscriptions: HashMap<String, Subscription> = HashMap::new();

    loop {
        tokio::select! {
            incoming_message = incoming.next() => {
                match incoming_message {
                    Some(Ok(Message::Text(text))) => {
                        let text: &str = &text;
                        let keep_going = handle_client_message(
                            text,
                            &mut sink,
                            &state,
                            &mut subscriptions,
                            &event_tx,
                        )
                        .await;
                        if !keep_going {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    // WS-level ping/pong and binary frames are handled by
                    // axum/tungstenite or ignored by this protocol.
                    Some(Ok(_)) => {}
                }
            }
            forwarded = event_rx.recv() => {
                match forwarded {
                    Some((session_id, event)) => {
                        // Events queued before an `unsubscribe` was processed
                        // are dropped, so unsubscribe takes effect immediately.
                        if !subscriptions.contains_key(&session_id) {
                            continue;
                        }
                        if !send_session_event(&mut sink, &session_id, &event).await {
                            break;
                        }
                        if matches!(event, SessionEvent::SessionClosed { .. }) {
                            remove_subscription(&mut subscriptions, &session_id);
                        }
                    }
                    None => break,
                }
            }
        }
    }
}

/// A live subscription to one session's event bus.
struct Subscription {
    /// Handle to the task forwarding the session's events into the
    /// connection's internal channel; aborted on unsubscribe or session
    /// close.
    forward_task: JoinHandle<()>,
}

/// Send the initial `server_hello` greeting carrying the server's identity.
/// Returns `false` when the client is gone.
async fn send_server_hello(sink: &mut SplitSink<WebSocket, Message>, state: &ServerState) -> bool {
    send_frame(
        sink,
        json!({
            "type": "server_hello",
            "serverId": state.server_id,
            "version": state.version,
        }),
    )
    .await
}

/// Wait for the client's `client_hello`. Returns `false` when the client
/// disconnects, sends a different frame first, or the [`HANDSHAKE_TIMEOUT`]
/// elapses.
async fn await_client_hello(
    incoming: &mut SplitStream<WebSocket>,
    sink: &mut SplitSink<WebSocket, Message>,
) -> bool {
    loop {
        match tokio::time::timeout(HANDSHAKE_TIMEOUT, incoming.next()).await {
            Err(_elapsed) => {
                tracing::warn!("client_hello not received within the handshake timeout");
                return false;
            }
            Ok(None) | Ok(Some(Err(_))) => return false,
            Ok(Some(Ok(Message::Text(text)))) => {
                let text: &str = &text;
                if frame_type(text).as_deref() == Some("client_hello") {
                    tracing::debug!("client_hello received; handshake complete");
                    return true;
                }
                tracing::warn!(text, "expected client_hello during handshake");
                let _ = send_frame(
                    sink,
                    json!({ "type": "error", "message": "expected client_hello" }),
                )
                .await;
                return false;
            }
            Ok(Some(Ok(Message::Close(_)))) => return false,
            // WS-level ping/pong and binary frames are not the handshake.
            Ok(Some(Ok(_))) => continue,
        }
    }
}

/// Handle one client control frame. Returns `false` when the client is gone
/// and the connection should be torn down.
async fn handle_client_message(
    text: &str,
    sink: &mut SplitSink<WebSocket, Message>,
    state: &ServerState,
    subscriptions: &mut HashMap<String, Subscription>,
    event_tx: &mpsc::Sender<(String, SessionEvent)>,
) -> bool {
    let Ok(Value::Object(fields)) = serde_json::from_str::<Value>(text) else {
        tracing::warn!(text, "unrecognized client message (not a JSON object)");
        return true;
    };
    let Some(message_type) = fields.get("type").and_then(Value::as_str) else {
        tracing::warn!(text, "unrecognized client message (missing type field)");
        return true;
    };
    match message_type {
        "client_hello" => {
            tracing::warn!("duplicate client_hello ignored");
            true
        }
        "subscribe" => {
            let Some(session_id) = fields.get("sessionId").and_then(Value::as_str) else {
                tracing::warn!(text, "subscribe without sessionId");
                return true;
            };
            subscribe_session(session_id, sink, state, subscriptions, event_tx).await
        }
        "unsubscribe" => {
            let Some(session_id) = fields.get("sessionId").and_then(Value::as_str) else {
                tracing::warn!(text, "unsubscribe without sessionId");
                return true;
            };
            unsubscribe_session(session_id, sink, subscriptions).await
        }
        "ping" => send_pong(sink, &fields).await,
        "interrupt" => {
            let Some(session_id) = fields.get("sessionId").and_then(Value::as_str) else {
                tracing::warn!(text, "interrupt without sessionId");
                return true;
            };
            ack_interrupt(sink, session_id).await
        }
        other => {
            tracing::warn!(message_type = other, "unrecognized client message type");
            true
        }
    }
}

/// Start forwarding events from one session's bus. Sends `subscribed` on
/// success and `error` when the session is unknown; a repeated `subscribe`
/// for an already-subscribed session is re-acknowledged. Returns `false`
/// when the client is gone.
async fn subscribe_session(
    session_id: &str,
    sink: &mut SplitSink<WebSocket, Message>,
    state: &ServerState,
    subscriptions: &mut HashMap<String, Subscription>,
    event_tx: &mpsc::Sender<(String, SessionEvent)>,
) -> bool {
    if subscriptions.contains_key(session_id) {
        tracing::debug!(
            session_id,
            "subscribe for already-subscribed session; re-acknowledging"
        );
        return send_frame(
            sink,
            json!({ "type": "subscribed", "sessionId": session_id }),
        )
        .await;
    }
    let Some(artifacts) = state
        .session_manager
        .session_artifacts(&SessionId::new(session_id.to_string()))
    else {
        tracing::warn!(session_id, "subscribe for unknown session");
        return send_frame(
            sink,
            json!({ "type": "error", "message": format!("session not found: {session_id}") }),
        )
        .await;
    };

    let forward_task = spawn_event_forwarder(
        event_tx.clone(),
        session_id,
        artifacts.event_bus.subscribe(),
    );
    subscriptions.insert(session_id.to_string(), Subscription { forward_task });
    tracing::info!(session_id, "session subscribed");
    send_frame(
        sink,
        json!({ "type": "subscribed", "sessionId": session_id }),
    )
    .await
}

/// Stop forwarding events from one session. Sends `unsubscribed` even when
/// the session was not subscribed (idempotent). Returns `false` when the
/// client is gone.
async fn unsubscribe_session(
    session_id: &str,
    sink: &mut SplitSink<WebSocket, Message>,
    subscriptions: &mut HashMap<String, Subscription>,
) -> bool {
    match subscriptions.remove(session_id) {
        Some(subscription) => {
            subscription.forward_task.abort();
            tracing::debug!(session_id, "session unsubscribed");
        }
        None => {
            tracing::debug!(
                session_id,
                "unsubscribe for not-subscribed session; acknowledging"
            );
        }
    }
    send_frame(
        sink,
        json!({ "type": "unsubscribed", "sessionId": session_id }),
    )
    .await
}

/// Drop a subscription (without acknowledging) once its session closed.
fn remove_subscription(subscriptions: &mut HashMap<String, Subscription>, session_id: &str) {
    if let Some(subscription) = subscriptions.remove(session_id) {
        subscription.forward_task.abort();
        tracing::debug!(session_id, "subscription dropped; session closed");
    }
}

/// Spawn the task that drains one session's broadcast receiver into the
/// connection's internal channel. The task exits after `session_closed` or
/// when the channel or bus closes; `JoinHandle::abort` covers unsubscribe.
fn spawn_event_forwarder(
    event_tx: mpsc::Sender<(String, SessionEvent)>,
    session_id: &str,
    mut receiver: broadcast::Receiver<SessionEvent>,
) -> JoinHandle<()> {
    let session_id = session_id.to_string();
    tokio::spawn(async move {
        loop {
            match receiver.recv().await {
                Ok(event) => {
                    let session_closed = matches!(event, SessionEvent::SessionClosed { .. });
                    if event_tx.send((session_id.clone(), event)).await.is_err() {
                        break; // the connection's main loop is gone
                    }
                    if session_closed {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(
                        session_id,
                        skipped,
                        "session event stream lagged; dropped events"
                    );
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

/// Reply to a client `ping`, echoing an optional client-supplied `nonce`.
/// Returns `false` when the client is gone.
async fn send_pong(sink: &mut SplitSink<WebSocket, Message>, ping: &Map<String, Value>) -> bool {
    let mut pong = json!({ "type": "pong" });
    if let Some(Value::String(nonce)) = ping.get("nonce") {
        pong["nonce"] = Value::String(nonce.clone());
    }
    send_frame(sink, pong).await
}

/// Acknowledge an `interrupt` command for a session. Returns `false` when
/// the client is gone.
async fn ack_interrupt(sink: &mut SplitSink<WebSocket, Message>, session_id: &str) -> bool {
    tracing::info!(session_id, "interrupt command received");
    send_frame(
        sink,
        json!({ "type": "interrupt_ack", "sessionId": session_id }),
    )
    .await
}

/// Forward one session event wrapped in a `session_event` frame. Returns
/// `false` when the client is gone.
async fn send_session_event(
    sink: &mut SplitSink<WebSocket, Message>,
    session_id: &str,
    event: &SessionEvent,
) -> bool {
    let Ok(event_value) = serde_json::to_value(event) else {
        tracing::error!(session_id, "failed to serialize session event");
        return true; // one dropped event must not kill the connection
    };
    send_frame(
        sink,
        json!({ "type": "session_event", "sessionId": session_id, "event": event_value }),
    )
    .await
}

/// Serialize and send one JSON frame. Returns `false` when the client is
/// gone.
async fn send_frame(sink: &mut SplitSink<WebSocket, Message>, frame: Value) -> bool {
    match sink.send(Message::Text(frame.to_string().into())).await {
        Ok(()) => true,
        Err(_) => {
            tracing::debug!("client disconnected from WebSocket");
            false
        }
    }
}

/// Extract the `type` field of a JSON frame, if it is a string.
fn frame_type(text: &str) -> Option<String> {
    serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|frame| frame.get("type").and_then(Value::as_str).map(str::to_owned))
}
