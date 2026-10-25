//! Unified JSON-RPC 2.0 WebSocket endpoint (`/api/v1/rpc`).
//!
//! This is the Phase-2 home of the unified protocol (design
//! `.tmp/rust-core-design/01-protocol-unification.md`, §3.1/§5/§9): every
//! client interaction — forward RPC, event stream, reverse RPC — runs over
//! one JSON-RPC 2.0 connection. The legacy multiplexed endpoint (`/api/v1/ws`,
//! [`crate::ws`]) stays untouched during the migration.
//!
//! ## Handshake (strict ordering, §5)
//!
//! 1. Server → client, immediately after the upgrade: a `server_hello`
//!    notification naming the server identity, the heartbeat interval, the
//!    event log size, and the offered capabilities:
//!    ```json
//!    {"jsonrpc":"2.0","method":"server_hello",
//!     "params":{"serverId":"...","version":"...","heartbeatMs":15000,
//!               "maxEventBufferSize":256,"capabilities":{"eventBatching":true}}}
//!    ```
//! 2. Client → server, within [`HANDSHAKE_TIMEOUT`] or the connection is
//!    dropped: a `client_hello` **request** carrying `clientId` and the
//!    client's capabilities (`reverseRpc` whitelist).
//! 3. Server → client: `client_hello` accepted (`{"accepted": true}`) — the
//!    handshake success signal.
//!
//! ## Authentication (§13.1)
//!
//! When [`ServerState::auth_token`] is configured the client must offer the
//! `mirri-code.bearer.<token>` subprotocol in `Sec-WebSocket-Protocol`
//! (browsers cannot set `Authorization` on WebSocket upgrades). The upgrade
//! is rejected with 401 otherwise; missing and wrong tokens get the same
//! response. The HTTP auth middleware skips this route (see
//! [`crate::auth`]) — the endpoint authenticates itself.
//!
//! ## Life after the handshake
//!
//! - **Forward RPC**: incoming requests dispatch to the shared RPC registry
//!   ([`ServerState::rpc_registry`]); `session.subscribe` /
//!   `session.unsubscribe` / `mcp.subscribe` / `ping` are handled
//!   connection-locally.
//! - **Terminal stream**: the web terminal panel's five control frames
//!   (`terminal_attach` / `terminal_input` / `terminal_resize` /
//!   `terminal_detach` / `terminal_close`) are connection-local too — they
//!   address per-connection attach state on the session's PTY
//!   [`TerminalPool`](mirri_core::terminal_pool::TerminalPool). Attached
//!   terminals stream their output back as `terminal_output` /
//!   `terminal_exit` notifications (snake_case payload, the v1 multiplexed
//!   endpoint's frame shapes).
//! - **Events**: subscribing to a session attaches this connection to the
//!   session's [`SessionEventChannel`]. The channel's log task stamps every
//!   durable [`SessionEvent`] with a per-session `{seq, epoch}` cursor
//!   (in-memory ring buffer, [`EVENT_LOG_CAPACITY`]), then the connection's
//!   forwarder relays it through mirri-core's shared event projection
//!   ([`event_projection::event_notification`]) as an `event.<type>`
//!   notification with a flat `params` envelope (`sessionId`, `timestamp`,
//!   `volatile`, plus `seq`/`epoch` when durable) and the camelCase payload
//!   fields merged in. Subscribing with a `cursor` replays missed events
//!   from the ring buffer; a cursor whose `epoch` no longer matches gets no
//!   replay (resync).
//! - **Batching**: consecutive pending notifications are coalesced into one
//!   array frame (≤ [`MAX_BATCH_SIZE`] members), the `eventBatching`
//!   capability advertised in `server_hello`.
//! - **Reverse RPC**: the server may send requests with incrementing integer
//!   ids; incoming responses resolve the matching pending one-shot
//!   ([`ReverseRpcState`]). The engine initiates reverse RPC through
//!   [`WsReverseRpcCallback`], which is stored on the session's artifacts
//!   when a client subscribes (`approval.ask` / `question.ask` wiring).
//! - **Heartbeat**: every [`ServerState::heartbeat_interval`] the server
//!   sends a `ping` notification with a fresh `nonce`; a client answers with
//!   a `pong` notification echoing it. Any inbound frame counts as liveness;
//!   a connection silent for `3 × interval` is dropped.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::HeaderMap,
    response::Response,
};
use futures::{
    SinkExt, StreamExt,
    stream::{SplitSink, SplitStream},
};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use uuid::Uuid;

use mirri_core::{
    EventBus, SessionEvent, SinkHandle, TerminalFrame, event_projection,
    reverse_rpc::CHANNEL_ERROR_PREFIX, terminal_pool::TerminalPool,
};

use crate::auth;
use crate::rpc::RpcError;
use crate::state::ServerState;
use mirri_transport::{
    INVALID_PARAMS, INVALID_REQUEST, JsonRpcError, JsonRpcFrame, JsonRpcNotification,
    JsonRpcRequest, JsonRpcResponse, RequestId, protocol_error_frame,
};

/// How long the server waits for the client's `client_hello` request after
/// sending `server_hello` before dropping the connection (design §5.1/§13.3).
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Per-session durable event log capacity: how many events the in-memory
/// ring buffer keeps for cursor replay (`maxEventBufferSize` in
/// `server_hello`; design §5.4).
pub(crate) const EVENT_LOG_CAPACITY: usize = 256;

/// Capacity of the internal channel carrying outbound frames from the
/// connection's tasks to its writer.
const OUTBOX_CAPACITY: usize = 256;

/// Upper bound of one batched array frame (design §4.1.4).
const MAX_BATCH_SIZE: usize = 256;

/// Upper bound on one outbound frame's serialized size. The outbox holds up
/// to `OUTBOX_CAPACITY` frames of already-serialized responses in memory
/// while a slow client drains; without a per-frame cap a single runaway
/// response (an accidental full-transcript serialization, a pathological
/// tool result) multiplied by the queue depth becomes a server-side memory
/// spike that has nothing to do with the channel's frame count. Frames over
/// the cap are refused — the caller surfaces a JSON-RPC error the client can
/// act on instead of the server silently ballooning.
const MAX_OUTBOUND_FRAME_BYTES: usize = 32 * 1024 * 1024;

/// Subprotocol prefix carrying the bearer token on the WebSocket upgrade
/// (design §13.1). The token follows the prefix; the server compares it in
/// constant time and never re-emits it into frame bodies.
const BEARER_SUBPROTOCOL_PREFIX: &str = "mirri-code.bearer.";

/// A connection with no inbound frame for this many heartbeat intervals is
/// considered dead and dropped (design §5.5).
const HEARTBEAT_TIMEOUT_MULTIPLIER: u32 = 3;

/// How long a session's `resync_required` signal suppresses cursor replay
/// after firing. The signal tells the lagging client to rebuild from a
/// snapshot; a client that instead re-subscribes with the same stale cursor
/// re-arms the replay push, and a slow one can enter a lag→resync→replay
/// loop that amplifies the very backlog it is trying to escape. Within the
/// cooldown the signal still fires but the replay stays off.
const RESYNC_COOLDOWN: Duration = Duration::from_secs(2);

/// Upgrade an HTTP connection to the unified JSON-RPC WebSocket endpoint,
/// enforcing bearer-subprotocol authentication when a token is configured.
pub async fn handle_ws_rpc(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    State(state): State<ServerState>,
) -> Response {
    match bearer_authentication(&state, &headers) {
        Ok(Some(subprotocol)) => {
            // Echo the (already validated) subprotocol back to the client so
            // browser clients observe the negotiation succeeding.
            ws.protocols([subprotocol])
                .on_upgrade(move |socket| run_connection(socket, state))
        }
        Ok(None) => {
            // Open mode (no token configured — e.g. --dangerous-bypass-auth):
            // a client that still offered the bearer subprotocol (a browser
            // holding a stale credential from an earlier auth-mode run) needs
            // the server to select one — browsers abort the upgrade (1006)
            // when their offer goes unanswered. Echo the offered subprotocol
            // back (the token itself is not validated — bypass mode never
            // validates one) so the handshake completes.
            match bearer_subprotocol(&headers) {
                Some(offered) => ws
                    .protocols([offered])
                    .on_upgrade(move |socket| run_connection(socket, state)),
                None => ws.on_upgrade(move |socket| run_connection(socket, state)),
            }
        }
        Err(()) => {
            // Same response for a missing and a wrong token (design §13.1):
            // nothing beyond token validity is revealed.
            tracing::warn!("rejected /api/v1/rpc upgrade: missing or invalid bearer subprotocol");
            let request_id = crate::http::request_id_from(&headers);
            auth::unauthorized_response(&request_id)
        }
    }
}

/// Check the upgrade's bearer subprotocol against the configured auth token.
///
/// Returns `Ok(None)` in open mode (no token configured), `Ok(Some(protocol))`
/// with the validated full subprotocol string to echo, and `Err(())` when the
/// token is missing or wrong.
fn bearer_authentication(state: &ServerState, headers: &HeaderMap) -> Result<Option<String>, ()> {
    let Some(expected_token) = &state.auth_token else {
        return Ok(None);
    };
    let Some(protocol) = bearer_subprotocol(headers) else {
        return Err(());
    };
    let Some(supplied_token) = protocol.strip_prefix(BEARER_SUBPROTOCOL_PREFIX) else {
        return Err(());
    };
    if auth::token_matches(supplied_token, expected_token) {
        Ok(Some(protocol))
    } else {
        Err(())
    }
}

/// Extract the first `mirri-code.bearer.<token>` subprotocol the client
/// offered in `Sec-WebSocket-Protocol` (multiple values and comma-separated
/// lists are both accepted).
fn bearer_subprotocol(headers: &HeaderMap) -> Option<String> {
    for value in headers.get_all(axum::http::header::SEC_WEBSOCKET_PROTOCOL) {
        let Ok(text) = value.to_str() else {
            continue;
        };
        for protocol in text.split(',') {
            let protocol = protocol.trim();
            if let Some(token) = protocol.strip_prefix(BEARER_SUBPROTOCOL_PREFIX)
                && !token.is_empty()
            {
                return Some(protocol.to_string());
            }
        }
    }
    None
}

// ===========================================================================
// Per-session event log (durable events + cursor replay)
// ===========================================================================

/// One durable event stamped with its per-session cursor position.
#[derive(Debug)]
pub(crate) struct SessionEventRecord {
    /// Monotonic per-session sequence, starting at 1 (design §9.1). Volatile
    /// (live-only) events carry seq 0; they never enter the durable log.
    pub(crate) seq: u64,
    /// Log generation marker attached to every record (the server instance
    /// id: it changes on restart, invalidating old client cursors).
    pub(crate) epoch: Arc<str>,
    pub(crate) event: SessionEvent,
}

/// The in-memory ring buffer behind one session's cursor replay.
struct EventLogInner {
    /// Records in seq-ascending order, capped at the channel capacity
    /// (oldest evicted first).
    entries: VecDeque<Arc<SessionEventRecord>>,
    /// Seq to assign to the next appended event (1-based).
    next_seq: u64,
}

impl EventLogInner {
    fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            next_seq: 1,
        }
    }

    /// The seq of the newest record, or 0 for an empty log.
    fn tail_seq(&self) -> u64 {
        self.entries.back().map_or(0, |record| record.seq)
    }

    /// Records with `seq > cursor.seq`, in order. Events evicted from the
    /// ring buffer are simply absent — a cursor behind the buffer starts
    /// replay at the earliest retained record.
    fn replay_after(&self, cursor: Option<&Cursor>) -> Vec<Arc<SessionEventRecord>> {
        let Some(cursor) = cursor else {
            return Vec::new();
        };
        self.entries
            .iter()
            .filter(|record| record.seq > cursor.seq)
            .cloned()
            .collect()
    }
}

/// One session's durable event log plus live fan-out — an opaque handle
/// shared by every `/api/v1/rpc` connection subscribed to that session.
///
/// A single log task (spawned by [`SessionEventChannelRegistry::get_or_create`])
/// is the **only** appender: it receives every [`SessionEvent`] from the
/// session's event bus, stamps a seq under the log lock, appends to the ring
/// buffer, and publishes the record to this channel's live broadcast — all
/// atomically, so a subscriber's replay window and live receiver can never
/// overlap or gap (see [`SessionEventChannel::subscribe`]). Volatile events
/// (streaming chunks, `Unknown`) skip the log and are broadcast live with
/// seq 0 instead.
pub struct SessionEventChannel {
    epoch: Arc<str>,
    capacity: usize,
    log: std::sync::Mutex<EventLogInner>,
    live: broadcast::Sender<Arc<SessionEventRecord>>,
}

impl SessionEventChannel {
    fn new(epoch: Arc<str>, capacity: usize) -> Self {
        let (live, _) = broadcast::channel(capacity);
        Self {
            epoch,
            capacity,
            log: std::sync::Mutex::new(EventLogInner::new()),
            live,
        }
    }

    /// Append one event and publish it to live subscribers.
    ///
    /// Durable events are stamped with the next seq and retained in the ring
    /// buffer; the publish happens while the log lock is held, so record
    /// visibility in the log and on the live channel are ordered against
    /// [`Self::subscribe`]. Volatile events bypass the durable log entirely
    /// — they are stamped with seq 0 and go only to live subscribers, since
    /// they are fire-and-forget stream notifications and must never be
    /// replayed via cursor.
    fn append(&self, event: SessionEvent) {
        let volatile = event_projection::is_volatile(&event);
        let record = if volatile {
            Arc::new(SessionEventRecord {
                seq: 0,
                epoch: Arc::clone(&self.epoch),
                event,
            })
        } else {
            let mut inner = self.log.lock().unwrap_or_else(|e| e.into_inner());
            let record = Arc::new(SessionEventRecord {
                seq: inner.next_seq,
                epoch: Arc::clone(&self.epoch),
                event,
            });
            inner.next_seq += 1;
            while inner.entries.len() >= self.capacity {
                inner.entries.pop_front();
            }
            inner.entries.push_back(Arc::clone(&record));
            record
        };
        // No live subscriber is not an error: durable records additionally
        // stay in the log for cursor replay.
        let _ = self.live.send(record);
    }

    /// Atomically capture the replay window and a live receiver for a new
    /// subscriber.
    ///
    /// Returns the records to replay (`seq > cursor.seq`, empty when no
    /// cursor or a mismatched epoch was given), the current tail seq for the
    /// subscribe acknowledgment, and a live receiver positioned after every
    /// record covered by the replay. Because `append` publishes durable
    /// records under the same lock, record `N` is either in the replay set
    /// or on this receiver — never both, never neither. Volatile records
    /// (seq 0) are never in the log and arrive only on the live receiver.
    pub(crate) fn subscribe(
        &self,
        cursor: Option<&Cursor>,
    ) -> (
        Vec<Arc<SessionEventRecord>>,
        u64,
        broadcast::Receiver<Arc<SessionEventRecord>>,
    ) {
        let inner = self.log.lock().unwrap_or_else(|e| e.into_inner());
        let epoch_matches =
            cursor.is_none_or(|cursor| cursor.epoch.as_str() == self.epoch.as_ref());
        let replay = if epoch_matches {
            inner.replay_after(cursor)
        } else {
            Vec::new()
        };
        (replay, inner.tail_seq(), self.live.subscribe())
    }

    /// The current tail seq of the durable log — the seq of the newest
    /// durable event, or `0` when the log is empty.
    pub fn tail_seq(&self) -> u64 {
        self.log
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .tail_seq()
    }

    /// Advance the durable log's next seq by `skipped` without appending a
    /// record — a bus lag dropped events that were never journaled, so the
    /// seq sequence must jump to keep the loss detectable by clients tracking
    /// consecutive seqs.
    pub(crate) fn skip_seq(&self, skipped: u64) {
        let mut inner = self.log.lock().unwrap_or_else(|e| e.into_inner());
        inner.next_seq += skipped;
    }

    /// The epoch string attached to every record of this channel.
    pub fn epoch(&self) -> &str {
        self.epoch.as_ref()
    }
}

