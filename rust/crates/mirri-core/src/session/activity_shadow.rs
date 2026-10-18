//! Per-session work-facts fold (US4.4 / SDR-013 / ADR-009).
//!
//! Mirrors [`InFlightTurnShadow`](crate::session::in_flight_shadow::InFlightTurnShadow)'s
//! spawned-task shape: one subscription to the session's event bus folds
//! [`SessionEvent`]s into a [`SessionActivityView`], and every real state
//! transition is published back onto the same bus as
//! [`SessionEvent::SessionWorkChanged`] — durable, fanned out by the existing
//! relays, ordered after the `turn.ended` / other frame that caused it (the
//! fold consumes that frame first, then publishes).

use std::sync::Arc;

use crate::SessionEvent;
use crate::session::events::EventBus;
use crate::session_activity::{
    SessionActivityState, SessionActivityView, SessionPendingInteraction, SessionTurnOutcome,
};

/// The per-session shadow folding the event bus into the work-facts state.
pub struct SessionActivityShadow {
    state: Arc<std::sync::RwLock<SessionActivityState>>,
}

impl std::fmt::Debug for SessionActivityShadow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionActivityShadow")
            .field(
                "state",
                &self
                    .state
                    .read()
                    .map(|s| s.clone())
                    .unwrap_or_else(|_| SessionActivityState::default()),
            )
            .finish()
    }
}

