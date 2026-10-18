//! Graceful shutdown for a session: drain all pending gates.
//!
//! When a session closes, every in-flight approval request and user
//! question must be resolved so the engine futures waiting on them are not
//! left hanging. [`SessionShutdown`] drains both gates of the session's
//! [`SessionArtifacts`]; the [`EventBus`] needs no active teardown because
//! the broadcast channel closes by itself once the last `Arc<EventBus>` is
//! released.
//!
//! Draining is strictly per-session: each session owns its own artifact set,
//! so shutting one session down never touches another session's gates.

use std::sync::Arc;

use super::artifacts::SessionArtifacts;
use super::events::EventBus;
use super::gates::{ApprovalGate, QuestionGate};

/// Drains all pending gate requests of one session when it closes.
pub struct SessionShutdown {
    approval_gate: Arc<ApprovalGate>,
    question_gate: Arc<QuestionGate>,
    event_bus: Arc<EventBus>,
}

impl SessionShutdown {
    /// Create a shutdown coordinator over the given session's artifacts.
    pub fn new(artifacts: &SessionArtifacts) -> Self {
        Self {
            approval_gate: Arc::clone(&artifacts.approval_gate),
            question_gate: Arc::clone(&artifacts.question_gate),
            event_bus: Arc::clone(&artifacts.event_bus),
        }
    }

    /// Drain all pending gates, cancelling every pending future.
    ///
    /// Pending approval requests resolve to
    /// [`ApprovalDecision::Timeout`](crate::session::gates::ApprovalDecision::Timeout)
    /// and pending questions return `None`. The event bus needs no explicit
    /// shutdown: it drops naturally once all `Arc<EventBus>` references are
    /// released, after which every subscriber's `recv()` observes
    /// [`RecvError::Closed`](tokio::sync::broadcast::error::RecvError::Closed).
    pub async fn shutdown(&self) {
        tracing::debug!(
            subscribers = self.event_bus.subscriber_count(),
            "session shutdown: draining pending gate requests"
        );
        self.approval_gate.drain().await;
        self.question_gate.drain().await;
    }
}