/// Registry of per-session event channels shared by every `/api/v1/rpc`
/// connection, so cursor replay survives individual connections (a client
/// that disconnects and resubscribes with its cursor resumes where it left
/// off as long as the session lives).
pub struct SessionEventChannelRegistry {
    channels: std::sync::Mutex<HashMap<String, Arc<SessionEventChannel>>>,
    capacity: usize,
}

impl SessionEventChannelRegistry {
    pub fn new(capacity: usize) -> Self {
        Self {
            channels: std::sync::Mutex::new(HashMap::new()),
            capacity,
        }
    }

    /// Get the live channel for `session_id`, creating it (and its log task)
    /// on first use. The channel is retained until the session's event bus
    /// closes (session torn down) or the session is `SessionClosed`, so the
    /// log keeps growing between subscriptions.
    pub(crate) fn get_or_create(
        self: &Arc<Self>,
        session_id: &str,
        bus: Arc<EventBus>,
        epoch: Arc<str>,
    ) -> Arc<SessionEventChannel> {
        let mut guard = self.channels.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(channel) = guard.get(session_id) {
            return Arc::clone(channel);
        }
        let channel = Arc::new(SessionEventChannel::new(epoch, self.capacity));
        let registry = Arc::clone(self);
        let task_channel = Arc::clone(&channel);
        let task_session_id = session_id.to_string();
        let mut bus_rx = bus.subscribe();
        tokio::spawn(async move {
            loop {
                match bus_rx.recv().await {
                    Ok(event) => {
                        let session_closed = matches!(event, SessionEvent::SessionClosed { .. });
                        task_channel.append(event);
                        if session_closed {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        // Missed bus events were never journaled: advance
                        // the durable seq so the sequence visibly jumps —
                        // a cursor below the hole resumes after it (the
                        // server still replays across the gap); it is the
                        // client's strict-seq-increase check that detects
                        // the loss.
                        tracing::warn!(
                            task_session_id,
                            skipped,
                            "session event bus lagged in RPC event log; advancing seq"
                        );
                        task_channel.skip_seq(skipped);
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            registry.remove_channel(&task_session_id, &task_channel);
        });
        guard.insert(session_id.to_string(), Arc::clone(&channel));
        channel
    }

    /// The live channel for `session_id`, if one exists.
    fn get(&self, session_id: &str) -> Option<Arc<SessionEventChannel>> {
        self.channels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session_id)
            .cloned()
    }

    /// Drop the channel when its log task ends — but only if it is still the
    /// registered one (a newer channel may have replaced it).
    fn remove_channel(&self, session_id: &str, channel: &Arc<SessionEventChannel>) {
        let mut guard = self.channels.lock().unwrap_or_else(|e| e.into_inner());
        if guard
            .get(session_id)
            .is_some_and(|existing| Arc::ptr_eq(existing, channel))
        {
            guard.remove(session_id);
        }
    }
}

/// Watch one session's event channel for a fire-mode turn's terminal event
/// and reap its in-flight entry.
///
/// Used by the engine fall-through in
/// [`ServerState::dispatch`](crate::state::ServerState): the caller
/// subscribed the channel in [`ServerState::prepare_fire_watch`](crate::state::ServerState::prepare_fire_watch)
/// BEFORE starting the engine call (so no terminal event can be missed) and
/// hands the already-positioned live receiver to this watcher together with
/// the `"{session_id}:{agent_id}:{turn_id}"` key registered in the shared
/// in-flight map. The watcher removes the key when the channel reports the
/// turn's `TurnCompleted` / `TurnFailed` (the two events that project to
/// `event.turn.ended`), or when the channel closes — so `session.status` /
/// `session.snapshot` keep their `running`/`busy` derivation for the whole
/// turn, exactly as the pre-thinning fire-mode handler did.
pub(crate) fn spawn_in_flight_watcher(
    mut live: broadcast::Receiver<Arc<SessionEventRecord>>,
    turn_id: String,
    key: String,
    in_flight: crate::rpc::InFlightTurns,
) {
    tokio::spawn(async move {
        loop {
            match live.recv().await {
                Ok(record) => {
                    let terminal = match &record.event {
                        SessionEvent::TurnCompleted { turn_id: id, .. }
                        | SessionEvent::TurnFailed { turn_id: id, .. } => id == &turn_id,
                        _ => false,
                    };
                    if terminal {
                        break;
                    }
                }
                // Missed records (lag) can only be terminal for a turn this
                // watcher no longer needs to track; keep waiting either way.
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                // The channel's log task ended (session closed): the turn is
                // gone too; the entry must not linger as "running".
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
        in_flight.lock().await.remove(&key);
    });
}

impl Default for SessionEventChannelRegistry {
    fn default() -> Self {
        Self::new(EVENT_LOG_CAPACITY)
    }
}

// ===========================================================================
// Connection state
// ===========================================================================

/// A frame queued for the connection's writer, or a control marker.
enum Outbound {
    /// A JSON-RPC frame ready to serialize.
    Frame(Value),
    /// The session's stream ended (its `SessionClosed` event was already
    /// queued); drop the subscription.
    SessionClosed(String),
}

/// A frame's serialized size — the number of bytes the writer will put on
/// the wire. Used by the outbound frame cap to bound what one queued frame
/// can pin in server memory.
fn frame_len(value: &Value) -> usize {
    serde_json::to_string(value).map(|text| text.len()).unwrap_or(usize::MAX)
}

/// Cursor position a client submits with `session.subscribe` (design §9).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Cursor {
    seq: u64,
    epoch: String,
}

/// Whether a subscribe with `cursor` skipped a replay the client should have
/// received: the cursor is behind the log's `tail_seq`, yet the replay leaves
/// a gap — either the ring buffer already evicted the events after the
/// cursor (`replay_front` is past `cursor.seq + 1`), or an epoch mismatch
/// suppressed the replay entirely (`replay_front` is `None` and no events
/// came back). A `seq: 0` cursor is a fresh-sync marker and never triggers
/// the signal, an absent cursor (fresh sync), a cursor already at or ahead
/// of the tail (nothing newer to miss), and a cursor behind the tail whose
/// replay starts exactly at `cursor.seq + 1` (contiguous, nothing evicted)
/// are all normal — only the missed-event gap needs the "client needs
/// resync" signal.
fn cursor_replay_bypassed(
    cursor: Option<&Cursor>,
    tail_seq: u64,
    replay_front: Option<u64>,
) -> bool {
    let Some(cursor) = cursor else {
        return false;
    };
    // A `seq: 0` cursor is a fresh-sync marker (the client deliberately
    // rebuilds from the retained window), not a missed-event claim.
    cursor.seq != 0
        && cursor.seq < tail_seq
        && replay_front.is_none_or(|front| front > cursor.seq + 1)
}

/// Reverse-RPC bookkeeping: the server's incrementing request ids, the
/// client's negotiated method-prefix whitelist, and the pending responses.
///
/// Requests are initiated by the engine through the [`WsReverseRpcCallback`]
/// bridge: the callback calls [`ReverseRpcState::send`] directly (minting
/// the id, registering the pending response, and enqueueing the request
/// frame onto the connection's outbox — delivered by the connection's
/// writer task) and awaits the client's response, which the connection's
/// inbound path resolves via [`ReverseRpcState::handle_response`]. The
/// state is shared (`Arc<Mutex<..>>`) between the connection loop and the
/// engine-side callback, so the reverse-RPC send path never needs the
/// connection loop — a wait-mode request (whose handler the loop awaits
/// inline) can still deliver `approval.ask` while the loop is busy with the
/// turn.
struct ReverseRpcState {
    /// Next id to mint for a server-initiated request (increments from 1).
    next_id: i64,
    /// Method prefixes the client declared in `client_hello` under
    /// `capabilities.reverseRpc` (e.g. `["approval", "question", "fs"]`).
    /// The server must not send reverse RPC outside this whitelist (§8).
    allowlist: Vec<String>,
    /// Pending reverse requests: id → response deliverer. Entries are
    /// removed when the client answers or the connection dies.
    pending: HashMap<i64, oneshot::Sender<JsonRpcResponse>>,
    /// Set when the owning connection tears down: the arc kept by callbacks
    /// stored on the session artifacts may outlive the connection, and a
    /// post-teardown send must fail fast (the historical channel-closed
    /// semantics) instead of queueing onto an undrained outbox.
    revoked: bool,
}

impl ReverseRpcState {
    fn new(allowlist: Vec<String>) -> Self {
        Self {
            next_id: 1,
            allowlist,
            pending: HashMap::new(),
            revoked: false,
        }
    }

    /// Whether the client may be asked to answer `method` (its domain prefix
    /// must appear in the negotiated whitelist).
    fn negotiated(&self, method: &str) -> bool {
        let Some(domain) = method.split('.').next() else {
            return false;
        };
        self.allowlist.iter().any(|allowed| allowed == domain)
    }

    /// Send a reverse RPC request: mint an id, register the pending
    /// response, and enqueue the request frame. Returns the id and the
    /// receiver that the caller awaits for the client's response.
    ///
    /// All effects happen under the shared state's lock; the caller holds no
    /// lock across an await. On an outbox failure the pending entry is
    /// withdrawn before the error is returned, so a failed send never leaks
    /// a pending response.
    fn send(
        &mut self,
        outbox: &mpsc::Sender<Outbound>,
        method: &str,
        params: Value,
    ) -> Result<(i64, oneshot::Receiver<JsonRpcResponse>), String> {
        if self.revoked {
            return Err(format!("{}connection closed", CHANNEL_ERROR_PREFIX));
        }
        if !self.negotiated(method) {
            return Err(format!(
                "reverse RPC '{method}' was not negotiated by the client (allowed: {:?})",
                self.allowlist
            ));
        }
        let id = self.next_id;
        self.next_id += 1;
        let (responder, receiver) = oneshot::channel();
        self.pending.insert(id, responder);
        let frame = JsonRpcFrame::Request(JsonRpcRequest::new(Some(id), method, params)).to_value();
        if outbox.try_send(Outbound::Frame(frame)).is_err() {
            self.pending.remove(&id);
            return Err(format!("{}outbox closed", CHANNEL_ERROR_PREFIX));
        }
        Ok((id, receiver))
    }

    /// Resolve a client response by its id. Returns the answered id, or
    /// `None` when no reverse request with that id is pending (a contract
    /// violation worth logging, design §10.1 `-32025`). A response without
    /// an integer id is unreachable through the strict codec and treated as
    /// protocol noise.
    fn handle_response(&mut self, response: JsonRpcResponse) -> Option<i64> {
        let Some(RequestId::Number(id)) = response.id else {
            tracing::debug!("response without an integer id; dropping as protocol noise");
            return None;
        };
        match self.pending.remove(&id) {
            Some(responder) => {
                let _ = responder.send(response);
                Some(id)
            }
            None => {
                tracing::warn!(id, "response to unknown reverse RPC request");
                None
            }
        }
    }
}

/// Bridge between the engine's [`ReverseRpcCallback`] trait and the WS
/// connection's [`ReverseRpcState`]. The callback is cheap to clone (the
/// state is an `Arc`, the outbox a cloneable sender) and is stored in
/// `Arc<dyn ReverseRpcCallback>` on the session's artifacts.
///
/// The callback sends request frames straight onto the connection's outbox
/// (delivered by the connection's writer task) and awaits the client's
/// response directly — it does NOT round-trip through the connection's main
/// loop. That independence is what lets a wait-mode turn issue
/// `approval.ask` without deadlocking the connection: the loop awaits the
/// turn's handler inline, and the reverse-RPC send path never needs the
/// loop.
struct WsReverseRpcCallback {
    /// The connection's shared reverse-RPC state (minting, allowlist,
    /// pending responses). Touched only under the lock, never across an
    /// await.
    state: Arc<std::sync::Mutex<ReverseRpcState>>,
    /// The connection's outbox; the writer task drains it and delivers the
    /// frame to the client.
    outbox: mpsc::Sender<Outbound>,
}

impl WsReverseRpcCallback {
    fn new(state: Arc<std::sync::Mutex<ReverseRpcState>>, outbox: mpsc::Sender<Outbound>) -> Self {
        Self { state, outbox }
    }
}

#[async_trait::async_trait]
impl mirri_core::ReverseRpcCallback for WsReverseRpcCallback {
    async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        // Mint the id, register the pending response, and queue the request
        // frame — one atomic step under the state lock, released before the
        // await. The connection's inbound path resolves the pending one-shot
        // with the client's response.
        let receiver = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let (_id, receiver) = state.send(&self.outbox, method, params)?;
            receiver
        };
        match receiver.await {
            Ok(response) => {
                if let Some(error) = response.error {
                    Err(error.message)
                } else {
                    Ok(response.result.unwrap_or(Value::Null))
                }
            }
            // The responder was dropped: the connection is gone. A
            // channel-class failure (ADR-008 / D-1): the consumer may be
            // waiting for the transport to reinstall a fresh callback.
            Err(_) => Err(format!("{}connection closed", CHANNEL_ERROR_PREFIX)),
        }
    }
}

/// Per-connection state of one `/api/v1/rpc` connection.
struct RpcConnection {
    state: ServerState,
    /// Frames queued by tasks spawned on this connection's behalf (event
    /// forwarders, heartbeats) and by the connection's own handlers. The
    /// channel is drained by the dedicated writer task spawned in
    /// [`run_connection`], never by the connection loop itself — that is
    /// what keeps handler-internal enqueue loops (replay) drain-safe.
    outbox: mpsc::Sender<Outbound>,
    /// Live event forwarder per subscribed session.
    subscriptions: HashMap<String, JoinHandle<()>>,
    /// Reverse-RPC id minting, whitelist, and pending responses — shared
    /// with the [`WsReverseRpcCallback`] instances stamped onto subscribed
    /// sessions' artifacts, so the engine-side callback can send requests
    /// (and the inbound path resolve responses) without this loop.
    reverse_rpc: Arc<std::sync::Mutex<ReverseRpcState>>,
    /// Live terminal-stream attachments, keyed by
    /// `(session_id, terminal_id)`. Each entry's sink forwards the
    /// terminal's [`TerminalFrame`]s to this connection as
    /// `terminal_output` / `terminal_exit` notifications; the sink is
    /// detached when the client detaches/closes the terminal or the
    /// connection ends (see [`RpcConnection::cleanup_terminal_attachments`]).
    terminal_attachments: HashMap<(String, String), SinkHandle>,
    /// The app-level MCP pool event forwarder (M7, F-9, `mcp.subscribe`):
    /// one task per connection relaying the engine's pool broadcast onto
    /// this connection's outbox as `event.mcp.server.status` notifications.
    /// Cancelled when the connection ends (the outbox closure also ends
    /// it — the same cleanup the per-session forwarders rely on).
    mcp_subscription: Option<JoinHandle<()>>,
    /// The app-level skill-catalog event forwarder (ADR-010 Task 6,
    /// `skill.subscribe`): one task per connection relaying the engine's
    /// skill-catalog broadcast onto this connection's outbox as
    /// `skill.catalogChanged` notifications — the `mcp_subscription`
    /// precedent shape.
    skill_catalog_subscription: Option<JoinHandle<()>>,
    /// Per-session cooldown bookkeeping for `resync_required` signaling: the
    /// wall-clock instant each session's last resync signal was sent. A
    /// client stuck in a lag→resync→re-subscribe loop (a slow renderer
    /// falling behind, then re-subscribing with the same stale cursor)
    /// would otherwise re-arm the replay push on every retry and feed the
    /// loop; within [`RESYNC_COOLDOWN`] the lagging re-subscribe still gets
    /// the resync signal (it must know to rebuild from a snapshot) but no
    /// event replay, breaking the amplification.
    resync_cooldowns: HashMap<String, std::time::Instant>,
}

impl RpcConnection {
    /// Queue one frame for the writer. Returns `false` when the writer is
    /// gone (the connection is being torn down).
    async fn enqueue_frame(&self, value: Value) -> bool {
        self.outbox.send(Outbound::Frame(value)).await.is_ok()
    }

    /// Respond to a well-parsed request.
    async fn respond(&mut self, id: i64, result: Result<Value, JsonRpcError>) {
        let frame = match result {
            Ok(result) => JsonRpcFrame::Response(JsonRpcResponse::ok(id, result)).to_value(),
            Err(error) => JsonRpcFrame::Response(JsonRpcResponse::err(id, error)).to_value(),
        };
        let _ = self.enqueue_frame(frame).await;
    }

    /// Route one inbound text frame.
    async fn handle_incoming_frame(&mut self, text: &str) {
        match JsonRpcFrame::parse(text) {
            Ok(JsonRpcFrame::Request(request)) => self.handle_request(request).await,
            Ok(JsonRpcFrame::Notification(notification)) => {
                self.handle_notification(notification).await
            }
            Ok(JsonRpcFrame::Response(response)) => {
                // Resolve the pending reverse-RPC one-shot. The shared state
                // is also touched by the engine-side callback (send); both
                // accesses are short, synchronous lock sections.
                self.reverse_rpc
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .handle_response(response);
            }
            Err(error) => {
                // Parse errors are always answered (id null); structurally
                // invalid frames are answered when they carried an id and
                // dropped silently otherwise — id-less junk is
                // notification-shaped (design §4.3).
                if error.respond {
                    self.enqueue_frame(protocol_error_frame(error.code, error.message, error.id))
                        .await;
                } else {
                    tracing::debug!(message = %error.message, "dropping invalid id-less frame");
                }
            }
        }
    }

    async fn handle_notification(&mut self, notification: JsonRpcNotification) {
        match notification.method.as_str() {
            "ping" => {
                // Client-initiated ping: answer with a pong echoing its nonce.
                let nonce = notification
                    .params
                    .get("nonce")
                    .cloned()
                    .unwrap_or(Value::Null);
                let pong = JsonRpcFrame::Notification(JsonRpcNotification::new(
                    "pong",
                    json!({ "nonce": nonce }),
                ))
                .to_value();
                let _ = self.enqueue_frame(pong).await;
            }
            "pong" => {
                tracing::debug!(params = ?notification.params, "client pong received");
            }
            other => {
                tracing::debug!(method = other, "ignoring unknown client notification");
            }
        }
    }

    async fn handle_request(&mut self, request: JsonRpcRequest) {
        // The strict codec admits integer ids only (string ids are rejected
        // at parse time), so a well-formed request carries a `Number`; the
        // string branch is defensive-unreachable.
        let Some(RequestId::Number(id)) = request.id else {
            // The codec classifies id-less frames as notifications; this
            // branch is defensive only.
            tracing::debug!(method = %request.method, "id-less request treated as notification");
            return;
        };
        match request.method.as_str() {
            "client_hello" => {
                tracing::warn!("duplicate client_hello ignored");
                self.respond(
                    id,
                    Err(JsonRpcError::new(INVALID_REQUEST, "duplicate client_hello")),
                )
                .await;
            }
            "ping" => {
                let nonce = request.params.get("nonce").cloned().unwrap_or(Value::Null);
                let pong = JsonRpcFrame::Notification(JsonRpcNotification::new(
                    "pong",
                    json!({ "nonce": nonce }),
                ))
                .to_value();
                let _ = self.enqueue_frame(pong).await;
            }
            "session.subscribe" => match parse_subscribe_params(&request.params) {
                Ok((session_id, cursor)) => self.subscribe_session(id, &session_id, cursor).await,
                Err(error) => self.respond(id, Err(error)).await,
            },
            // The app-level MCP pool event subscription (M7, F-9) —
            // connection-local like session.subscribe (it addresses this
            // connection's outbox, not the shared RPC surface).
            "mcp.subscribe" => self.subscribe_mcp(id).await,
            // The app-level skill-catalog change event subscription
            // (ADR-010 Task 6) — connection-local the same way.
            "skill.subscribe" => self.subscribe_skill_catalog(id).await,
            "session.unsubscribe" => {
                let outcome = match request.params.get("sessionId").and_then(Value::as_str) {
                    Some(session_id) => self.unsubscribe_session(session_id).await,
                    None => Err(JsonRpcError::new(
                        INVALID_PARAMS,
                        "session.unsubscribe requires a string 'sessionId'",
                    )),
                };
                self.respond(id, outcome).await;
            }
            // The web terminal panel's control frames — connection-local
            // like session.subscribe (they address per-connection attach
            // state, not the shared RPC registry). The frame names and
            // snake_case payload fields mirror the v1 multiplexed endpoint's
            // `terminal_*` frames (the web client's `ws.ts`), so the two
            // transports stay shape-compatible.
            "terminal_attach" => {
                self.terminal_attach(id, &request.params).await;
            }
            "terminal_input" => {
                let outcome = self.terminal_input(&request.params).await;
                self.respond(id, outcome).await;
            }
            "terminal_resize" => {
                let outcome = self.terminal_resize(&request.params).await;
                self.respond(id, outcome).await;
            }
            "terminal_detach" => {
                let outcome = self.terminal_detach(&request.params).await;
                self.respond(id, outcome).await;
            }
            "terminal_close" => {
                let outcome = self.terminal_close(&request.params).await;
                self.respond(id, outcome).await;
            }
            other => {
                // Engine-domain methods (e.g. wait-mode `message.send`) can
                // block for the whole turn — and a turn may issue reverse
                // RPC (`approval.ask` / `question.ask`) whose client answer
                // must be read while the turn is still in flight. The
                // dispatch therefore runs on a spawned task so this loop
                // keeps reading inbound frames (it is the socket's only
                // reader); the response is queued through the outbox, which
                // the writer task drains — a spawned handler never touches
                // connection state directly. Connection-local methods
                // (subscribe, terminal frames) stay inline and serialized.
                let response_id = id;
                let method = other.to_string();
                let params = request.params;
                let state = self.state.clone();
                let outbox = self.outbox.clone();
                tokio::spawn(async move {
                    let result = dispatch_registry(&state, &method, params).await;
                    if let Err(error) = &result {
                        tracing::error!(
                            id = response_id,
                            method = &method,
                            error = %error.message,
                            "rpc error"
                        );
                    }
                    let frame = match result {
                        Ok(result) => {
                            JsonRpcFrame::Response(JsonRpcResponse::ok(response_id, result))
                                .to_value()
                        }
                        Err(error) => {
                            JsonRpcFrame::Response(JsonRpcResponse::err(response_id, error))
                                .to_value()
                        }
                    };
                    // The engine-domain methods carry the big read faces
                    // (snapshots, histories, task payloads); the outbound
                    // frame cap applies here so one runaway response cannot
                    // pin the connection's outbox memory while a slow
                    // client drains.
                    let frame = if frame_len(&frame) > MAX_OUTBOUND_FRAME_BYTES {
                        let frame_bytes = frame_len(&frame);
                        tracing::error!(
                            id = response_id,
                            method = &method,
                            frame_bytes,
                            "response exceeds the outbound frame cap; refusing to queue it"
                        );
                        JsonRpcFrame::Response(JsonRpcResponse::err(
                            response_id,
                            JsonRpcError::new(
                                -32010,
                                format!(
                                    "response too large: {frame_bytes} bytes exceeds the {} byte frame cap",
                                    MAX_OUTBOUND_FRAME_BYTES
                                ),
                            ),
                        ))
                        .to_value()
                    } else {
                        frame
                    };
                    let _ = outbox.send(Outbound::Frame(frame)).await;
                });
            }
        }
    }

    /// Subscribe this connection to one session's event stream (design §6.0
    /// `session.subscribe`).
    ///
    /// Wire order follows the respond-before-publish rule: the
    /// acknowledgment (`{seq, epoch}` — the log's current tail) is queued
    /// first, then any cursor replay, then the live forwarder is spawned so
    /// its buffered live records cannot overtake the replayed ones. A
    /// repeated subscribe without a cursor re-acknowledges idempotently (the
    /// live forwarder already covers new events). A repeated subscribe that
    /// carries a cursor is treated as a resync: the stale forwarder is
    /// aborted, the gap check runs, and the full ack → (resync_required) →
    /// replay → live pipeline re-establishes the subscription — a client
    /// that dropped events needs the replay or the resync signal, not a
    /// silent re-ack.
    async fn subscribe_session(&mut self, id: i64, session_id: &str, cursor: Option<Cursor>) {
        if self.subscriptions.contains_key(session_id) && cursor.is_none() {
            tracing::debug!(
                session_id,
                "subscribe for already-subscribed session; re-acknowledging"
            );
            let ack = self.subscribe_ack(session_id);
            self.respond(id, ack).await;
            return;
        }
        // A cursor-carrying re-subscribe replaces this connection's stale
        // live forwarder (the new subscribe spawns its own below).
        if self.subscriptions.contains_key(session_id)
            && let Some(forwarder) = self.subscriptions.remove(session_id)
        {
            forwarder.abort();
            tracing::debug!(
                session_id,
                "cursor resubscribe replaced the stale live forwarder"
            );
        }
        // Route through the seam (W3): the addressed session's artifacts come
        // from its owning workspace's manager, and a persisted-only session
        // (a restart left it on disk) is resumed on demand — the subscribe's
        // reverse-RPC callback and live forwarder attach to the live session.
        let mut artifacts = self.state.session_routing.session_artifacts(session_id);
        if artifacts.is_none() && !self.state.session_routing.is_closed(session_id) {
            self.state.session_routing.restore_session(session_id).await;
            artifacts = self.state.session_routing.session_artifacts(session_id);
        }
        let Some(artifacts) = artifacts else {
            self.respond(
                id,
                Err(JsonRpcError::new(
                    -32000,
                    format!("session not found: {session_id}"),
                )),
            )
            .await;
            return;
        };
        // Set this connection's reverse RPC callback on the session artifacts
        // so the engine can issue approval.ask / question.ask through this WS
        // connection. The last subscriber wins: a single client is assumed
        // per session for interactive reverse RPC.
        let callback = Arc::new(WsReverseRpcCallback::new(
            Arc::clone(&self.reverse_rpc),
            self.outbox.clone(),
        )) as Arc<dyn mirri_core::ReverseRpcCallback>;
        *artifacts
            .reverse_rpc_callback
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Some(callback);
        let channel = self.state.session_event_channels.get_or_create(
            session_id,
            Arc::clone(&artifacts.event_bus),
            self.state.server_id.clone().into(),
        );
        let (replay, tail_seq, live_rx) = channel.subscribe(cursor.as_ref());
        let replay_front = replay.first().map(|record| record.seq);
        let epoch_mismatch = cursor
            .as_ref()
            .is_some_and(|cursor| cursor.epoch != channel.epoch.as_ref());
        let needs_resync =
            epoch_mismatch || cursor_replay_bypassed(cursor.as_ref(), tail_seq, replay_front);
        // Cooldown: a resync signal already fired for this session within
        // RESYNC_COOLDOWN means a lagging client is re-subscribing in a
        // tight loop — keep signaling (the client must rebuild from a
        // snapshot) but suppress the replay, so the loop cannot amplify
        // the backlog it is trying to escape.
        let resync_in_cooldown = needs_resync
            && self
                .resync_cooldowns
                .get(session_id)
                .is_some_and(|last| last.elapsed() < RESYNC_COOLDOWN);
        if needs_resync {
            let reason = if epoch_mismatch {
                "epoch_changed"
            } else {
                "buffer_overflow"
            };
            tracing::warn!(
                session_id,
                reason,
                in_cooldown = resync_in_cooldown,
                "subscribe cursor bypassed replay; sending resync_required"
            );
        } else {
            tracing::debug!(
                session_id,
                "subscribe cursor at or ahead of tail; no replay expected"
            );
        }

        // 1. Ack, 2. resync_required when the cursor cannot be caught up,
        // 3. replay, 4. live — the forwarder is spawned last so live records
        // cannot overtake replayed ones in the outbox. The resync signal
        // precedes the replay so a client that receives one knows the
        // replayed window (if any) is incomplete and rebuilds from a
        // snapshot instead of silently skipping the missed events.
        let ack = json!({ "seq": tail_seq, "epoch": channel.epoch.as_ref() });
        let ack_frame = JsonRpcFrame::Response(JsonRpcResponse::ok(id, ack)).to_value();
        if !self.enqueue_frame(ack_frame).await {
            return;
        }
        if needs_resync {
            let frame = resync_required_frame(
                session_id,
                if epoch_mismatch {
                    "epoch_changed"
                } else {
                    "buffer_overflow"
                },
                tail_seq,
                channel.epoch.as_ref(),
            );
            if !self.enqueue_frame(frame).await {
                return;
            }
            self.resync_cooldowns
                .insert(session_id.to_string(), std::time::Instant::now());
        }
        // A lagging re-subscribe inside the cooldown skips the replay: the
        // client already holds the resync signal and must rebuild from a
        // snapshot; re-pushing the (incomplete) replay window feeds the
        // loop instead of resolving it.
        if !resync_in_cooldown {
            for record in &replay {
                if !self.enqueue_frame(event_frame(session_id, record)).await {
                    return;
                }
            }
        }
        // ADR-011 e (swarm T5): replay every pending approval as an
        // `approval.ask` NOTIFICATION (id-less) — the gate read face is the
        // source of truth for what is pending, and the replay is a pure
        // notify face: a client that wants to decide must go through
        // `approval.decide` / the interaction read surface (an id-less
        // notification is not answerable, so a revived ask can never race
        // the gate). This lets a reconnected client rebuild the approval
        // cards without depending on the pre-disconnect notification
        // frames surviving. Payload shape mirrors the engine's original
        // ask (`tool_name` / `tool_call_id` / `action` / `display` /
        // `reason` / `request_id`, plus the optional `approval_rule` /
        // `agent_id` facets) so existing ask consumers parse it unchanged.
        for (request_id, snapshot) in artifacts.approval_gate.list_pending() {
            let mut params = serde_json::Map::new();
            params.insert("request_id".into(), json!(request_id.to_string()));
            params.insert("tool_name".into(), json!(snapshot.tool_name));
            params.insert("action".into(), json!(snapshot.action));
            params.insert("display".into(), snapshot.display);
            params.insert("reason".into(), json!("approval required"));
            if let Some(tool_call_id) = snapshot.tool_call_id {
                params.insert("tool_call_id".into(), json!(tool_call_id));
            }
            // The requesting member's full agent id (supervision B1) — the
            // snapshot's uuid is the `agent-<uuid>` namespace the host
            // mints, nil when unattributed (never emit a fabricated id).
            if snapshot.agent_id != Uuid::nil() {
                params.insert(
                    "agent_id".into(),
                    json!(format!("agent-{}", snapshot.agent_id)),
                );
            }
            // The engine approval-rule facet (B9) — emitted only when the
            // snapshot carries one (absent ≠ empty).
            if let Some(approval_rule) = snapshot.approval_rule {
                params.insert("approval_rule".into(), json!(approval_rule));
            }
            let frame = JsonRpcFrame::Notification(JsonRpcNotification::new(
                "approval.ask",
                Value::Object(params),
            ))
            .to_value();
            if !self.enqueue_frame(frame).await {
                return;
            }
        }
        let forwarder = spawn_event_forwarder(
            self.outbox.clone(),
            session_id.to_string(),
            Arc::clone(&channel),
            live_rx,
        );
        self.subscriptions.insert(session_id.to_string(), forwarder);
        tracing::info!(session_id, "session subscribed (json-rpc)");
    }

    /// Acknowledge a re-subscribe with the channel's current tail position.
    fn subscribe_ack(&self, session_id: &str) -> Result<Value, JsonRpcError> {
        #[derive(serde::Serialize)]
        struct SubscribeAck {
            seq: u64,
            epoch: String,
        }
        match self.state.session_event_channels.get(session_id) {
            Some(channel) => {
                let (_, tail_seq, _) = channel.subscribe(None);
                serde_json::to_value(SubscribeAck {
                    seq: tail_seq,
                    epoch: channel.epoch().to_string(),
                })
                .map_err(|e| JsonRpcError::new(-32000, e.to_string()))
            }
            None => Err(JsonRpcError::new(
                -32000,
                format!("session not found: {session_id}"),
            )),
        }
    }

    /// Stop relaying one session's events (idempotent, design §6.0
    /// `session.unsubscribe`).
    async fn unsubscribe_session(&mut self, session_id: &str) -> Result<Value, JsonRpcError> {
        match self.subscriptions.remove(session_id) {
            Some(forwarder) => {
                forwarder.abort();
                tracing::debug!(session_id, "session unsubscribed (json-rpc)");
            }
            None => {
                tracing::debug!(
                    session_id,
                    "unsubscribe for not-subscribed session; acknowledging"
                );
            }
        }
        #[derive(serde::Serialize)]
        struct UnsubscribeAck {
            ok: bool,
        }
        serde_json::to_value(UnsubscribeAck { ok: true })
            .map_err(|e| JsonRpcError::new(-32000, e.to_string()))
    }

    /// Subscribe this connection to the app-level MCP pool event broadcast
    /// (M7, F-9, `mcp.subscribe`) — the connection-local counterpart of
    /// [`Self::subscribe_session`]: no session is involved (the pool
    /// surface belongs to the app), and the events are connection-scoped.
    ///
    /// Wire order follows the respond-before-publish rule: the
    /// acknowledgment goes out first, then the live forwarder is spawned so
    /// no buffered pool event can overtake it. A repeated subscribe
    /// re-acknowledges without spawning a second forwarder (idempotent);
    /// the forwarder ends with the connection (its outbox send fails once
    /// the writer is gone, and the connection teardown aborts it
    /// explicitly — the same cleanup the per-session forwarders rely on).
    async fn subscribe_mcp(&mut self, id: i64) {
        #[derive(serde::Serialize)]
        struct SubscribeMcpAck {
            ok: bool,
        }
        let ack = serde_json::to_value(SubscribeMcpAck { ok: true })
            .unwrap_or_else(|_| serde_json::json!({ "ok": true }));
        let ack_frame = JsonRpcFrame::Response(JsonRpcResponse::ok(id, ack)).to_value();
        if !self.enqueue_frame(ack_frame).await {
            return;
        }
        if self.mcp_subscription.is_some() {
            tracing::debug!("mcp.subscribe for an already-subscribed connection; re-acknowledging");
            return;
        }
        let mut live_rx = self.state.mcp_event_rx();
        let outbox = self.outbox.clone();
        let forwarder = tokio::spawn(async move {
            loop {
                match live_rx.recv().await {
                    Ok(notification) => {
                        let frame =
                            Outbound::Frame(JsonRpcFrame::Notification(notification).to_value());
                        if outbox.send(frame).await.is_err() {
                            break; // the connection's writer is gone
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(skipped, "MCP pool event stream lagged; dropped events");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        self.mcp_subscription = Some(forwarder);
        tracing::info!("app-level MCP events subscribed (json-rpc)");
    }

    /// Subscribe this connection to the app-level skill-catalog change
    /// broadcast (ADR-010 Task 6, `skill.subscribe`) — the
    /// [`Self::subscribe_mcp`] precedent: the acknowledgment goes out
    /// first (the respond-before-publish rule), then the live forwarder
    /// is spawned; a repeated subscribe re-acknowledges without spawning
    /// a second forwarder (idempotent); the forwarder ends with the
    /// connection.
    async fn subscribe_skill_catalog(&mut self, id: i64) {
        #[derive(serde::Serialize)]
        struct SubscribeSkillCatalogAck {
            ok: bool,
        }
        let ack =
            serde_json::to_value(SubscribeSkillCatalogAck { ok: true }).unwrap_or_else(|_| {
                serde_json::json!({ "ok": true })
            });
        let ack_frame = JsonRpcFrame::Response(JsonRpcResponse::ok(id, ack)).to_value();
        if !self.enqueue_frame(ack_frame).await {
            return;
        }
        if self.skill_catalog_subscription.is_some() {
            tracing::debug!(
                "skill.subscribe for an already-subscribed connection; re-acknowledging"
            );
            return;
        }
        let mut live_rx = self.state.skill_catalog_event_rx();
        let outbox = self.outbox.clone();
        let forwarder = tokio::spawn(async move {
            loop {
                match live_rx.recv().await {
                    Ok(notification) => {
                        let frame = Outbound::Frame(
                            JsonRpcFrame::Notification(notification).to_value(),
                        );
                        if outbox.send(frame).await.is_err() {
                            break; // the connection's writer is gone
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(
                            skipped,
                            "skill catalog event stream lagged; dropped events"
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        self.skill_catalog_subscription = Some(forwarder);
        tracing::info!("app-level skill catalog events subscribed (json-rpc)");
    }

    // -----------------------------------------------------------------------
    // Terminal stream bridge (web terminal panel)
    // -----------------------------------------------------------------------

    /// `terminal_attach` — replay the terminal's buffered output since
    /// `since_seq`, then stream its live frames to this connection.
    ///
    /// The replay snapshot and the sink installation happen in **one atomic
    /// step** ([`TerminalPool::attach_sink_with_replay`]): a frame emitted
    /// concurrently is either in the replay or delivered to the sink, never
    /// lost and never duplicated (F-P3-1).
    ///
    /// The wire order mirrors `session.subscribe`: the acknowledgment is
    /// queued first, then the replayed `terminal_output` notifications, then
    /// the live sink streams. A gate inside the sink holds any frame that
    /// arrives before the replay has been queued, so a live frame can never
    /// overtake the replay in the outbox; the gate opens (and the held
    /// frames flush, in order) once the replay is fully queued. A repeated
    /// attach for the same terminal detaches the previous sink first, so a
    /// client reconnecting without an explicit detach does not double-stream.
    async fn terminal_attach(&mut self, id: i64, params: &Value) {
        let (session_id, terminal_id, since_seq) = match parse_terminal_attach_params(params) {
            Ok(parsed) => parsed,
            Err(error) => {
                self.respond(id, Err(error)).await;
                return;
            }
        };
        let pool = match self.terminal_pool(&session_id).await {
            Ok(pool) => pool,
            Err(error) => {
                self.respond(id, Err(error)).await;
                return;
            }
        };
        // The sink gate: frames the sink receives before the replay is
        // queued are held, and the flush below runs under the same mutex —
        // so the outbox sees replay frames first, then held frames, then
        // live frames, in that order.
        let gate = Arc::new(std::sync::Mutex::new(SinkGate::default()));
        let sink = {
            let gate = Arc::clone(&gate);
            let outbox = self.outbox.clone();
            let sink_session_id = session_id.clone();
            let sink_terminal_id = terminal_id.clone();
            Box::new(move |frame: TerminalFrame| {
                let mut gate = gate.lock().unwrap_or_else(|e| e.into_inner());
                if gate.open {
                    forward_terminal_frame(&outbox, &sink_session_id, &sink_terminal_id, &frame);
                } else {
                    gate.held.push(frame);
                }
            })
        };
        // 1. Atomic replay snapshot + sink installation. A failure here
        // happens before anything is queued, so the client still gets a
        // clean error response.
        let (handle, replay) = match pool
            .attach_sink_with_replay(&terminal_id, since_seq.unwrap_or(0), sink)
            .await
        {
            Ok(attached) => attached,
            Err(error) => {
                self.respond(id, Err(terminal_error(error))).await;
                return;
            }
        };
        // 2. Ack, 3. replay — the same ordering contract `session.subscribe`
        // keeps for its cursor replay.
        let ack_frame =
            JsonRpcFrame::Response(JsonRpcResponse::ok(id, json!({ "ok": true }))).to_value();
        if !self.enqueue_frame(ack_frame).await {
            // The connection is gone: nothing more can be queued, so the
            // sink must go too.
            let _ = pool.detach_sink(&terminal_id, handle).await;
            return;
        }
        for frame in &replay {
            if let TerminalFrame::Output { seq, data, .. } = frame {
                let notification =
                    terminal_output_notification(&session_id, &terminal_id, *seq, data);
                if !self.enqueue_frame(notification).await {
                    let _ = pool.detach_sink(&terminal_id, handle).await;
                    return;
                }
            }
        }
        // 4. Open the gate: live frames stream from here, and any frame the
        // sink held while the replay was queued flushes behind it. The
        // mutex makes the open + flush one atomic step against the sink.
        {
            let mut gate = gate.lock().unwrap_or_else(|e| e.into_inner());
            gate.open = true;
            let outbox = self.outbox.clone();
            for frame in gate.held.drain(..) {
                forward_terminal_frame(&outbox, &session_id, &terminal_id, &frame);
            }
        }
        // A previous attach for the same terminal is replaced: the new sink
        // is installed before the old one detaches, so an install failure
        // can never leave the terminal unstreamed.
        if let Some(previous) = self
            .terminal_attachments
            .insert((session_id.clone(), terminal_id.clone()), handle)
        {
            let _ = pool.detach_sink(&terminal_id, previous).await;
        }
    }

    /// `terminal_input` — write one chunk to the terminal's stdin.
    async fn terminal_input(&mut self, params: &Value) -> Result<Value, JsonRpcError> {
        let (session_id, terminal_id) = parse_terminal_id_params(params)?;
        let data = params
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                JsonRpcError::new(INVALID_PARAMS, "terminal_input requires a string 'data'")
            })?
            .to_string();
        let pool = self.terminal_pool(&session_id).await?;
        pool.write(&terminal_id, &data)
            .await
            .map_err(terminal_error)?;
        terminal_ok()
    }

    /// `terminal_resize` — resize the terminal's PTY.
    async fn terminal_resize(&mut self, params: &Value) -> Result<Value, JsonRpcError> {
        let (session_id, terminal_id) = parse_terminal_id_params(params)?;
        let cols = params.get("cols").and_then(Value::as_u64).ok_or_else(|| {
            JsonRpcError::new(INVALID_PARAMS, "terminal_resize requires an integer 'cols'")
        })? as u16;
        let rows = params.get("rows").and_then(Value::as_u64).ok_or_else(|| {
            JsonRpcError::new(INVALID_PARAMS, "terminal_resize requires an integer 'rows'")
        })? as u16;
        let pool = self.terminal_pool(&session_id).await?;
        pool.resize(&terminal_id, cols, rows)
            .await
            .map_err(terminal_error)?;
        terminal_ok()
    }

    /// `terminal_detach` — stop streaming a terminal (the shell keeps
    /// running; a later `terminal_attach` with the last seen seq resumes
    /// without gaps).
    async fn terminal_detach(&mut self, params: &Value) -> Result<Value, JsonRpcError> {
        let (session_id, terminal_id) = parse_terminal_id_params(params)?;
        let pool = self.terminal_pool(&session_id).await?;
        self.detach_terminal(&pool, &session_id, &terminal_id).await;
        terminal_ok()
    }

    /// `terminal_close` — kill the terminal's shell.
    ///
    /// The sink stays attached across the kill so the client observes the
    /// `terminal_exit` notification (detaching first would silence the
    /// exit); the stale sink is harmless afterwards (the terminal's event
    /// loop has ended) and is reaped by the connection teardown or a later
    /// re-attach.
    async fn terminal_close(&mut self, params: &Value) -> Result<Value, JsonRpcError> {
        let (session_id, terminal_id) = parse_terminal_id_params(params)?;
        let pool = self.terminal_pool(&session_id).await?;
        // Kill (not remove): the record stays in the pool as `exited`, so
        // the REST surface keeps reporting it until the session closes.
        pool.kill(&terminal_id).await.map_err(terminal_error)?;
        terminal_ok()
    }

    /// Resolve the session's terminal pool, answering a session-not-found
    /// error the same way `session.subscribe` does — through the routing
    /// seam (W3) with restore-on-demand, so a non-default-workspace
    /// session's pool resolves on its own manager.
    async fn terminal_pool(&self, session_id: &str) -> Result<Arc<TerminalPool>, JsonRpcError> {
        crate::rpc::live_or_restored_session(self.state.session_routing.as_ref(), session_id)
            .await
            .map(|session| session.terminal_pool())
            .ok_or_else(|| JsonRpcError::new(-32000, format!("session not found: {session_id}")))
    }

    /// Remove this connection's sink for one terminal (idempotent).
    async fn detach_terminal(
        &mut self,
        pool: &Arc<TerminalPool>,
        session_id: &str,
        terminal_id: &str,
    ) {
        if let Some(handle) = self
            .terminal_attachments
            .remove(&(session_id.to_string(), terminal_id.to_string()))
        {
            let _ = pool.detach_sink(terminal_id, handle).await;
        }
    }

    /// Detach every live terminal sink when the connection ends — the
    /// sinks hold this connection's outbox and must not outlive it. The
    /// shells themselves keep running (they belong to the session, not
    /// the connection; `session.close` kills them).
    async fn cleanup_terminal_attachments(&mut self) {
        let attachments = std::mem::take(&mut self.terminal_attachments);
        for ((session_id, terminal_id), handle) in attachments {
            // Same routing seam as the attach that registered the sink —
            // a foreign-workspace session's pool must resolve here too,
            // or the sink would leak until the session closes.
            let Ok(pool) = self.terminal_pool(&session_id).await else {
                continue;
            };
            let _ = pool.detach_sink(&terminal_id, handle).await;
        }
    }
}

/// Dispatch a forward RPC through the shared dispatch: the transport
/// registry first, then the engine fall-through gated on the
/// `mirri_proto::rpc` availability facts.
async fn dispatch_registry(
    state: &ServerState,
    method: &str,
    params: Value,
) -> Result<Value, JsonRpcError> {
    let params = if params.is_null() { json!({}) } else { params };
    state
        .dispatch(method, params)
        .await
        .map_err(|error| rpc_error_to_json_rpc(&error))
}

/// The connection's dedicated outbound writer: drain the outbox and write
/// every queued frame to the client, batching runs of consecutive
/// notifications into array frames (design §4.1.4).
///
/// Running this as its **own task** (rather than draining the outbox inside
/// the connection loop's `select!`) is what makes handler-internal enqueue
/// loops drain-safe (F-T9-1): `terminal_attach`'s scrollback replay and
/// `session.subscribe`'s cursor replay can enqueue more frames than the
/// outbox's capacity in one handler call. When that handler is awaited
/// inline by the inbound branch, a full outbox whose only consumer was the
/// very loop stuck awaiting the handler is a deadlock; with the writer
/// draining concurrently, every `enqueue_frame` completes as the writer
/// frees capacity. Wire order is preserved either way — all frames still
/// pass through the single FIFO outbox channel.
///
/// `SessionClosed` markers are not written to the client: the writer does
/// not own the connection's subscription map, so it reports them back to
/// the connection loop through `closed_tx`, which aborts the forwarder.
/// Returns when a socket write fails (the client is gone) or when every
/// outbox sender is dropped (the connection loop is tearing down).
async fn write_outbound(
    mut sink: SplitSink<WebSocket, Message>,
    mut outbox_rx: mpsc::Receiver<Outbound>,
    closed_tx: mpsc::Sender<String>,
) {
    while let Some(first) = outbox_rx.recv().await {
        let mut queue = Vec::with_capacity(8);
        queue.push(first);
        while queue.len() < MAX_BATCH_SIZE {
            match outbox_rx.try_recv() {
                Ok(outbound) => queue.push(outbound),
                Err(_) => break,
            }
        }

        let mut writes: Vec<Value> = Vec::new();
        let mut closed: Vec<String> = Vec::new();
        for outbound in queue {
            match outbound {
                Outbound::Frame(value) => writes.push(value),
                Outbound::SessionClosed(session_id) => closed.push(session_id),
            }
        }

        let mut index = 0;
        while index < writes.len() {
            if writes[index].get("id").is_some() {
                // Requests and responses are always their own message so the
                // client can correlate them by id.
                if !send_text(&mut sink, &writes[index].to_string()).await {
                    return;
                }
                index += 1;
            } else {
                let run_start = index;
                while index < writes.len() && writes[index].get("id").is_none() {
                    index += 1;
                }
                let run = &writes[run_start..index];
                let payload = if run.len() == 1 {
                    run[0].to_string()
                } else {
                    format!(
                        "[{}]",
                        run.iter()
                            .map(Value::to_string)
                            .collect::<Vec<_>>()
                            .join(",")
                    )
                };
                if !send_text(&mut sink, &payload).await {
                    return;
                }
            }
        }

        for session_id in closed {
            if closed_tx.send(session_id).await.is_err() {
                return; // the connection loop is gone
            }
        }
    }
}

/// Build the `event` notification for one logged record (design §7.0).
///
/// The frame's `method`, payload, and envelope (`sessionId`, and
/// `seq`/`epoch`/`volatile` when durable) all come from the shared core
/// projection ([`event_projection::event_notification`] — the single source
/// of truth for the `event.*` vocabulary across backends). Returns
/// [`Value::Null`] when the event has no wire projection
/// ([`SessionEvent::Unknown`]) — the forwarder drops null frames.
fn event_frame(session_id: &str, record: &SessionEventRecord) -> Value {
    match event_projection::event_notification(
        &record.event,
        session_id,
        Some(record.seq),
        Some(record.epoch.as_ref()),
        chrono::Utc::now(),
    ) {
        Some(notification) => JsonRpcFrame::Notification(JsonRpcNotification::new(
            notification.method,
            notification.params,
        ))
        .to_value(),
        // Events with no wire projection are silently dropped by the
        // forwarder, which skips null frames.
        None => Value::Null,
    }
}

/// Build a `resync_required` JSON-RPC notification (protocol §9.3): the
/// server cannot catch the client up from its cursor, so the client must
/// rebuild the session from `session.snapshot`. The payload carries both
/// the spec's `currentSeq` and a `seq` alias — the web client's direct-
/// event dispatch reads `seq` off the envelope for its cursor bookkeeping.
fn resync_required_frame(session_id: &str, reason: &str, current_seq: u64, epoch: &str) -> Value {
    JsonRpcFrame::Notification(JsonRpcNotification::new(
        "resync_required",
        json!({
            "sessionId": session_id,
            "reason": reason,
            "currentSeq": current_seq,
            "seq": current_seq,
            "epoch": epoch,
        }),
    ))
    .to_value()
}

/// Spawn the task relaying one session's live records into the connection's
/// outbox. Exits after the session's closing event or when the channel or
/// outbox closes.
fn spawn_event_forwarder(
    outbox: mpsc::Sender<Outbound>,
    session_id: String,
    channel: Arc<SessionEventChannel>,
    mut live_rx: broadcast::Receiver<Arc<SessionEventRecord>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            match live_rx.recv().await {
                Ok(record) => {
                    let session_closed = matches!(record.event, SessionEvent::SessionClosed { .. });
                    let frame_val = event_frame(&session_id, &record);
                    if frame_val.is_null() {
                        // Unknown event with no wire projection — skip it.
                        // (`SessionClosed` does project to
                        // `event.session.deleted`, so a null frame never
                        // needs the session-closed stop; kept defensively.)
                        if session_closed {
                            let _ = outbox
                                .send(Outbound::SessionClosed(session_id.clone()))
                                .await;
                            break;
                        }
                        continue;
                    }
                    let frame = Outbound::Frame(frame_val);
                    if outbox.send(frame).await.is_err() {
                        break; // the connection's writer is gone
                    }
                    if session_closed {
                        let _ = outbox
                            .send(Outbound::SessionClosed(session_id.clone()))
                            .await;
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    // The live fan-out dropped `skipped` records this
                    // subscriber can never recover on this connection:
                    // tell the client to rebuild from a snapshot (§9.3)
                    // instead of silently skipping.
                    tracing::warn!(
                        session_id,
                        skipped,
                        "session event stream lagged; dropped events; sending resync_required"
                    );
                    let frame = resync_required_frame(
                        &session_id,
                        "buffer_overflow",
                        channel.tail_seq(),
                        channel.epoch(),
                    );
                    if outbox.send(Outbound::Frame(frame)).await.is_err() {
                        break; // the connection's writer is gone
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

/// Map a registry error to the JSON-RPC error object (camelCase `data`
/// fields per design §10.2).
fn rpc_error_to_json_rpc(error: &RpcError) -> JsonRpcError {
    let data = error
        .error_id()
        .map(|error_id| json!({ "errorId": error_id }))
        .or_else(|| {
            error
                .domain_code()
                // The D5 domain-code payload — the same builder the embedded
                // engine uses (`{"domainCode": "..."}`).
                .map(mirri_proto::rpc::domain_error_data)
        });
    JsonRpcError {
        code: i64::from(error.json_rpc_code()),
        message: error.to_string(),
        data,
    }
}

/// Parse `session.subscribe` params: `{sessionId, cursor?}`.
fn parse_subscribe_params(params: &Value) -> Result<(String, Option<Cursor>), JsonRpcError> {
    let session_id = params
        .get("sessionId")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            JsonRpcError::new(
                INVALID_PARAMS,
                "session.subscribe requires a string 'sessionId'",
            )
        })?
        .to_string();
    let cursor = match params.get("cursor") {
        Some(value) => Some(parse_cursor(value)?),
        None => None,
    };
    Ok((session_id, cursor))
}

/// Parse a cursor `{seq, epoch}`. The epoch is a string on the wire (design
/// §6.0 `Cursor`); a numeric epoch is tolerated and stringified.
fn parse_cursor(value: &Value) -> Result<Cursor, JsonRpcError> {
    let object = value
        .as_object()
        .ok_or_else(|| JsonRpcError::new(INVALID_PARAMS, "cursor must be an object"))?;
    let seq = object
        .get("seq")
        .and_then(Value::as_u64)
        .ok_or_else(|| JsonRpcError::new(INVALID_PARAMS, "cursor requires an integer 'seq'"))?;
    let epoch = match object.get("epoch") {
        Some(Value::String(epoch)) => epoch.clone(),
        Some(Value::Number(epoch)) => epoch.to_string(),
        _ => {
            return Err(JsonRpcError::new(
                INVALID_PARAMS,
                "cursor requires a string 'epoch'",
            ));
        }
    };
    Ok(Cursor { seq, epoch })
}

// ===========================================================================
// Terminal stream bridge helpers
// ===========================================================================

/// Parse the common `{session_id, terminal_id}` pair the terminal control
/// frames carry.
fn parse_terminal_id_params(params: &Value) -> Result<(String, String), JsonRpcError> {
    let session_id = params
        .get("session_id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            JsonRpcError::new(
                INVALID_PARAMS,
                "terminal frames require a string 'session_id'",
            )
        })?
        .to_string();
    let terminal_id = params
        .get("terminal_id")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            JsonRpcError::new(
                INVALID_PARAMS,
                "terminal frames require a string 'terminal_id'",
            )
        })?
        .to_string();
    Ok((session_id, terminal_id))
}

/// Parse `terminal_attach`'s params: `{session_id, terminal_id, since_seq?}`.
fn parse_terminal_attach_params(
    params: &Value,
) -> Result<(String, String, Option<u64>), JsonRpcError> {
    let (session_id, terminal_id) = parse_terminal_id_params(params)?;
    let since_seq = match params.get("since_seq") {
        Some(value) => Some(value.as_u64().ok_or_else(|| {
            JsonRpcError::new(
                INVALID_PARAMS,
                "terminal_attach's 'since_seq' must be an integer",
            )
        })?),
        None => None,
    };
    Ok((session_id, terminal_id, since_seq))
}

/// Map a [`TerminalPool`](mirri_core::terminal_pool::TerminalPool) error
/// onto the wire (an unknown terminal or a closed PTY is a not-found-class
/// application error).
fn terminal_error(error: impl std::fmt::Display) -> JsonRpcError {
    JsonRpcError::new(-32000, error.to_string())
}

/// The `{ok: true}` acknowledgment the terminal control frames answer with.
fn terminal_ok() -> Result<Value, JsonRpcError> {
    Ok(json!({ "ok": true }))
}

/// Build one `terminal_output` notification frame (the v1 multiplexed
/// endpoint's shape: top-level `session_id` / `terminal_id` / `seq` with
/// the data riding `payload.data`).
fn terminal_output_notification(
    session_id: &str,
    terminal_id: &str,
    seq: u64,
    data: &str,
) -> Value {
    JsonRpcFrame::Notification(JsonRpcNotification::new(
        "terminal_output",
        json!({
            "session_id": session_id,
            "terminal_id": terminal_id,
            "seq": seq,
            "data": data,
        }),
    ))
    .to_value()
}

/// Build one `terminal_exit` notification frame (`payload.exit_code`).
fn terminal_exit_notification(
    session_id: &str,
    terminal_id: &str,
    exit_code: Option<i32>,
) -> Value {
    JsonRpcFrame::Notification(JsonRpcNotification::new(
        "terminal_exit",
        json!({
            "session_id": session_id,
            "terminal_id": terminal_id,
            "exit_code": exit_code,
        }),
    ))
    .to_value()
}

/// The attach-time sink gate: frames the sink receives while `open` is
/// false are held instead of forwarded, so a live frame can never overtake
/// the replayed scrollback in the connection's outbox. `terminal_attach`
/// flips `open` and drains `held` under the same mutex once the replay has
/// been fully queued — the mutex makes the open + flush one atomic step
/// against the sink's check-and-forward.
#[derive(Default)]
struct SinkGate {
    open: bool,
    held: Vec<TerminalFrame>,
}

/// Forward one live terminal frame to a connection's outbox — the sink
/// closure installed by `terminal_attach`. The session and terminal ids
/// are captured by the closure (the [`TerminalFrame`] itself carries only
/// the terminal id and its payload).
///
/// The sink is synchronous (the pool's fan-out cannot await), so the frame
/// is queued with `try_send`: a full outbox drops the frame (the client is
/// not keeping up with the terminal's output; the buffer's `since_seq`
/// replay covers a re-attach) and a closed outbox is a dead connection
/// (the attachment is cleaned up when the loop exits).
fn forward_terminal_frame(
    outbox: &mpsc::Sender<Outbound>,
    session_id: &str,
    terminal_id: &str,
    frame: &TerminalFrame,
) {
    let notification = match frame {
        TerminalFrame::Output { seq, data, .. } => {
            terminal_output_notification(session_id, terminal_id, *seq, data)
        }
        TerminalFrame::Exit { code, .. } => {
            terminal_exit_notification(session_id, terminal_id, *code)
        }
    };
    if outbox.try_send(Outbound::Frame(notification)).is_err() {
        tracing::debug!(
            session_id,
            terminal_id,
            "terminal frame dropped: outbox closed or full"
        );
    }
}

// ===========================================================================
// Connection lifecycle
// ===========================================================================

/// Drive one connection: handshake, then the frame loop until the client
/// disconnects or fails liveness.
///
/// All outbound frames flow through one bounded outbox channel that is
/// drained by a **dedicated writer task** ([`write_outbound`]) owning the
/// socket sink — not by this loop. This division of labor is the F-T9-1 fix:
/// handlers run inline in the inbound branch (`handle_incoming_frame` is
/// awaited by the `select!`), and the replay loops inside `terminal_attach` /
/// `session.subscribe` can enqueue more frames than the outbox capacity in a
/// single handler call. If this loop were also the outbox's only consumer,
/// such a handler would await a full channel that nothing drains — a
/// connection-wide deadlock (no ack, no frames, no teardown, leaked sinks).
/// With the writer draining concurrently, every enqueue completes as the
/// writer frees capacity, and backpressure degrades to flow control instead
/// of deadlock.
async fn run_connection(socket: WebSocket, state: ServerState) {
    let (mut sink, mut incoming) = socket.split();

    let Some((_hello_id, reverse_rpc_allowlist)) =
        send_server_hello_and_await_client_hello(&mut sink, &mut incoming, &state).await
    else {
        tracing::debug!("connection closed during handshake");
        return;
    };

    let (outbox_tx, outbox_rx) = mpsc::channel::<Outbound>(OUTBOX_CAPACITY);
    // Session-closed notices from the writer: it encounters the
    // `SessionClosed` markers while draining but cannot touch this loop's
    // subscription map, so it reports the session ids back here.
    let (closed_tx, mut closed_rx) = mpsc::channel::<String>(16);
    // The writer task: the outbox's sole consumer, owning the socket sink.
    // Its termination (socket write failed, or every sender dropped) ends
    // this loop — nothing can leave the connection after that.
    let mut writer = tokio::spawn(write_outbound(sink, outbox_rx, closed_tx));
    // Reverse RPC state shared with the WsReverseRpcCallback instances
    // stored on subscribed sessions' artifacts: the callback sends requests
    // straight onto the outbox (the writer delivers them) and awaits the
    // client's response directly — the connection's loop is not involved in
    // the reverse-RPC send path, so a wait-mode turn whose handler this loop
    // awaits inline can still deliver approval.ask / question.ask.
    let reverse_rpc = Arc::new(std::sync::Mutex::new(ReverseRpcState::new(
        reverse_rpc_allowlist,
    )));
    let mut connection = RpcConnection {
        state,
        outbox: outbox_tx,
        subscriptions: HashMap::new(),
        reverse_rpc,
        terminal_attachments: HashMap::new(),
        mcp_subscription: None,
        skill_catalog_subscription: None,
        resync_cooldowns: HashMap::new(),
    };

    // Heartbeat: one `ping` per interval; a connection silent for
    // `3 × interval` is dropped (design §5.5).
    let mut heartbeat = tokio::time::interval(connection.state.heartbeat_interval);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);
    heartbeat.tick().await; // consume the immediate first tick
    let timeout = connection.state.heartbeat_interval * HEARTBEAT_TIMEOUT_MULTIPLIER;
    let mut liveness_deadline = tokio::time::Instant::now() + timeout;

    loop {
        tokio::select! {
            biased;
            inbound = incoming.next() => {
                match inbound {
                    Some(Ok(Message::Text(text))) => {
                        // Any inbound frame counts as liveness (not just pongs).
                        liveness_deadline = tokio::time::Instant::now() + timeout;
                        let text: &str = &text;
                        connection.handle_incoming_frame(text).await;
                    }
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(_)) => {}
                }
            }
            closed = closed_rx.recv() => match closed {
                // The writer drained a session's closing marker: drop the
                // subscription (the session's `SessionClosed` event was
                // already queued ahead of it).
                Some(session_id) => {
                    if let Some(forwarder) = connection.subscriptions.remove(&session_id) {
                        forwarder.abort();
                        tracing::debug!(session_id, "subscription dropped; session closed");
                    }
                }
                // The writer is gone (channel closed): the client is gone.
                None => break,
            },
            _ = heartbeat.tick() => {
                let nonce = Uuid::new_v4().to_string();
                let ping = JsonRpcFrame::Notification(JsonRpcNotification::new(
                    "ping",
                    json!({ "nonce": nonce }),
                ))
                .to_value();
                if !connection.enqueue_frame(ping).await {
                    break;
                }
            }
            _ = tokio::time::sleep_until(liveness_deadline) => {
                tracing::warn!("dropping connection: no frame received within the heartbeat timeout");
                break;
            }
            _ = &mut writer => break,
        }
    }
    // Stop the writer (a no-op if it already ended): dropping its receiver
    // closes the outbox, so every spawned task still holding a sender —
    // event forwarders, this connection's handlers awaiting `enqueue_frame`
    // — observes the closure instead of leaking.
    writer.abort();
    // The connection is gone: revoke the shared reverse-RPC state and drop
    // its pending responders. The callback's `Arc` may outlive this loop on
    // the session artifacts, so a later send must fail fast (the historical
    // channel-closed semantics) instead of queueing onto this now-undrained
    // outbox, and an in-flight ask awaiting a client answer must error
    // instead of hanging forever.
    //
    // DESIGN CONSTRAINT (ADR-008, "never attached vs attached-then-broken"):
    // this teardown deliberately does NOT clear the session artifacts'
    // reverse-RPC SLOT. The slot staying populated (with the revoked
    // callback) is exactly what distinguishes "a client was attached and
    // the connection broke" (the question delivery loop keeps waiting for
    // a re-subscribe to re-install a fresh callback) from "no client was
    // ever attached" (the slot is empty — fail fast). Clearing the slot
    // here would silently regress the question-channel resilience into
    // the never-attached fast-fail. Do not add slot clearing here.
    {
        let mut reverse_rpc = connection
            .reverse_rpc
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        reverse_rpc.revoked = true;
        reverse_rpc.pending.clear();
    }
    // Detach every terminal sink this connection installed — the sinks hold
    // this connection's outbox and must not outlive it (the shells keep
    // running; they belong to their sessions).
    connection.cleanup_terminal_attachments().await;
    // End the app-level MCP event forwarder (M7): the outbox closure would
    // end it on its own, but the explicit abort guarantees no buffered
    // event escapes after the connection is gone.
    if let Some(forwarder) = connection.mcp_subscription.take() {
        forwarder.abort();
    }
    // End the app-level skill-catalog event forwarder (ADR-010 Task 6) the
    // same way.
    if let Some(forwarder) = connection.skill_catalog_subscription.take() {
        forwarder.abort();
    }
}

/// Server identity + capabilities, sent as the first frame (design §5.2).
fn server_hello_frame(state: &ServerState) -> JsonRpcFrame {
    JsonRpcFrame::Notification(JsonRpcNotification::new(
        "server_hello",
        json!({
            "serverId": state.server_id,
            "version": state.version,
            "heartbeatMs": state.heartbeat_interval.as_millis() as u64,
            "maxEventBufferSize": EVENT_LOG_CAPACITY,
            "capabilities": {
                "eventBatching": true,
            },
        }),
    ))
}

/// Send `server_hello`. Returns `false` when the client is gone.
async fn send_server_hello(sink: &mut SplitSink<WebSocket, Message>, state: &ServerState) -> bool {
    send_text(sink, &server_hello_frame(state).to_string()).await
}

/// The handshake as one step: greet with `server_hello`, then wait for the
/// `client_hello` request. Returns the request id and the client's
/// `reverseRpc` allowlist, or `None` when the client is gone or the
/// handshake failed.
async fn send_server_hello_and_await_client_hello(
    sink: &mut SplitSink<WebSocket, Message>,
    incoming: &mut SplitStream<WebSocket>,
    state: &ServerState,
) -> Option<(i64, Vec<String>)> {
    if !send_server_hello(sink, state).await {
        return None;
    }
    await_client_hello(incoming, sink).await
}

/// Wait for the client's `client_hello` request and answer it. Returns the
/// request id and the client's `reverseRpc` allowlist, or `None` when the
/// client disconnects, sends a different frame first, or the handshake times
/// out.
async fn await_client_hello(
    incoming: &mut SplitStream<WebSocket>,
    sink: &mut SplitSink<WebSocket, Message>,
) -> Option<(i64, Vec<String>)> {
    loop {
        match tokio::time::timeout(HANDSHAKE_TIMEOUT, incoming.next()).await {
            Err(_elapsed) => {
                tracing::warn!("client_hello not received within the handshake timeout");
                return None;
            }
            Ok(None) | Ok(Some(Err(_))) => return None,
            Ok(Some(Ok(Message::Text(text)))) => {
                let text: &str = &text;
                match JsonRpcFrame::parse(text) {
                    Ok(JsonRpcFrame::Request(request)) if request.method == "client_hello" => {
                        let Some(RequestId::Number(id)) = request.id else {
                            tracing::warn!("client_hello without an id is not a valid request");
                            let _ = send_text(
                                sink,
                                &protocol_error_frame(
                                    INVALID_REQUEST,
                                    "client_hello must carry an id",
                                    None,
                                )
                                .to_string(),
                            )
                            .await;
                            return None;
                        };
                        let client_id = request
                            .params
                            .get("clientId")
                            .and_then(Value::as_str)
                            .unwrap_or("<unknown>");
                        let allowlist = request
                            .params
                            .get("capabilities")
                            .and_then(|caps| caps.get("reverseRpc"))
                            .and_then(Value::as_array)
                            .map(|items| {
                                items
                                    .iter()
                                    .filter_map(Value::as_str)
                                    .map(str::to_owned)
                                    .collect()
                            })
                            .unwrap_or_default();
                        tracing::debug!(
                            client_id,
                            ?allowlist,
                            "client_hello received; handshake complete"
                        );
                        let ack = JsonRpcFrame::Response(JsonRpcResponse::ok(
                            id,
                            json!({ "accepted": true }),
                        ))
                        .to_string();
                        return send_text(sink, &ack).await.then_some((id, allowlist));
                    }
                    Ok(_other_frame) => {
                        tracing::warn!(text, "expected client_hello during handshake");
                        let _ = send_text(
                            sink,
                            &protocol_error_frame(INVALID_REQUEST, "expected client_hello", None)
                                .to_string(),
                        )
                        .await;
                        return None;
                    }
                    Err(error) => {
                        tracing::warn!(message = %error.message, "invalid frame during handshake");
                        let _ = send_text(
                            sink,
                            &protocol_error_frame(error.code, error.message, error.id).to_string(),
                        )
                        .await;
                        return None;
                    }
                }
            }
            Ok(Some(Ok(Message::Close(_)))) => return None,
            Ok(Some(Ok(_))) => continue,
        }
    }
}

/// Serialize and send one JSON text frame. Returns `false` when the client
/// is gone.
async fn send_text(sink: &mut SplitSink<WebSocket, Message>, text: &str) -> bool {
    match sink.send(Message::Text(text.to_owned().into())).await {
        Ok(()) => true,
        Err(_) => {
            tracing::debug!("client disconnected from WebSocket");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mirri_core::{ReverseRpcCallback, SessionId, SessionManager};
    use mirri_transport::adapter::{EngineAdapter, EngineError};

    /// A stub engine for constructing a [`ServerState`] in unit tests that
    /// only exercise transport-side state (auth headers, hello).
    struct NoopEngine;

    #[async_trait::async_trait]
    impl EngineAdapter for NoopEngine {
        async fn call_method(
            &self,
            method: &str,
            _params: serde_json::Value,
        ) -> Result<serde_json::Value, mirri_transport::adapter::EngineError> {
            Err(mirri_transport::adapter::EngineError::MethodNotFound(
                method.to_string(),
            ))
        }

        async fn subscribe_events(
            &self,
        ) -> Result<
            tokio::sync::mpsc::Receiver<mirri_transport::frame::JsonRpcNotification>,
            mirri_transport::adapter::EngineError,
        > {
            let (_tx, rx) = tokio::sync::mpsc::channel(8);
            Ok(rx)
        }

        async fn on_reverse_rpc(
            &self,
            _handler: mirri_transport::adapter::ReverseRpcHandler,
        ) -> Result<(), mirri_transport::adapter::EngineError> {
            Ok(())
        }

        async fn dispose(&self) -> Result<(), mirri_transport::adapter::EngineError> {
            Ok(())
        }
    }

    fn message_received(summary: &str) -> SessionEvent {
        SessionEvent::MessageReceived {
            agent_id: "main".to_string(),
            role: "user".to_string(),
            content_summary: summary.to_string(),
        }
    }

    // -----------------------------------------------------------------------
    // Reverse RPC: send path, response resolution, callback bridge
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn should_send_reverse_request_with_incrementing_ids_and_whitelist() {
        let (outbox_tx, mut outbox_rx) = mpsc::channel::<Outbound>(16);
        let mut reverse = ReverseRpcState::new(vec!["approval".to_string()]);

        // A method outside the negotiated whitelist is refused.
        let error = reverse
            .send(&outbox_tx, "fs.readTextFile", json!({ "path": "/x" }))
            .expect_err("non-negotiated method is refused");
        assert!(error.contains("not negotiated"), "{error}");

        let (id, mut receiver) = reverse
            .send(&outbox_tx, "approval.ask", json!({ "toolName": "bash" }))
            .expect("negotiated method sends");
        assert_eq!(id, 1);

        // The request frame went out with the minted id.
        let Outbound::Frame(frame) = outbox_rx.recv().await.expect("request frame queued") else {
            panic!("expected a frame");
        };
        assert_eq!(frame["jsonrpc"], "2.0");
        assert_eq!(frame["id"], 1);
        assert_eq!(frame["method"], "approval.ask");

        // The client's response resolves the pending one-shot.
        let response = JsonRpcResponse::ok(1, json!({ "approved": false }));
        assert_eq!(reverse.handle_response(response.clone()), Some(1));
        assert_eq!(receiver.try_recv().expect("response delivered"), response);

        // The next reverse request increments the id.
        let (next_id, _) = reverse
            .send(&outbox_tx, "approval.ask", json!({}))
            .expect("second request sends");
        assert_eq!(next_id, 2);
    }

    #[test]
    fn should_ignore_response_to_unknown_reverse_rpc_id() {
        let mut reverse = ReverseRpcState::new(Vec::new());
        assert_eq!(
            reverse.handle_response(JsonRpcResponse::ok(99, json!({}))),
            None
        );
        assert!(reverse.pending.is_empty());
    }

    /// Given a `WsReverseRpcCallback` behind the engine's trait, when the
    /// engine calls it, the request frame goes straight onto the connection's
    /// outbox and a mock client answer (the real loop's `handle_response` on
    /// the shared state) resolves the callback with the client's result.
    #[tokio::test]
    async fn should_route_reverse_rpc_call_through_callback_to_client_and_back() {
        let (outbox_tx, mut outbox_rx) = mpsc::channel::<Outbound>(16);
        let reverse = Arc::new(std::sync::Mutex::new(ReverseRpcState::new(vec![
            "approval".to_string(),
        ])));
        let callback: Arc<dyn mirri_core::ReverseRpcCallback> =
            Arc::new(WsReverseRpcCallback::new(Arc::clone(&reverse), outbox_tx));

        // The engine's call runs on its task; the callback sends the request
        // directly (no connection loop involved).
        let call = tokio::spawn({
            let callback = Arc::clone(&callback);
            async move {
                callback
                    .call("approval.ask", json!({ "toolName": "bash" }))
                    .await
            }
        });

        // The request frame went out on the outbox with the minted id.
        let Outbound::Frame(frame) = outbox_rx.recv().await.expect("request frame") else {
            panic!("expected a frame");
        };
        assert_eq!(frame["jsonrpc"], "2.0");
        assert_eq!(frame["method"], "approval.ask");
        assert_eq!(frame["params"]["toolName"], "bash");

        // The mock client's response resolves the pending one-shot, exactly
        // like the connection's inbound path does (handle_response under the
        // shared lock).
        let id = frame["id"].as_i64().expect("request id");
        reverse
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .handle_response(JsonRpcResponse::ok(id, json!({ "decision": "approved" })));

        let result = call.await.expect("callback resolves");
        assert_eq!(result.expect("delivered result")["decision"], "approved");
    }

    /// Given a `WsReverseRpcCallback` whose connection is gone, when the
    /// engine calls it, the call fails instead of hanging: a closed outbox
    /// (the writer is gone) and a revoked state (the teardown path) both
    /// error out.
    #[tokio::test]
    async fn should_fail_reverse_rpc_call_when_connection_loop_is_gone() {
        let (outbox_tx, outbox_rx) = mpsc::channel::<Outbound>(16);
        let reverse = Arc::new(std::sync::Mutex::new(ReverseRpcState::new(vec![
            "approval".to_string(),
        ])));
        let callback = WsReverseRpcCallback::new(Arc::clone(&reverse), outbox_tx);

        // The writer is gone: dropping the outbox's receive end closes the
        // channel, so the callback's send fails and the pending entry is
        // withdrawn.
        drop(outbox_rx);
        let error = callback
            .call("approval.ask", json!({}))
            .await
            .expect_err("closed outbox fails the call");
        assert_eq!(error, "reverse-rpc channel error: outbox closed");
        assert!(
            mirri_core::reverse_rpc::is_channel_error(&error),
            "a closed outbox is a channel-class error (ADR-008 / D-1)"
        );

        // The teardown path: the connection revoked the shared state before
        // dropping it, so a callback still held on the session artifacts
        // fails fast instead of queueing onto the undrained outbox.
        reverse.lock().unwrap_or_else(|e| e.into_inner()).revoked = true;
        let error = callback
            .call("approval.ask", json!({}))
            .await
            .expect_err("revoked state fails the call");
        assert_eq!(error, "reverse-rpc channel error: connection closed");
        assert!(
            mirri_core::reverse_rpc::is_channel_error(&error),
            "a revoked connection is a channel-class error (ADR-008 / D-1)"
        );
        assert!(
            reverse
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pending
                .is_empty(),
            "a failed send must not leak a pending response"
        );
    }

    /// Given a `WsReverseRpcCallback` whose shared state was revoked by the
    /// connection teardown, when the engine calls it, then the error carries
    /// the channel-error signature — the stable prefix the question
    /// delivery loop classifies on (ADR-008 / D-1: implementors construct
    /// via `CHANNEL_ERROR_PREFIX`, consumers via `is_channel_error`) — while
    /// a business-class refusal ("not negotiated") stays unsigned so it is
    /// never mistaken for a recoverable channel drop.
    #[tokio::test]
    async fn should_return_channel_signed_error_when_state_revoked() {
        let (outbox_tx, _outbox_rx) = mpsc::channel::<Outbound>(16);
        let reverse = Arc::new(std::sync::Mutex::new(ReverseRpcState::new(vec![
            "question".to_string(),
        ])));
        let callback = WsReverseRpcCallback::new(Arc::clone(&reverse), outbox_tx);

        // The teardown path: revoked state, live outbox.
        reverse.lock().unwrap_or_else(|e| e.into_inner()).revoked = true;
        let error = callback
            .call("question.ask", json!({}))
            .await
            .expect_err("revoked state fails the call");
        assert_eq!(error, "reverse-rpc channel error: connection closed");
        assert!(
            mirri_core::reverse_rpc::is_channel_error(&error),
            "the revoked teardown error must carry the channel signature"
        );

        // The business-class contrast: an un-negotiated domain is NOT a
        // channel error — the client's capability set does not include it,
        // and no reconnect changes that. It must stay unsigned.
        let (outbox_tx, _outbox_rx) = mpsc::channel::<Outbound>(16);
        let reverse = Arc::new(std::sync::Mutex::new(ReverseRpcState::new(Vec::new())));
        let callback = WsReverseRpcCallback::new(reverse, outbox_tx);
        let error = callback
            .call("question.ask", json!({}))
            .await
            .expect_err("un-negotiated domain fails the call");
        assert!(
            !mirri_core::reverse_rpc::is_channel_error(&error),
            "a not-negotiated refusal is a business error, not a channel error: {error}"
        );
        assert!(error.contains("was not negotiated"), "unexpected error: {error}");
    }

    // -----------------------------------------------------------------------
    // Reverse RPC while a wait-mode request is in flight (serve deadlock,
    // P0-1 acceptance followup)
    // -----------------------------------------------------------------------

    /// A stub engine whose wait-mode `message.send` asks for approval through
    /// the session's reverse-RPC callback BEFORE it returns — the embedded
    /// engine's shape (the turn's `approval.ask` is dispatched through the
    /// session artifacts' callback and awaited inside `call_method`). The
    /// client's decision becomes the method result, so answering the ask
    /// completes the wait-mode request.
    struct ApprovalProbeEngine {
        manager: Arc<SessionManager>,
    }

    #[async_trait::async_trait]
    impl EngineAdapter for ApprovalProbeEngine {
        async fn call_method(&self, method: &str, params: Value) -> Result<Value, EngineError> {
            if method == "message.send" {
                let session_id = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .or_else(|| params.get("session_id").and_then(Value::as_str))
                    .expect("message.send carries a session id")
                    .to_string();
                let artifacts = self
                    .manager
                    .session_artifacts(&SessionId::new(session_id))
                    .expect("the addressed session exists");
                // The connection's reverse-RPC callback, stamped onto the
                // session artifacts by `session.subscribe` — the same
                // precedence the embedded engine's dispatch resolves
                // (message_handlers.rs: artifacts first, engine-level
                // fallback second; the serve wire only ever has the former).
                let callback = artifacts
                    .reverse_rpc_callback
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone()
                    .expect("session.subscribe installed the connection's reverse-RPC callback");
                let decision = callback
                    .call("approval.ask", json!({ "toolName": "write" }))
                    .await
                    .map_err(|error| {
                        EngineError::Internal(format!("approval channel failed: {error}"))
                    })?;
                return Ok(decision);
            }
            Err(EngineError::MethodNotFound(method.to_string()))
        }

        async fn subscribe_events(
            &self,
        ) -> Result<mpsc::Receiver<JsonRpcNotification>, EngineError> {
            let (_tx, rx) = mpsc::channel(8);
            Ok(rx)
        }

        async fn on_reverse_rpc(
            &self,
            _handler: mirri_transport::adapter::ReverseRpcHandler,
        ) -> Result<(), EngineError> {
            // The serve wire never calls `EngineAdapter::on_reverse_rpc`:
            // the per-connection callback is stamped into the session
            // artifacts at subscribe. Stored-handler registration is
            // accepted and ignored, mirroring the other stub engines.
            Ok(())
        }

        async fn dispose(&self) -> Result<(), EngineError> {
            Ok(())
        }
    }

    /// Boot a full HTTP/WS server over the given engine and session manager
    /// on an ephemeral port — the unit-test stand-in for
    /// `tests/common::TestServer::start_with_engine`: the serve surface is
    /// exercised through a real WebSocket connection.
    async fn serve_engine(
        engine: Arc<dyn EngineAdapter>,
        manager: Arc<SessionManager>,
    ) -> (String, ServerState) {
        let state = ServerState::with_engine(engine, manager, "0.1.0-test".to_string(), None);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let url = format!("ws://{addr}/api/v1/rpc");
        let server_state = state.clone();
        tokio::spawn(async move {
            if let Err(error) = crate::http::serve(listener, server_state, None).await {
                tracing::error!(%error, "ws_rpc test server exited with an error");
            }
        });
        (url, state)
    }

    type TestSocket = tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >;

    /// Connect to the unified JSON-RPC WebSocket endpoint.
    async fn connect_ws(url: &str) -> TestSocket {
        tokio_tungstenite::connect_async(url)
            .await
            .expect("websocket upgrade")
            .0
    }

    /// Read the next text frame, parsing it into one or more JSON values (a
    /// batched array frame is flattened into its members).
    async fn ws_next_frames(socket: &mut TestSocket) -> Vec<Value> {
        let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .expect("message arrives within the timeout")
            .expect("stream stays open")
            .expect("text message");
        let value: Value = serde_json::from_str(message.to_text().expect("UTF-8 text message"))
            .expect("message is valid JSON");
        match value {
            Value::Array(frames) => frames,
            other => vec![other],
        }
    }

    /// Read the next single frame (panics if the server batched several).
    async fn ws_next_frame(socket: &mut TestSocket) -> Value {
        let mut frames = ws_next_frames(socket).await;
        assert_eq!(frames.len(), 1, "expected a single frame, got {frames:?}");
        frames.remove(0)
    }

    /// Read frames until the response with the given id arrives, skipping
    /// any interleaved notifications (e.g. heartbeats).
    async fn ws_next_response(socket: &mut TestSocket, id: i64) -> Value {
        loop {
            for frame in ws_next_frames(socket).await {
                if frame.get("id").and_then(Value::as_i64) == Some(id) {
                    return frame;
                }
            }
        }
    }

    /// Given a client whose `session.subscribe` installed the connection's
    /// reverse-RPC callback, when a wait-mode method request is in flight
    /// and the engine asks for approval, then the `approval.ask` frame
    /// reaches the client while the request is still pending, and answering
    /// it lets the wait-mode request complete with the decision as its
    /// content.
    ///
    /// Before the fix this deadlocks: the connection loop awaits the whole
    /// wait-mode turn inline, so the loop's reverse-RPC delivery arm is
    /// never polled and the ask is never sent.
    #[tokio::test]
    async fn should_deliver_reverse_rpc_while_a_wait_mode_request_is_in_flight() {
        // Given a live session on a server whose engine asks for approval
        // from inside a wait-mode message.send
        let tmp = tempfile::tempdir().expect("temp dir");
        let app = mirri_core::AppScope::new(mirri_core::AppScopeConfig::default());
        let workspace = app
            .create_workspace(
                mirri_core::WorkspaceId::new("approval-ws"),
                tmp.path().to_path_buf(),
            )
            .await
            .expect("create workspace");
        let manager = Arc::new(mirri_core::SessionManager::with_persistence_dir(
            workspace,
            tmp.path().join(".mirri").join("sessions"),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        ));
        let now = chrono::Utc::now();
        manager
            .create_session_with_id(
                SessionId::new("approval-session"),
                mirri_core::SessionMetadata {
                    title: "approval in wait mode".to_string(),
                    created_at: now,
                    updated_at: now,
                    tags: Default::default(),
                    archived: false,
                    is_custom_title: false,
                    work_dir: None,
                    last_prompt: None,
                    forked_from: None,
                    cache_root_id: None,
                    custom: None,
                    last_used_at: None,
                },
                mirri_core::SessionScopeConfig::default(),
            )
            .await
            .expect("create session");
        let engine: Arc<dyn EngineAdapter> = Arc::new(ApprovalProbeEngine {
            manager: Arc::clone(&manager),
        });
        let (url, _state) = serve_engine(engine, Arc::clone(&manager)).await;

        // Given a connected client whose subscribe installs the connection's
        // reverse-RPC callback (allowlist: approval)
        let mut socket = connect_ws(&url).await;
        let hello = ws_next_frame(&mut socket).await;
        assert_eq!(hello["method"], "server_hello");
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                r#"{"id":1,"method":"client_hello","params":{"clientId":"wait-approval-client","capabilities":{"reverseRpc":["approval"]}}}"#
                    .into(),
            ))
            .await
            .expect("send client_hello");
        assert_eq!(
            ws_next_response(&mut socket, 1).await["result"]["accepted"],
            true
        );
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                json!({ "id": 2, "method": "session.subscribe", "params": { "sessionId": "approval-session" } })
                    .to_string()
                    .into(),
            ))
            .await
            .expect("send subscribe");
        assert!(ws_next_response(&mut socket, 2).await["result"].is_object());

        // When a wait-mode message.send is sent (the connection loop is
        // inside the turn for the whole request) and the engine asks for
        // approval
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                json!({
                    "id": 3,
                    "method": "message.send",
                    "params": { "sessionId": "approval-session", "text": "write the file", "mode": "wait" },
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("send message.send");

        // Then the approval.ask frame reaches the client while the wait-mode
        // request is still in flight (5s budget; before the fix the loop is
        // stuck in the turn and the ask is never delivered — deadlock)
        let ask = tokio::time::timeout(Duration::from_secs(5), ws_next_frame(&mut socket))
            .await
            .expect("approval.ask must arrive while the wait-mode request is in flight");
        assert_eq!(ask["method"], "approval.ask");
        assert_eq!(ask["params"]["toolName"], "write");
        let ask_id = ask["id"].as_i64().expect("approval.ask carries an id");

        // When the client answers with an approved decision
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                json!({
                    "jsonrpc": "2.0",
                    "id": ask_id,
                    "result": { "decision": "approved" },
                })
                .to_string()
                .into(),
            ))
            .await
            .expect("send approval decision");

        // Then the wait-mode method response returns with the decision as
        // its content (the turn is no longer blocked on the approval)
        let response =
            tokio::time::timeout(Duration::from_secs(5), ws_next_response(&mut socket, 3))
                .await
                .expect("the wait-mode method response must return after the approval");
        assert_eq!(response["result"]["decision"], "approved");
    }

    /// Given a session whose approval gate holds one pending approval, when
    /// a NEW client subscribes, then the pending approval is replayed to it
    /// as an `approval.ask` notification frame (id-less: the replay is a
    /// pure notify face — the gate, not the revived notification, is where
    /// the decision lands via `approval.decide`) whose params carry the
    /// same ask shape the engine issues (`tool_name` / `tool_call_id` /
    /// `action` / `display` / `request_id`). ADR-011 e: the card rebuild
    /// must not depend on the pre-disconnect notification surviving.
    #[tokio::test]
    async fn should_replay_pending_approvals_as_ask_notifications_when_a_client_subscribes() {
        // Given a live session whose gate holds one pending approval
        let tmp = tempfile::tempdir().expect("temp dir");
        let app = mirri_core::AppScope::new(mirri_core::AppScopeConfig::default());
        let workspace = app
            .create_workspace(
                mirri_core::WorkspaceId::new("replay-ws"),
                tmp.path().to_path_buf(),
            )
            .await
            .expect("create workspace");
        let manager = Arc::new(mirri_core::SessionManager::with_persistence_dir(
            workspace,
            tmp.path().join(".mirri").join("sessions"),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        ));
        let now = chrono::Utc::now();
        manager
            .create_session_with_id(
                SessionId::new("replay-session"),
                mirri_core::SessionMetadata {
                    title: "approval replay".to_string(),
                    created_at: now,
                    updated_at: now,
                    tags: Default::default(),
                    archived: false,
                    is_custom_title: false,
                    work_dir: None,
                    last_prompt: None,
                    forked_from: None,
                    cache_root_id: None,
                    custom: None,
                    last_used_at: None,
                },
                mirri_core::SessionScopeConfig::default(),
            )
            .await
            .expect("create session");
        let artifacts = manager
            .session_artifacts(&SessionId::new("replay-session"))
            .expect("created session has artifacts");
        let (request_id, _receiver) =
            artifacts
                .approval_gate
                .register(mirri_core::session::gates::ApprovalSnapshot {
                    tool_name: "Bash".to_string(),
                    action: "execute Bash".to_string(),
                    display: json!({ "summary": "echo hi" }),
                    agent_id: Uuid::nil(),
                    turn_id: Some(7),
                    tool_call_id: Some("call_replay".to_string()),
                    created_at: 1_000,
                    approval_rule: None,
                    grant_dir: None,
                });

        // When a new client connects and subscribes to the session
        let engine: Arc<dyn EngineAdapter> = Arc::new(NoopEngine);
        let (url, _state) = serve_engine(engine, Arc::clone(&manager)).await;
        let mut socket = connect_ws(&url).await;
        let hello = ws_next_frame(&mut socket).await;
        assert_eq!(hello["method"], "server_hello");
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                r#"{"id":1,"method":"client_hello","params":{"clientId":"replay-client","capabilities":{"reverseRpc":["approval"]}}}"#
                    .into(),
            ))
            .await
            .expect("send client_hello");
        assert_eq!(
            ws_next_response(&mut socket, 1).await["result"]["accepted"],
            true
        );
        socket
            .send(tokio_tungstenite::tungstenite::Message::Text(
                json!({ "id": 2, "method": "session.subscribe", "params": { "sessionId": "replay-session" } })
                    .to_string()
                    .into(),
            ))
            .await
            .expect("send subscribe");
        assert!(ws_next_response(&mut socket, 2).await["result"].is_object());

        // Then the pending approval arrives as an id-less approval.ask
        // notification carrying the same ask shape the engine issued
        let ask = tokio::time::timeout(Duration::from_secs(5), ws_next_frame(&mut socket))
            .await
            .expect("the replayed approval.ask arrives after subscribe");
        assert_eq!(ask["method"], "approval.ask");
        assert!(
            ask.get("id").is_none(),
            "the replay is a notification, not an answerable request: {ask}"
        );
        assert_eq!(ask["params"]["tool_name"], "Bash");
        assert_eq!(ask["params"]["tool_call_id"], "call_replay");
        assert_eq!(ask["params"]["action"], "execute Bash");
        assert_eq!(ask["params"]["display"], json!({ "summary": "echo hi" }));
        assert_eq!(
            ask["params"]["request_id"],
            request_id.to_string(),
            "the replayed ask names the gate entry: {ask}"
        );
    }

    // -----------------------------------------------------------------------
    // Event log: seq stamping, ring-buffer eviction, cursor replay
    // -----------------------------------------------------------------------

    #[test]
    fn should_log_events_with_monotonic_seq_and_replay_after_cursor() {
        let channel = SessionEventChannel::new("epoch-a".to_string().into(), 4);
        channel.append(message_received("first"));
        channel.append(message_received("second"));
        channel.append(message_received("third"));

        // Fresh subscribe: no replay, tail is the newest seq.
        let (replay, tail_seq, _rx) = channel.subscribe(None);
        assert!(replay.is_empty());
        assert_eq!(tail_seq, 3);

        // Cursor at seq 1 replays 2 and 3 in order.
        let (replay, tail_seq, _rx) = channel.subscribe(Some(&Cursor {
            seq: 1,
            epoch: "epoch-a".to_string(),
        }));
        let seqs: Vec<u64> = replay.iter().map(|record| record.seq).collect();
        assert_eq!(seqs, vec![2, 3]);
        assert_eq!(tail_seq, 3);

        // A cursor behind the buffer starts at the earliest retained record.
        let (replay, _, _) = channel.subscribe(Some(&Cursor {
            seq: 0,
            epoch: "epoch-a".to_string(),
        }));
        assert_eq!(replay.len(), 3);
    }

    #[test]
    fn should_evict_oldest_events_from_the_ring_buffer() {
        let channel = SessionEventChannel::new("epoch-a".to_string().into(), 2);
        channel.append(message_received("first"));
        channel.append(message_received("second"));
        channel.append(message_received("third"));

        let (replay, tail_seq, _rx) = channel.subscribe(Some(&Cursor {
            seq: 0,
            epoch: "epoch-a".to_string(),
        }));
        let seqs: Vec<u64> = replay.iter().map(|record| record.seq).collect();
        assert_eq!(seqs, vec![2, 3], "the evicted first event is gone");
        assert_eq!(tail_seq, 3);
    }

    #[test]
    fn should_not_replay_when_cursor_epoch_mismatches() {
        let channel = SessionEventChannel::new("epoch-a".to_string().into(), 4);
        channel.append(message_received("first"));

        let (replay, tail_seq, _rx) = channel.subscribe(Some(&Cursor {
            seq: 0,
            epoch: "epoch-b".to_string(),
        }));
        assert!(replay.is_empty(), "stale epoch means resync, not replay");
        assert_eq!(tail_seq, 1, "the ack still carries the current position");
    }

    #[test]
    fn should_not_warn_when_subscribing_without_a_cursor() {
        // Given a fresh subscribe carries no cursor (the client syncs from
        // zero and expects no replay)
        let cursor: Option<&Cursor> = None;
        // When the replay window is empty and the log has advanced
        // Then the subscribe is not flagged as a bypassed replay
        assert!(!cursor_replay_bypassed(cursor, 3, None));
    }

    #[test]
    fn should_not_warn_when_the_cursor_is_already_at_the_tail() {
        // Given a client whose cursor seq equals the log's tail seq
        let at_tail = Cursor {
            seq: 3,
            epoch: "epoch-a".to_string(),
        };
        // When the replay window is empty — there is simply nothing newer
        // Then no resync warning fires: the client is already up to date
        assert!(!cursor_replay_bypassed(Some(&at_tail), 3, None));
        // A cursor ahead of the tail is equally not a missed-event gap.
        let ahead = Cursor {
            seq: 4,
            epoch: "epoch-a".to_string(),
        };
        assert!(!cursor_replay_bypassed(Some(&ahead), 3, None));
    }

    #[test]
    fn should_warn_when_the_cursor_is_behind_the_tail_but_replay_came_back_empty() {
        // Given a client whose cursor seq is behind the log's tail seq
        let cursor = Cursor {
            seq: 1,
            epoch: "epoch-a".to_string(),
        };
        // When the replay window came back empty — the expected events were
        // evicted from the ring buffer, or the epoch no longer matches and
        // suppresssed the replay
        // Then the resync warning fires: events the client expected are gone
        assert!(cursor_replay_bypassed(Some(&cursor), 3, None));
    }

    #[test]
    fn should_warn_when_the_replay_skips_evicted_events_between_cursor_and_front() {
        // Given a client whose cursor seq predates the ring buffer's
        // earliest retained record (the events after the cursor were
        // evicted, yet a partial replay of the retained window remains)
        let cursor = Cursor {
            seq: 1,
            epoch: "epoch-a".to_string(),
        };
        // When the replay window starts past the cursor's next event
        // (front 5 means seqs 2..=4 are gone)
        // Then the resync signal fires: the client cannot catch up from the
        // cursor it holds even though a partial replay is available
        assert!(cursor_replay_bypassed(Some(&cursor), 7, Some(5)));
    }

    #[test]
    fn should_not_warn_when_the_replay_is_contiguous_from_the_cursor() {
        // Given a client whose cursor seq is behind the tail
        let cursor = Cursor {
            seq: 1,
            epoch: "epoch-a".to_string(),
        };
        // When the replay window starts exactly at the cursor's next event
        // (nothing was evicted between the cursor and the replay front)
        // Then no resync signal fires: the replay fully covers the gap
        assert!(!cursor_replay_bypassed(Some(&cursor), 7, Some(2)));
    }

    #[tokio::test]
    async fn should_fan_events_out_to_each_subscriber_once() {
        let channel = Arc::new(SessionEventChannel::new("epoch-a".to_string().into(), 8));
        let (_replay, _tail, mut rx1) = channel.subscribe(None);
        let (_replay, _tail, mut rx2) = channel.subscribe(None);

        channel.append(message_received("first"));

        for receiver in [&mut rx1, &mut rx2] {
            let record = receiver.recv().await.expect("live record");
            assert_eq!(record.seq, 1);
        }
        let no_duplicate = tokio::time::timeout(Duration::from_millis(50), rx1.recv()).await;
        assert!(
            no_duplicate.is_err(),
            "no second delivery of the same event"
        );
    }

    #[tokio::test]
    async fn should_send_volatile_events_live_but_skip_the_durable_log() {
        let channel = Arc::new(SessionEventChannel::new("epoch-a".to_string().into(), 4));
        let (_replay, _tail, mut rx) = channel.subscribe(None);

        channel.append(SessionEvent::AgentMessageChunk {
            agent_id: "main".to_string(),
            text: "delta".to_string(),
        });
        channel.append(message_received("first"));

        // Both records reach live subscribers — the volatile chunk first,
        // stamped seq 0.
        let volatile = rx.recv().await.expect("volatile record broadcast live");
        assert_eq!(volatile.seq, 0, "volatile events carry seq 0");
        assert!(matches!(
            volatile.event,
            SessionEvent::AgentMessageChunk { .. }
        ));
        let durable = rx.recv().await.expect("durable record broadcast live");
        assert_eq!(durable.seq, 1, "durable seq numbering is unaffected");

        // The volatile record never lands in the ring buffer: it neither
        // consumes a seq nor appears in cursor replay.
        let (replay, tail_seq, _rx) = channel.subscribe(Some(&Cursor {
            seq: 0,
            epoch: "epoch-a".to_string(),
        }));
        assert_eq!(replay.len(), 1, "volatile events bypass the durable log");
        assert_eq!(replay[0].seq, 1);
        assert!(matches!(
            replay[0].event,
            SessionEvent::MessageReceived { .. }
        ));
        assert_eq!(tail_seq, 1, "volatile events do not advance the tail seq");
    }

    // -----------------------------------------------------------------------
    // Auth subprotocol parsing
    // -----------------------------------------------------------------------

    #[test]
    fn should_extract_bearer_subprotocol_from_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::SEC_WEBSOCKET_PROTOCOL,
            "chat, mirri-code.bearer.secret-token".parse().unwrap(),
        );
        assert_eq!(
            bearer_subprotocol(&headers).as_deref(),
            Some("mirri-code.bearer.secret-token")
        );

        let multiple_headers = HeaderMap::new();
        assert_eq!(bearer_subprotocol(&multiple_headers), None);
    }

    #[tokio::test]
    async fn should_accept_only_matching_bearer_token() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let app = mirri_core::AppScope::new(mirri_core::AppScopeConfig::default());
        let workspace = app
            .create_workspace(mirri_core::WorkspaceId::new("ws"), tmp.path().to_path_buf())
            .await
            .expect("create workspace");
        let manager = Arc::new(mirri_core::SessionManager::with_persistence_dir(
            workspace,
            tmp.path().join(".mirri").join("sessions"),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        ));
        let state = ServerState::with_engine(
            Arc::new(NoopEngine),
            Arc::clone(&manager),
            "0.1.0-test".to_string(),
            Some("secret".to_string()),
        );

        let mut good = HeaderMap::new();
        good.insert(
            axum::http::header::SEC_WEBSOCKET_PROTOCOL,
            "mirri-code.bearer.secret".parse().unwrap(),
        );
        assert!(bearer_authentication(&state, &good).is_ok());

        let mut bad = HeaderMap::new();
        bad.insert(
            axum::http::header::SEC_WEBSOCKET_PROTOCOL,
            "mirri-code.bearer.wrong".parse().unwrap(),
        );
        assert!(bearer_authentication(&state, &bad).is_err());

        let bare = HeaderMap::new();
        assert!(bearer_authentication(&state, &bare).is_err());

        // Open mode: no subprotocol required.
        let open_state = ServerState::with_engine(
            Arc::new(NoopEngine),
            manager,
            "0.1.0-test".to_string(),
            None,
        );
        assert_eq!(
            bearer_authentication(&open_state, &HeaderMap::new()),
            Ok(None)
        );
    }

    // -----------------------------------------------------------------------
    // Params parsing
    // -----------------------------------------------------------------------

    #[test]
    fn should_parse_subscribe_params_with_optional_cursor() {
        let (session_id, cursor) =
            parse_subscribe_params(&json!({ "sessionId": "session-1" })).expect("parses");
        assert_eq!(session_id, "session-1");
        assert!(cursor.is_none());

        let (_, cursor) = parse_subscribe_params(&json!({
            "sessionId": "session-1",
            "cursor": { "seq": 4, "epoch": 1 },
        }))
        .expect("parses");
        assert_eq!(
            cursor,
            Some(Cursor {
                seq: 4,
                epoch: "1".to_string(),
            })
        );

        assert!(parse_subscribe_params(&json!({ "cursor": { "seq": 1, "epoch": "e" } })).is_err());
        assert!(
            parse_subscribe_params(
                &json!({ "sessionId": "session-1", "cursor": { "seq": "x", "epoch": "e" } })
            )
            .is_err()
        );
    }

    // -----------------------------------------------------------------------
    // mcp.subscribe: the connection-local pool event forwarder (M7, F-9)
    // -----------------------------------------------------------------------

    /// A stub engine whose app-level MCP pool broadcast the test controls
    /// directly, for exercising the `mcp.subscribe` wiring without an
    /// embedded pool behind it.
    struct PoolEventEngine {
        pool_tx: broadcast::Sender<Arc<Value>>,
    }

    impl PoolEventEngine {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                pool_tx: broadcast::channel(64).0,
            })
        }

        fn emit(&self, payload: Value) {
            let _ = self.pool_tx.send(Arc::new(payload));
        }

        fn engine_receivers(&self) -> usize {
            self.pool_tx.receiver_count()
        }
    }

