//! Per-session event plumbing: the event bus and gates owned by one session.
//!
//! Every session owns its own [`EventBus`], [`ApprovalGate`], and
//! [`QuestionGate`]. The gates publish their request/response events on the
//! session's own bus, so subscribers (WebSocket streams, RPC responders) only
//! ever observe events from the session they subscribed to — events from one
//! session never leak into another session's stream.
//!
//! [`SessionManager`](crate::session::manager::SessionManager) creates a
//! fresh `SessionArtifacts` when a session is created or resumed, hands clones
//! out through [`session_artifacts`](crate::session::manager::SessionManager::session_artifacts),
//! and drains them through
//! [`SessionShutdown`](crate::session::shutdown::SessionShutdown) when the
//! session closes. The same lifecycle owns the session's **turn-id
//! allocator** ([`Self::turn_id_counter`]): seeded from the replayed wire
//! log's maximum at resume, handed to every turn flow the session creates,
//! and discarded with the artifacts at close — a fresh counter never
//! inherits a dead session's numbering.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use super::activity_shadow::SessionActivityShadow;
use super::events::{DEFAULT_EVENT_BUS_CAPACITY, EventBus};
use super::gates::{ApprovalGate, QuestionGate};
use super::in_flight_shadow::InFlightTurnShadow;
use crate::reverse_rpc::ReverseRpcCallback;

/// The event plumbing owned by a single session.
///
/// Cloning is cheap (every field is an `Arc`), so a clone can be handed to
/// each consumer (RPC handler, WebSocket stream, test harness) that needs to
/// publish events or respond to gate requests for that session.
pub struct SessionArtifacts {
    /// The session's broadcast channel for [`SessionEvent`]s.
    pub event_bus: Arc<EventBus>,
    /// The session's tool-approval gate. External responders resolve pending
    /// requests through [`ApprovalGate::resolve`] with the `request_id`
    /// carried by the `ToolApprovalRequested` event on [`event_bus`](Self::event_bus).
    pub approval_gate: Arc<ApprovalGate>,
    /// The session's question gate. External responders answer pending
    /// questions through [`QuestionGate::answer`] with the `request_id`
    /// carried by the `QuestionAsked` event on [`event_bus`](Self::event_bus).
    pub question_gate: Arc<QuestionGate>,
    /// The session's in-flight-turn shadow — the volatile stream accumulator
    /// `session.snapshot`'s `in_flight_turn` reads (mid-turn reconnect
    /// rebuilds the half-finished reply). A derived cache over the same bus,
    /// never a second source of truth.
    pub in_flight_shadow: Arc<InFlightTurnShadow>,
    /// The session's work-facts shadow — folds the same bus into the
    /// `session.work_changed` activity quad and publishes every real
    /// transition back onto it as `SessionWorkChanged` (US4.4 / SDR-013 /
    /// ADR-009). Also a derived cache, never a second source of truth.
    pub activity_shadow: Arc<SessionActivityShadow>,
    /// Reverse-RPC callback set by the transport layer when a client
    /// subscribes. The engine reads this to issue `approval.ask` /
    /// `question.ask` requests back to the client.
    pub reverse_rpc_callback: Arc<RwLock<Option<Arc<dyn ReverseRpcCallback>>>>,
    /// The session-scoped turn-id allocator — the single numeric namespace
    /// every turn flow of the session (main agent, cron/task turns,
    /// subagents) draws its `turn.ended` ids from. Holds the NEXT id to hand
    /// out: starts at 1 on a fresh create, and a resume seeds it past the
    /// replayed wire log's maximum (see [`Self::bump_turn_id_floor`]) so no
    /// id the log already holds is minted again. Flow disposal/rebuild never
    /// touches it — the namespace lives with the session, not with any one
    /// [`crate::TurnFlow`].
    turn_id_counter: Arc<AtomicU64>,
}

impl Clone for SessionArtifacts {
    fn clone(&self) -> Self {
        Self {
            event_bus: Arc::clone(&self.event_bus),
            approval_gate: Arc::clone(&self.approval_gate),
            question_gate: Arc::clone(&self.question_gate),
            in_flight_shadow: Arc::clone(&self.in_flight_shadow),
            activity_shadow: Arc::clone(&self.activity_shadow),
            reverse_rpc_callback: Arc::clone(&self.reverse_rpc_callback),
            turn_id_counter: Arc::clone(&self.turn_id_counter),
        }
    }
}

