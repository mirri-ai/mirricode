//! Session-event plumbing: notification construction
//! (`event_notification`), the per-session relay task
//! (`spawn_session_relay`), and the live-chunk callback.
//!
//! Notifications are built through `mirri-core`'s shared event projection
//! (the same authority `mirri-server` uses): every [`SessionEvent`] maps
//! onto an `event.<type>` method with a flat camelCase payload merged into
//! the envelope params, which carry the session id, volatility, timestamp,
//! and — for durable events only — a per-session `seq` plus the emitting
//! server instance's `epoch`.

use std::sync::Arc;

use chrono::Utc;

use mirri_core::{EventBus, LiveEventCallback, ServerEvent, SessionEvent, event_projection};
use mirri_transport::frame::JsonRpcNotification;

use super::{EmbeddedServer, SessionScope, SessionView, SessionViewSource};
/// Build the `event.<type>` notification for one session event through the
/// shared projection (`mirri_core::event_projection::event_notification`),
/// or `None` when the event has no wire projection
/// ([`SessionEvent::Unknown`]).
///
/// `seq` is the per-session monotonic position of durable events — the
/// documented resync signal for gap detection; no replay path exists in the
/// embedded engine (a subscriber that observes a skipped seq resyncs). The
/// `epoch` is the emitting server instance's id, mirroring the server's
/// instance-id semantics: a change of epoch means the per-session seq
/// counters restarted. Volatile events carry neither (the projection drops
/// them from the envelope).
fn event_notification(
    session_id: &str,
    event: &SessionEvent,
    seq: Option<u64>,
    epoch: Option<&str>,
) -> Option<JsonRpcNotification> {
    event_projection::event_notification(event, session_id, seq, epoch, Utc::now()).map(
        |projected| JsonRpcNotification {
            method: projected.method,
            params: projected.params,
        },
    )
}

/// Build the live-event callback that surfaces incremental assistant text
/// deltas on the session's event bus as [`SessionEvent::AgentMessageChunk`]
/// events, so event subscribers can stream the reply token-by-token.
///
/// The LiveEvent→SessionEvent translation itself lives in
/// `mirri_core::session::live_relay` (the shared authority — the subagent
/// host wires its children through the same mapping); this wrapper only
/// binds it to this session's bus and agent. `turn_id` is the wire turn id
/// (`turn-<uuid>`) the dispatch minted for the turn the callback serves —
/// it rides on the `turn.step.interrupted` / `turn.step.retrying` events.
pub(super) fn live_chunk_callback(
    event_bus: &Arc<EventBus>,
    agent_id: &str,
    session_id: &str,
    turn_id: Option<String>,
) -> LiveEventCallback {
    mirri_core::session::live_relay::live_event_callback(event_bus, agent_id, session_id, turn_id)
}

/// Project one server-scoped event and push it onto the broadcast — the ONE
/// projection point for both emitters: [`EmbeddedServer::emit_server_event`]
/// (which holds `&self`) and the captured-sender tasks that hold no
/// `&EmbeddedServer` (the cron run spawner's fire-and-forget task). A send
/// failure only means "no subscribers" — never fatal.
pub(super) fn send_server_event(
    sender: &tokio::sync::broadcast::Sender<JsonRpcNotification>,
    event: ServerEvent,
) {
    let projected = event_projection::project_server_event(&event);
    let _ = sender.send(JsonRpcNotification {
        method: projected.method,
        params: projected.params,
    });
}

impl EmbeddedServer {
    // --- event plumbing ---------------------------------------------------

    /// Broadcast one server-scoped lifecycle event (a session or workspace
    /// lifecycle change) to every event subscriber. The per-subscriber
    /// forwarders installed by [`EngineAdapter::subscribe_events`] relay the
    /// pre-framed notification onto the subscriber's channel; direct
    /// broadcast consumers (the serve wire's connection-level surface, via
    /// [`EngineAdapter::subscribe_server_events`]) read the same frames.
    ///
    /// Callers emit AFTER the mutation landed — an event must never announce
    /// a change the engine did not commit.
    pub(super) fn emit_server_event(&self, event: ServerEvent) {
        send_server_event(&self.server_events, event);
    }