    #[async_trait::async_trait]
    impl EngineAdapter for PoolEventEngine {
        async fn call_method(
            &self,
            method: &str,
            _params: serde_json::Value,
        ) -> Result<Value, mirri_transport::adapter::EngineError> {
            Err(mirri_transport::adapter::EngineError::MethodNotFound(
                method.to_string(),
            ))
        }

        async fn subscribe_events(
            &self,
        ) -> Result<mpsc::Receiver<JsonRpcNotification>, mirri_transport::adapter::EngineError>
        {
            let (_tx, rx) = mpsc::channel(8);
            Ok(rx)
        }

        fn subscribe_mcp_pool_events(
            &self,
        ) -> Result<broadcast::Receiver<Arc<Value>>, mirri_transport::adapter::EngineError>
        {
            Ok(self.pool_tx.subscribe())
        }

        async fn on_reverse_rpc(
            &self,
            _handler: mirri_transport::adapter::ReverseRpcHandler,
        ) -> Result<(), mirri_transport::adapter::EngineError> {
            Ok(())
        }

        async fn dispose(&self) -> Result<(), mirri_transport::adapter::EngineError> {
            Ok(())
        }
    }

    /// Build a [`ServerState`] over the given engine (throwaway workspace),
    /// mirroring the production boot the integration suites use.
    async fn pool_event_state(engine: Arc<dyn EngineAdapter>) -> ServerState {
        let tmp = tempfile::tempdir().expect("temp dir");
        let app = mirri_core::AppScope::new(mirri_core::AppScopeConfig::default());
        let workspace = app
            .create_workspace(
                mirri_core::WorkspaceId::new("mcp-subscribe"),
                tmp.path().to_path_buf(),
            )
            .await
            .expect("create workspace");
        let manager = Arc::new(mirri_core::SessionManager::with_persistence_dir(
            workspace,
            tmp.path().join(".mirri").join("sessions"),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        ));
        ServerState::with_engine(engine, manager, "0.1.0-test".to_string(), None)
    }