impl SessionActivityShadow {
    /// Create the shadow and start folding the given bus's events into it.
    pub fn spawn(event_bus: &Arc<EventBus>) -> Arc<Self> {
        let shadow = Arc::new(Self {
            state: Arc::new(std::sync::RwLock::new(SessionActivityState::default())),
        });
        if tokio::runtime::Handle::try_current().is_err() {
            tracing::debug!("activity shadow not started: no tokio runtime");
            return shadow;
        }
        let mut receiver = event_bus.subscribe();
        let view = Arc::new(std::sync::Mutex::new(SessionActivityView::new()));
        let task_shadow = Arc::clone(&shadow);
        let bus = Arc::clone(event_bus);
        tokio::spawn(async move {
            loop {
                match receiver.recv().await {
                    Ok(event) => {
                        let next = Self::apply(&mut view.lock().unwrap(), &event);
                        if let Some(state) = next {
                            *task_shadow.state.write().unwrap() = state.clone();
                            bus.publish(SessionEvent::SessionWorkChanged { state });
                        }
                        if matches!(event, SessionEvent::SessionClosed { .. }) {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        tracing::warn!("activity shadow lagged; resetting");
                        let mut v = view.lock().unwrap();
                        *v = SessionActivityView::new();
                        *task_shadow.state.write().unwrap() = SessionActivityState::default();
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        shadow
    }

    /// The current work-facts state snapshot.
    pub fn snapshot_value(&self) -> SessionActivityState {
        self.state.read().unwrap().clone()
    }

    /// Fold one event into the view; `Some(next)` when a real state
    /// transition happened (and should be published back onto the bus as
    /// `SessionWorkChanged`), `None` for a no-op / explicitly-ignored event.
    ///
    /// `SessionWorkChanged` frames are ignored on purpose — the fold is the
    /// state authority, the published event is only a projection, so a
    /// self-published transition frame must never re-enter the fold.
    fn apply(view: &mut SessionActivityView, event: &SessionEvent) -> Option<SessionActivityState> {
        match event {
            // Self-loop guard: the shadow's own published projections do not
            // feed back into the fold (it is the state authority).
            SessionEvent::SessionWorkChanged { .. } => None,
            SessionEvent::TurnStarted { agent_id, .. } if agent_id == MAIN_AGENT => {
                let before = view.state();
                view.on_agent_turn_started();
                changed(view, &before)
            }
            SessionEvent::TurnStarted { .. } => None,
            SessionEvent::TurnCompleted {
                agent_id,
                stop_reason,
                ..
            } if agent_id == MAIN_AGENT => {
                let before = view.state();
                view.on_agent_turn_ended(outcome_of(stop_reason));
                changed(view, &before)
            }
            SessionEvent::TurnCompleted { .. } => None,
            SessionEvent::TurnFailed {
                agent_id,
                error_code,
                ..
            } if agent_id == MAIN_AGENT => {
                let before = view.state();
                view.on_agent_turn_ended(outcome_of_failure(error_code));
                changed(view, &before)
            }
            SessionEvent::TurnFailed { .. } => None,
            SessionEvent::ToolApprovalRequested { .. } => {
                let before = view.state();
                view.on_interaction_pending(SessionPendingInteraction::Approval);
                changed(view, &before)
            }
            SessionEvent::ToolApprovalResolved { .. } => {
                let before = view.state();
                view.on_interaction_pending(SessionPendingInteraction::None);
                changed(view, &before)
            }
            SessionEvent::QuestionAsked { .. } => {
                let before = view.state();
                view.on_interaction_pending(SessionPendingInteraction::Question);
                changed(view, &before)
            }
            SessionEvent::QuestionAnswered { .. } | SessionEvent::QuestionDismissed { .. } => {
                let before = view.state();
                view.on_interaction_pending(SessionPendingInteraction::None);
                changed(view, &before)
            }
            SessionEvent::BackgroundTaskStarted { .. } => {
                let before = view.state();
                view.on_background_added();
                changed(view, &before)
            }
            SessionEvent::BackgroundTaskTerminated { .. } => {
                let before = view.state();
                view.on_background_removed(false);
                changed(view, &before)
            }
            _ => None,
        }
    }
}

/// The fold's dedup gate: `Some(next)` only when the view's state actually
/// transitioned — the inner `SessionActivityView::commit` is already a
/// no-op on an unchanged tuple, but a redundant input (a second
/// `TurnStarted` mid-turn, a resolved interaction with none pending) must
/// not publish a durable `SessionWorkChanged` frame that burns a seq slot
/// for no observable change.
fn changed(
    view: &SessionActivityView,
    before: &SessionActivityState,
) -> Option<SessionActivityState> {
    let next = view.state();
    (next != *before).then_some(next)
}

/// The agent identity the session-level work facts fold tracks. Matches the
/// `in_flight_shadow`'s [`MAIN_AGENT`](crate::session::in_flight_shadow::MAIN_AGENT)
/// scoping: subagent turns do not drive the main conversation's activity
/// quad (v2 口径 — the main dialogue turn).
const MAIN_AGENT: &str = "main";

/// Map a turn `stop_reason` wire string onto the session turn outcome:
/// `completed` → Completed, `cancelled` → Cancelled, `stalled` → Stalled
/// (ADR-004), everything else (`blocked` / `filtered` / `max_tokens` / …)
/// → Failed.
fn outcome_of(stop_reason: &str) -> SessionTurnOutcome {
    match stop_reason {
        "completed" => SessionTurnOutcome::Completed,
        "cancelled" => SessionTurnOutcome::Cancelled,
        "stalled" => SessionTurnOutcome::Stalled,
        _ => SessionTurnOutcome::Failed,
    }
}

/// Map a failed turn's error code onto the outcome: an engine-side abort
/// (`error_code` prefixed `abort.` — e.g. `abort.user_cancelled` published
/// by the `message.abort` dispatch) is a user-initiated cancellation and
/// surfaces as Cancelled; every other failure stays Failed.
fn outcome_of_failure(error_code: &str) -> SessionTurnOutcome {
    if error_code.starts_with("abort.") {
        SessionTurnOutcome::Cancelled
    } else {
        SessionTurnOutcome::Failed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::broadcast::error::RecvError;

    fn bus() -> Arc<EventBus> {
        Arc::new(EventBus::new(64))
    }

    /// Read from `rx` until a `SessionWorkChanged` event arrives, skipping
    /// the input frames that precede it on the bus.
    async fn recv_work_changed(
        rx: &mut tokio::sync::broadcast::Receiver<SessionEvent>,
    ) -> SessionActivityState {
        loop {
            match rx.recv().await {
                Ok(SessionEvent::SessionWorkChanged { state }) => return state,
                Ok(_) => continue,
                Err(RecvError::Lagged(_)) | Err(RecvError::Closed) => panic!("stream ended"),
            }
        }
    }

    #[tokio::test]
    async fn should_publish_work_changed_and_flip_state_when_a_main_turn_starts_and_ends() {
        let bus = bus();
        let shadow = SessionActivityShadow::spawn(&bus);
        // Subscribe after spawn so this receiver observes the published
        // transition frames on top of the input frames.
        let mut rx = bus.subscribe();

        bus.publish(SessionEvent::TurnStarted {
            agent_id: "main".to_string(),
            session_id: "sess-1".to_string(),
            turn_id: "turn-1".to_string(),
        });
        let start = recv_work_changed(&mut rx).await;
        assert!(start.main_turn_active, "turn start flips the main gate on");
        assert!(start.busy, "turn start marks the session busy");
        assert!(shadow.snapshot_value().main_turn_active);

        bus.publish(SessionEvent::TurnCompleted {
            agent_id: "main".to_string(),
            session_id: "sess-1".to_string(),
            turn_id: "turn-1".to_string(),
            stop_reason: "completed".to_string(),
            wire_turn_id: 1,
            duration_ms: None,
        });
        let end = recv_work_changed(&mut rx).await;
        assert!(!end.main_turn_active, "turn end clears the main gate");
        assert_eq!(
            end.last_turn_reason,
            Some(SessionTurnOutcome::Completed),
            "completed stop reason records a completed outcome"
        );
        assert!(!shadow.snapshot_value().main_turn_active);
    }

    #[tokio::test]
    async fn should_not_flip_main_turn_when_a_subagent_turn_runs() {
        let bus = bus();
        let shadow = SessionActivityShadow::spawn(&bus);
        bus.publish(SessionEvent::TurnStarted {
            agent_id: "agent-<uuid>".to_string(),
            session_id: "sess-1".to_string(),
            turn_id: "turn-2".to_string(),
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let state = shadow.snapshot_value();
        assert!(
            !state.main_turn_active,
            "subagent turn is not the main turn"
        );
        assert!(!state.busy, "subagent turn does not light the session busy");
    }

    #[tokio::test]
    async fn should_not_publish_work_changed_when_a_redundant_input_changes_nothing() {
        let bus = bus();
        let shadow = SessionActivityShadow::spawn(&bus);
        let mut rx = bus.subscribe();

        bus.publish(SessionEvent::TurnStarted {
            agent_id: "main".to_string(),
            session_id: "sess-1".to_string(),
            turn_id: "turn-1".to_string(),
        });
        let start = recv_work_changed(&mut rx).await;
        assert!(start.main_turn_active);

        // A second TurnStarted mid-turn is a no-op input: the fold must NOT
        // publish another work_changed frame for the unchanged tuple. The
        // input frame itself IS on the bus — drain frames until quiet and
        // assert no work_changed ever arrives.
        bus.publish(SessionEvent::TurnStarted {
            agent_id: "main".to_string(),
            session_id: "sess-1".to_string(),
            turn_id: "turn-1b".to_string(),
        });
        let silence_deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(150);
        loop {
            let next = match tokio::time::timeout_at(silence_deadline, rx.recv()).await {
                Ok(Ok(event)) => event,
                Ok(Err(_)) | Err(_) => break, // channel drained / timeout: quiet
            };
            assert!(
                !matches!(next, SessionEvent::SessionWorkChanged { .. }),
                "a redundant turn-start must not publish a work_changed frame"
            );
        }
        assert!(shadow.snapshot_value().main_turn_active);

        // The next real transition still publishes.
        bus.publish(SessionEvent::TurnCompleted {
            agent_id: "main".to_string(),
            session_id: "sess-1".to_string(),
            turn_id: "turn-1b".to_string(),
            stop_reason: "completed".to_string(),
            wire_turn_id: 2,
            duration_ms: None,
        });
        let end = recv_work_changed(&mut rx).await;
        assert!(!end.main_turn_active);
    }

    #[tokio::test]
    async fn should_surface_an_aborted_turn_as_cancelled_not_failed() {
        let bus = bus();
        let shadow = SessionActivityShadow::spawn(&bus);
        let mut rx = bus.subscribe();

        bus.publish(SessionEvent::TurnStarted {
            agent_id: "main".to_string(),
            session_id: "sess-1".to_string(),
            turn_id: "turn-3".to_string(),
        });
        let _ = recv_work_changed(&mut rx).await;

        // The message.abort dispatch closes the bracket with a TurnFailed
        // carrying `abort.user_cancelled` — a user-initiated cancellation.
        bus.publish(SessionEvent::TurnFailed {
            agent_id: "main".to_string(),
            turn_id: "turn-3".to_string(),
            error: "turn aborted by the user via message.abort".to_string(),
            error_code: "abort.user_cancelled".to_string(),
            wire_turn_id: 3,
        });
        let end = recv_work_changed(&mut rx).await;
        assert_eq!(
            end.last_turn_reason,
            Some(SessionTurnOutcome::Cancelled),
            "an engine-side abort (error_code prefixed abort.) is a user cancellation"
        );
        assert!(!shadow.snapshot_value().main_turn_active);
    }

    #[tokio::test]
    async fn should_surface_a_genuine_turn_failure_as_failed() {
        let bus = bus();
        let _shadow = SessionActivityShadow::spawn(&bus);
        let mut rx = bus.subscribe();

        bus.publish(SessionEvent::TurnStarted {
            agent_id: "main".to_string(),
            session_id: "sess-1".to_string(),
            turn_id: "turn-4".to_string(),
        });
        let _ = recv_work_changed(&mut rx).await;

        bus.publish(SessionEvent::TurnFailed {
            agent_id: "main".to_string(),
            turn_id: "turn-4".to_string(),
            error: "provider unavailable".to_string(),
            error_code: "provider.unavailable".to_string(),
            wire_turn_id: 4,
        });
        let end = recv_work_changed(&mut rx).await;
        assert_eq!(
            end.last_turn_reason,
            Some(SessionTurnOutcome::Failed),
            "a non-abort failure stays Failed"
        );
    }
}