impl std::fmt::Debug for SessionArtifacts {
    /// Manual `Debug`: `dyn ReverseRpcCallback` has no `Debug` impl, so print
    /// only whether a callback is attached.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionArtifacts")
            .field("event_bus", &self.event_bus)
            .field("approval_gate", &self.approval_gate)
            .field("question_gate", &self.question_gate)
            .field("in_flight_shadow", &self.in_flight_shadow)
            .field("activity_shadow", &self.activity_shadow)
            .field(
                "reverse_rpc_callback",
                &self
                    .reverse_rpc_callback
                    .read()
                    .map(|guard| guard.is_some())
                    .unwrap_or(false),
            )
            .field(
                "turn_id_counter",
                &self.turn_id_counter.load(Ordering::Relaxed),
            )
            .finish()
    }
}

impl SessionArtifacts {
    /// Create a fresh, empty artifact set: a new event bus with the two gates
    /// wired to it (approval gate in interactive, non-auto-approve mode),
    /// the in-flight-turn shadow folding the same bus, no reverse-RPC
    /// callback attached, and a turn-id allocator starting at 1.
    ///
    /// A session must never share artifacts with another session — that would
    /// leak events and gate requests across session boundaries.
    pub fn new() -> Self {
        let event_bus = Arc::new(EventBus::new(DEFAULT_EVENT_BUS_CAPACITY));
        Self {
            approval_gate: Arc::new(ApprovalGate::with_event_bus(false, Arc::clone(&event_bus))),
            question_gate: Arc::new(QuestionGate::with_event_bus(Arc::clone(&event_bus))),
            in_flight_shadow: InFlightTurnShadow::spawn(&event_bus),
            activity_shadow: SessionActivityShadow::spawn(&event_bus),
            reverse_rpc_callback: Arc::new(RwLock::new(None)),
            turn_id_counter: Arc::new(AtomicU64::new(1)),
            event_bus,
        }
    }

    /// [`Self::new`], but the reverse-RPC callback slot is the caller's
    /// handle instead of a fresh empty one.
    ///
    /// [`SessionScope`](crate::scope::SessionScope) uses this at artifacts
    /// installation so the artifacts' `reverse_rpc_callback` and the
    /// session's agents' tool-side slot are ONE shared handle: the
    /// transport's subscribe-time install reaches both views at once, so an
    /// agent created before the first subscribe still resolves the callback
    /// once one connects.
    pub fn with_reverse_rpc_slot(
        reverse_rpc_callback: Arc<RwLock<Option<Arc<dyn ReverseRpcCallback>>>>,
    ) -> Self {
        let event_bus = Arc::new(EventBus::new(DEFAULT_EVENT_BUS_CAPACITY));
        Self {
            approval_gate: Arc::new(ApprovalGate::with_event_bus(false, Arc::clone(&event_bus))),
            question_gate: Arc::new(QuestionGate::with_event_bus(Arc::clone(&event_bus))),
            in_flight_shadow: InFlightTurnShadow::spawn(&event_bus),
            activity_shadow: SessionActivityShadow::spawn(&event_bus),
            reverse_rpc_callback,
            turn_id_counter: Arc::new(AtomicU64::new(1)),
            event_bus,
        }
    }

    /// The shared handle of the session's turn-id allocator — the source
    /// every turn flow of the session wires via
    /// [`crate::TurnFlow::set_turn_id_source`] so its `turn.ended` ids
    /// continue the session's single numeric namespace.
    pub fn turn_id_counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.turn_id_counter)
    }

    /// The next turn id the allocator will hand out (the counter's current
    /// value). Read face for tests and diagnostics — allocation goes through
    /// the flows' `fetch_add`, never through this method.
    pub fn next_turn_id(&self) -> u64 {
        self.turn_id_counter.load(Ordering::Acquire)
    }

    /// Raise the turn-id allocator's floor: after this call the next handed
    /// out id is at least `max_observed + 1`. Monotonic (never lowers the
    /// counter) and idempotent, so a resume can seed once for the main wire
    /// log and again for each replayed subagent journal that minted higher.
    pub fn bump_turn_id_floor(&self, max_observed: u64) {
        // saturating: a corrupted log can carry u64::MAX; the next id then
        // sticks at the ceiling rather than wrapping to 0.
        let floor = max_observed.saturating_add(1);
        self.turn_id_counter.fetch_max(floor, Ordering::AcqRel);
    }
}

impl Default for SessionArtifacts {
    fn default() -> Self {
        Self::new()
    }
}