    /// Build a [`RpcConnection`] over an mpsc outbox (the writer-task stand-in
    /// `run_connection` spawns in production), and the outbox's receive end.
    fn pool_event_connection(state: ServerState) -> (RpcConnection, mpsc::Receiver<Outbound>) {
        let (outbox_tx, outbox_rx) = mpsc::channel::<Outbound>(16);
        let connection = RpcConnection {
            state,
            outbox: outbox_tx,
            subscriptions: HashMap::new(),
            reverse_rpc: Arc::new(std::sync::Mutex::new(ReverseRpcState::new(Vec::new()))),
            terminal_attachments: HashMap::new(),
            mcp_subscription: None,
            skill_catalog_subscription: None,
            resync_cooldowns: HashMap::new(),
        };
        (connection, outbox_rx)
    }

    /// Read the next frame queued on the connection's outbox (bounded, so a
    /// broken forwarder fails fast instead of hanging the suite).
    async fn next_outbound_frame(outbox_rx: &mut mpsc::Receiver<Outbound>) -> Value {
        let frame = tokio::time::timeout(Duration::from_secs(5), outbox_rx.recv())
            .await
            .expect("outbox frame queued within the timeout")
            .expect("outbox stays open");
        let Outbound::Frame(frame) = frame else {
            panic!("expected a frame");
        };
        frame
    }