    /// Announce a freshly created session on the server-scoped stream —
    /// **the single creation seam** every creation path calls (`session.create`,
    /// `session.fork`, `session.createChild`, and the scheduled-task run
    /// spawner), so a connected client adopts the row live instead of
    /// discovering it on its next list refresh (SDR-127's real-time face).
    ///
    /// The payload is [`Self::session_created_row`] — the same LIST ROW
    /// `session.list` composes, so the adopted row and a refreshed row
    /// cannot diverge. Emitted AFTER the session exists and its index row
    /// landed (the announcement must never outrun the state it describes);
    /// a resume deliberately does NOT go through here — re-announcing would
    /// re-adopt and re-order a row the client already holds.
    ///
    /// The value is a plain `serde_json::Value` because the row crosses into
    /// `mirri-core`'s projection, which frames but never interprets it.
    pub(super) fn announce_session_created(&self, row: &SessionView) {
        let Ok(session) = serde_json::to_value(row) else {
            tracing::warn!(
                session_id = %row.id,
                "session.created announcement dropped: the row does not serialize"
            );
            return;
        };
        self.emit_server_event(ServerEvent::SessionCreated { session });
    }

    /// The LIST ROW of a live session — the one row constructor shared by
    /// `session.list` and the created announcement. `last_prompt` comes from
    /// the index row (the live scope carries none, SDR-004), exactly as the
    /// listing's merged entry supplies it.
    pub(super) async fn session_created_row(&self, session: &SessionScope) -> SessionView {
        let last_prompt = self
            .session_index()
            .and_then(|index| index.get_session(session.id().as_str()))
            .and_then(|row| row.last_prompt.clone());
        SessionViewSource::from_server(self)
            .listing_row_of(session, last_prompt)
            .await
    }

    /// Start relaying one session's event bus to every event subscriber.
    ///
    /// Events published before the relay is installed are not replayed (a
    /// cursor log like the WebSocket endpoint's is out of scope for the
    /// embedded engine). A relay exits when its session closes; the session
    /// must be live (have artifacts) when this is called.
    ///
    /// The relay's durable seq continues from the last watermark this
    /// server stamped for the session (a same-process close→resume keeps
    /// the stream monotonic); a first relay starts at 0.
    pub(super) fn spawn_session_relay(&self, session_id: &str) {
        let Ok(artifacts) = self.resolve_session_artifacts(session_id) else {
            tracing::warn!(session_id, "cannot relay events: session has no artifacts");
            return;
        };
        let mut bus_rx = artifacts.event_bus.subscribe();
        let subscribers = Arc::clone(&self.subscribers);
        let relays = Arc::clone(&self.relays);
        let epoch = self.epoch.clone();
        let sid = session_id.to_string();
        let task_sid = sid.clone();
        let watermarks = Arc::clone(&self.relay_seq_watermarks);
        let initial_seq = watermarks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&sid)
            .copied()
            .unwrap_or(0);

        let handle = tokio::spawn(async move {
            // Per-session monotonic position of durable events, incremented
            // before use: the first durable event of a session carries seq 1;
            // a resumed session's relay continues from the watermark its
            // predecessor stamped.
            let mut seq: u64 = initial_seq;
            loop {
                match bus_rx.recv().await {
                    Ok(event) => {
                        let (seq_value, epoch_value) = if event_projection::is_volatile(&event) {
                            // Volatile events are fire-and-forget: no seq,
                            // no epoch (they never occupy cursor space).
                            (None, None)
                        } else {
                            seq += 1;
                            // Record the watermark so a later relay for this
                            // session (a same-process resume) continues the
                            // sequence instead of restarting at 1.
                            watermarks
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .insert(task_sid.clone(), seq);
                            (Some(seq), Some(epoch.as_str()))
                        };
                        // Unknown events have no wire projection — the
                        // shared builder returns None and nothing is
                        // forwarded for them.
                        if let Some(notification) =
                            event_notification(&task_sid, &event, seq_value, epoch_value)
                        {
                            let mut subs = subscribers.lock().unwrap_or_else(|e| e.into_inner());
                            for subscriber in subs.iter() {
                                // Backpressure: a full channel means the
                                // subscriber is too slow — drop the event
                                // for it (it observes a gap, the documented
                                // resync signal). Closed subscribers are
                                // purged.
                                let _ = subscriber.try_send(notification.clone());
                            }
                            subs.retain(|subscriber| !subscriber.is_closed());
                            drop(subs);
                        }
                        if matches!(event, SessionEvent::SessionClosed { .. }) {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        tracing::warn!(session_id = %task_sid, skipped, "session event bus lagged in embedded relay");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
            relays
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&task_sid);
        });
        self.relays
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(sid, handle);
    }
}