    /// Wait until the state-level engine → broadcast forwarder has
    /// subscribed to the engine's pool channel: `mcp_event_rx` spawns it
    /// lazily, and the task only calls `subscribe_mcp_pool_events` on its
    /// first poll — emitting before that would race the subscription.
    async fn await_engine_subscription(engine: &PoolEventEngine) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while engine.engine_receivers() == 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the state-level forwarder never subscribed to the engine broadcast"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Given a connection, when `mcp.subscribe` is called and the engine
    /// publishes pool transitions, then the acknowledgment goes out first,
    /// every event is forwarded exactly once as `event.mcp.server.status`,
    /// and a repeated subscribe re-acknowledges without spawning a second
    /// forwarder (one engine receiver serves them all).
    #[tokio::test]
    async fn should_ack_and_forward_pool_events_when_mcp_subscribe_is_called_and_repeated() {
        // Given a connection over an engine whose pool broadcast the test
        // controls
        let engine = PoolEventEngine::new();
        let state = pool_event_state(Arc::clone(&engine) as Arc<dyn EngineAdapter>).await;
        let (mut connection, mut outbox_rx) = pool_event_connection(state);

        // When the connection subscribes
        connection.subscribe_mcp(10).await;
        await_engine_subscription(&engine).await;

        // Then the honest acknowledgment is queued under the echoed id
        let ack = next_outbound_frame(&mut outbox_rx).await;
        assert_eq!(ack["jsonrpc"], "2.0");
        assert_eq!(ack["id"], 10);
        assert_eq!(ack["result"], json!({ "ok": true }), "ack: {ack}");
        assert!(connection.mcp_subscription.is_some(), "forwarder spawned");

        // When the engine publishes a pool transition
        let payload = json!({
            "server": {
                "name": "mock",
                "transport": "http",
                "status": "connected",
                "toolCount": 2,
                "pool": "user",
            }
        });
        engine.emit(payload.clone());

        // Then it is relayed onto the connection as an
        // `event.mcp.server.status` notification, payload as-is
        let event = next_outbound_frame(&mut outbox_rx).await;
        assert_eq!(event["method"], "event.mcp.server.status");
        assert_eq!(event["params"], payload, "event: {event}");

        // When the connection re-subscribes and the engine publishes again
        connection.subscribe_mcp(11).await;
        let re_ack = next_outbound_frame(&mut outbox_rx).await;
        assert_eq!(re_ack["id"], 11);
        assert_eq!(re_ack["result"], json!({ "ok": true }), "re-ack: {re_ack}");
        let second = json!({ "server": { "name": "mock", "status": "failed" } });
        engine.emit(second.clone());

        // Then exactly one notification is queued — the idempotent
        // re-subscribe spawned no second forwarder
        let event = next_outbound_frame(&mut outbox_rx).await;
        assert_eq!(event["params"], second, "second event: {event}");
        assert!(
            outbox_rx.try_recv().is_err(),
            "no duplicate frame from a second forwarder"
        );

        // And the engine's pool channel carries exactly one receiver — the
        // single state-level forwarder both subscribes share
        assert_eq!(engine.engine_receivers(), 1);
    }

    /// Given a subscribed connection, when the connection's writer goes
    /// away (the outbox closes — the disconnect path `run_connection` also
    /// aborts explicitly), then the forwarder observes the closure and
    /// ends instead of leaking.
    #[tokio::test]
    async fn should_end_the_mcp_forwarder_when_the_connection_outbox_closes() {
        // Given a subscribed connection whose outbox frame end is dropped
        // (the writer task is gone, as at connection teardown)
        let engine = PoolEventEngine::new();
        let state = pool_event_state(Arc::clone(&engine) as Arc<dyn EngineAdapter>).await;
        let (mut connection, outbox_rx) = pool_event_connection(state);
        connection.subscribe_mcp(10).await;
        await_engine_subscription(&engine).await;
        drop(outbox_rx);

        // When the engine publishes a transition (the forwarder's outbox
        // send fails, ending the relay)
        engine.emit(json!({ "server": { "name": "mock", "status": "connected" } }));

        // Then the forwarder task finishes instead of leaking
        let forwarder = connection
            .mcp_subscription
            .take()
            .expect("forwarder was spawned");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if forwarder.is_finished() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "forwarder outlived the connection's writer"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Given a subscribed connection, when the connection ends (the
    /// teardown `run_connection` performs: the forwarder is taken and
    /// aborted), then later pool transitions forward nothing — the aborted
    /// forwarder no longer relays onto the (still-open) outbox.
    #[tokio::test]
    async fn should_forward_nothing_after_the_connection_teardown_aborts_the_forwarder() {
        // Given a subscribed connection with a live outbox
        let engine = PoolEventEngine::new();
        let state = pool_event_state(Arc::clone(&engine) as Arc<dyn EngineAdapter>).await;
        let (mut connection, mut outbox_rx) = pool_event_connection(state);
        connection.subscribe_mcp(10).await;
        await_engine_subscription(&engine).await;
        let _ = next_outbound_frame(&mut outbox_rx).await; // the ack

        // When the connection ends — the same teardown run_connection
        // performs on disconnect (take + abort)
        if let Some(forwarder) = connection.mcp_subscription.take() {
            forwarder.abort();
        }
        engine.emit(json!({ "server": { "name": "mock", "status": "pending" } }));
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Then no frame escapes: the aborted forwarder delivers nothing
        // onto the still-open outbox
        assert!(
            outbox_rx.try_recv().is_err(),
            "an aborted forwarder must not relay events"
        );
    }
}
