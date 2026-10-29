//! Session-level aggregated activity projection.
//!
//! Ports the TypeScript `sessionActivity` domain. [`SessionActivityView`]
//! aggregates per-agent activity into a session-level view: `busy` (any
//! agent with an active turn or live background work), the main agent's
//! turn activity, the session's pending-interaction slice, and the latest
//! turn outcome. Change notifications carry the domain `cause` so consumers
//! can schedule their own rendering around related facts.
//!
//! The view owns no authoritative state and can be discarded and rebuilt at
//! any time. Bound at Session scope.

use serde::{Deserialize, Serialize};

/// The kind of pending human interaction, if any.
///
/// Maps to the TypeScript `SessionPendingInteraction` union:
/// `'none' | 'approval' | 'question'`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPendingInteraction {
    /// No interaction is pending.
    None,
    /// A tool-approval request is pending.
    Approval,
    /// A user question is pending.
    Question,
}

/// The outcome of a completed turn, as seen at the session level.
///
/// Maps to the TypeScript `SessionTurnOutcome` union:
/// `'completed' | 'cancelled' | 'failed'` (plus the engine-side `stalled`
/// and `interrupted` presentation values).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionTurnOutcome {
    /// The turn completed normally.
    Completed,
    /// The turn was cancelled.
    Cancelled,
    /// The turn failed.
    Failed,
    /// The turn was terminated by the stall watchdog (ADR-004): no
    /// progress within the inactivity window. Distinct from a user
    /// cancel — the session-level tag renders its own word.
    Stalled,
    /// The turn was cut off by a service exit (interrupted-task
    /// recovery, spec §2.1): the synthetic `turn.ended{interrupted}`
    /// closure's own presentation value — never folded into the failed
    /// family, never degraded to completed.
    Interrupted,
}

/// The session-level activity state — the aggregate snapshot returned by
/// [`SessionActivityView::state`].
///
/// Maps to the TypeScript `SessionActivityState` interface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionActivityState {
    /// Whether any agent has an active turn or live background work.
    pub busy: bool,
    /// Whether the main agent's turn is active.
    pub main_turn_active: bool,
    /// The kind of pending human interaction, if any.
    pub pending_interaction: SessionPendingInteraction,
    /// The outcome of the main agent's most recent turn, if one has
    /// completed.
    pub last_turn_reason: Option<SessionTurnOutcome>,
}

impl Default for SessionActivityState {
    fn default() -> Self {
        Self {
            busy: false,
            main_turn_active: false,
            pending_interaction: SessionPendingInteraction::None,
            last_turn_reason: None,
        }
    }
}

/// What caused a session-activity state change.
///
/// Maps to the TypeScript `SessionActivityCause` union. Consumers use the
/// cause to schedule their own rendering around related facts (e.g. only
/// re-render the turn indicator when `TurnStarted` or `TurnEnded` fires).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionActivityCause {
    /// A turn started (agent began working).
    TurnStarted,
    /// A turn ended (agent finished working).
    TurnEnded,
    /// Background work was added or removed.
    Background,
    /// The pending-interaction set changed.
    Interaction,
    /// An agent was created or disposed.
    AgentLifecycle,
}

/// A change event emitted when the session activity state transitions.
///
/// Maps to the TypeScript `SessionActivityChangedEvent` interface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionActivityChangedEvent {
    /// The new state after the change.
    pub state: SessionActivityState,
    /// What caused the change.
    pub cause: SessionActivityCause,
}

// ---------------------------------------------------------------------------
// SessionActivityView
// ---------------------------------------------------------------------------

/// Session-level aggregated activity view.
///
/// Folds per-agent activity and interaction state into a session-level
/// aggregate. The view owns no authoritative state — it is a pure
/// projection that can be discarded and rebuilt at any time. Change events
/// are buffered internally and retrieved via
/// [`take_pending_changes`](Self::take_pending_changes).
///
/// # Example
///
/// ```
/// use mirri_core::session_activity::{
///     SessionActivityCause, SessionActivityView, SessionTurnOutcome,
/// };
///
/// let mut view = SessionActivityView::new();
/// view.on_agent_turn_started();
/// let changes = view.take_pending_changes();
/// assert_eq!(changes.len(), 1);
/// assert_eq!(changes[0].cause, SessionActivityCause::TurnStarted);
/// ```
#[derive(Debug)]
pub struct SessionActivityView {
    /// Current aggregated state.
    current: SessionActivityState,
    /// Whether any background work is currently tracked. Used to compute
    /// `busy` when the main turn is not active.
    has_background: bool,
    /// Buffered change events for consumers.
    pending_changes: Vec<SessionActivityChangedEvent>,
}

impl SessionActivityView {
    /// Create a new session activity view in the idle state (not busy, no
    /// active turn, no pending interaction, no last-turn outcome).
    pub fn new() -> Self {
        Self {
            current: SessionActivityState::default(),
            has_background: false,
            pending_changes: Vec::new(),
        }
    }

    /// The current activity state snapshot (a cheap clone of plain data).
    pub fn state(&self) -> SessionActivityState {
        self.current.clone()
    }

    /// Record that the main agent's turn has started.
    ///
    /// Sets `busy=true` and `main_turn_active=true`. Emits a change with
    /// cause [`SessionActivityCause::TurnStarted`] — but only if the state
    /// actually changed (e.g. a second `on_agent_turn_started` with no
    /// intervening `on_agent_turn_ended` is a no-op).
    pub fn on_agent_turn_started(&mut self) {
        let next = SessionActivityState {
            busy: true,
            main_turn_active: true,
            pending_interaction: self.current.pending_interaction,
            last_turn_reason: self.current.last_turn_reason,
        };
        self.commit(next, SessionActivityCause::TurnStarted);
    }

    /// Record that the main agent's turn has ended.
    ///
    /// Updates `last_turn_reason` to the given outcome and sets
    /// `main_turn_active=false`. `busy` becomes `false` unless background
    /// work is still tracked. Emits a change with cause
    /// [`SessionActivityCause::TurnEnded`] — but only if the state actually
    /// changed.
    pub fn on_agent_turn_ended(&mut self, outcome: SessionTurnOutcome) {
        let busy = self.has_background;
        let next = SessionActivityState {
            busy,
            main_turn_active: false,
            pending_interaction: self.current.pending_interaction,
            last_turn_reason: Some(outcome),
        };
        self.commit(next, SessionActivityCause::TurnEnded);
    }

    /// Record that background work has been added.
    ///
    /// Sets `has_background=true` and `busy=true` (if not already). Emits a
    /// change with cause [`SessionActivityCause::Background`] — but only if
    /// the state actually changed.
    pub fn on_background_added(&mut self) {
        self.has_background = true;
        let next = SessionActivityState {
            busy: true,
            main_turn_active: self.current.main_turn_active,
            pending_interaction: self.current.pending_interaction,
            last_turn_reason: self.current.last_turn_reason,
        };
        self.commit(next, SessionActivityCause::Background);
    }

    /// Record that background work has been removed.
    ///
    /// The caller passes `has_background` to indicate whether any background
    /// work remains after the removal. Updates `busy` accordingly (`false`
    /// unless the main turn is still active). Emits a change with cause
    /// [`SessionActivityCause::Background`] — but only if the state actually
    /// changed.
    pub fn on_background_removed(&mut self, has_background: bool) {
        self.has_background = has_background;
        let busy = has_background || self.current.main_turn_active;
        let next = SessionActivityState {
            busy,
            main_turn_active: self.current.main_turn_active,
            pending_interaction: self.current.pending_interaction,
            last_turn_reason: self.current.last_turn_reason,
        };
        self.commit(next, SessionActivityCause::Background);
    }

    /// Record that a human interaction is pending.
    ///
    /// Sets `pending_interaction` to the given kind. Emits a change with
    /// cause [`SessionActivityCause::Interaction`] — but only if the state
    /// actually changed (e.g. setting `Approval` when `Approval` is already
    /// pending is a no-op).
    pub fn on_interaction_pending(&mut self, kind: SessionPendingInteraction) {
        let next = SessionActivityState {
            busy: self.current.busy,
            main_turn_active: self.current.main_turn_active,
            pending_interaction: kind,
            last_turn_reason: self.current.last_turn_reason,
        };
        self.commit(next, SessionActivityCause::Interaction);
    }

    /// Record that the pending interaction has been resolved.
    ///
    /// Sets `pending_interaction` to [`SessionPendingInteraction::None`].
    /// Emits a change with cause [`SessionActivityCause::Interaction`] —
    /// but only if the state actually changed.
    pub fn on_interaction_resolved(&mut self) {
        let next = SessionActivityState {
            busy: self.current.busy,
            main_turn_active: self.current.main_turn_active,
            pending_interaction: SessionPendingInteraction::None,
            last_turn_reason: self.current.last_turn_reason,
        };
        self.commit(next, SessionActivityCause::Interaction);
    }

    /// Record that an agent was created or disposed.
    ///
    /// Recomputes `busy` from the current `main_turn_active` and
    /// `has_background` flags. Emits a change with cause
    /// [`SessionActivityCause::AgentLifecycle`] — but only if the state
    /// actually changed.
    pub fn on_agent_lifecycle_change(&mut self) {
        let busy = self.current.main_turn_active || self.has_background;
        let next = SessionActivityState {
            busy,
            main_turn_active: self.current.main_turn_active,
            pending_interaction: self.current.pending_interaction,
            last_turn_reason: self.current.last_turn_reason,
        };
        self.commit(next, SessionActivityCause::AgentLifecycle);
    }

    /// Drain and return all buffered change events.
    ///
    /// After this call, the internal buffer is empty. Consumers should call
    /// this periodically to process state transitions.
    pub fn take_pending_changes(&mut self) -> Vec<SessionActivityChangedEvent> {
        std::mem::take(&mut self.pending_changes)
    }

    /// Commit a new state if it differs from the current one.
    ///
    /// If the state changed, updates `current` and pushes a
    /// [`SessionActivityChangedEvent`] onto the pending buffer. If the state
    /// is unchanged, no event is emitted (this mirrors the TypeScript
    /// `activityEquals` guard).
    fn commit(&mut self, next: SessionActivityState, cause: SessionActivityCause) {
        if self.current != next {
            self.current = next;
            self.pending_changes.push(SessionActivityChangedEvent {
                state: self.current.clone(),
                cause,
            });
        }
    }
}

impl Default for SessionActivityView {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_return_idle_state_when_queried_from_new_view() {
        let view = SessionActivityView::new();

        let state = view.state();
        assert!(!state.busy);
        assert!(!state.main_turn_active);
        assert_eq!(state.pending_interaction, SessionPendingInteraction::None);
        assert!(state.last_turn_reason.is_none());
    }

    #[test]
    fn should_become_busy_with_turn_started_cause_when_turn_starts_from_idle() {
        let mut view = SessionActivityView::new();

        view.on_agent_turn_started();

        let state = view.state();
        assert!(state.busy);
        assert!(state.main_turn_active);

        let changes = view.take_pending_changes();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].cause, SessionActivityCause::TurnStarted);
        assert!(changes[0].state.busy);
        assert!(changes[0].state.main_turn_active);
    }

    #[test]
    fn should_clear_busy_and_record_completed_outcome_when_turn_ends_from_active() {
        let mut view = SessionActivityView::new();
        view.on_agent_turn_started();
        view.take_pending_changes(); // drain the turn_started event

        view.on_agent_turn_ended(SessionTurnOutcome::Completed);

        let state = view.state();
        assert!(!state.busy);
        assert!(!state.main_turn_active);
        assert_eq!(state.last_turn_reason, Some(SessionTurnOutcome::Completed));

        let changes = view.take_pending_changes();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].cause, SessionActivityCause::TurnEnded);
    }

    #[test]
    fn should_record_cancelled_outcome_when_turn_ends_from_active() {
        let mut view = SessionActivityView::new();
        view.on_agent_turn_started();
        view.take_pending_changes();

        view.on_agent_turn_ended(SessionTurnOutcome::Cancelled);

        assert_eq!(
            view.state().last_turn_reason,
            Some(SessionTurnOutcome::Cancelled)
        );
    }

    #[test]
    fn should_record_failed_outcome_when_turn_ends_from_active() {
        let mut view = SessionActivityView::new();
        view.on_agent_turn_started();
        view.take_pending_changes();

        view.on_agent_turn_ended(SessionTurnOutcome::Failed);

        assert_eq!(
            view.state().last_turn_reason,
            Some(SessionTurnOutcome::Failed)
        );
    }

    #[test]
    fn should_become_busy_with_background_cause_when_background_added_from_idle() {
        let mut view = SessionActivityView::new();

        view.on_background_added();

        let state = view.state();
        assert!(state.busy);
        assert!(!state.main_turn_active);

        let changes = view.take_pending_changes();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].cause, SessionActivityCause::Background);
    }

    #[test]
    fn should_clear_busy_when_last_background_work_is_removed() {
        let mut view = SessionActivityView::new();
        view.on_background_added();
        view.take_pending_changes();

        view.on_background_removed(false);

        assert!(!view.state().busy);
        let changes = view.take_pending_changes();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].cause, SessionActivityCause::Background);
    }

    #[test]
    fn should_stay_busy_when_background_is_removed_while_turn_active() {
        let mut view = SessionActivityView::new();
        view.on_agent_turn_started();
        view.on_background_added();
        view.take_pending_changes();

        view.on_background_removed(false);

        // Turn is still active, so busy stays true.
        assert!(view.state().busy);
        assert!(view.state().main_turn_active);
    }

    #[test]
    fn should_stay_busy_when_turn_ends_while_background_active() {
        let mut view = SessionActivityView::new();
        view.on_agent_turn_started();
        view.on_background_added();
        view.take_pending_changes();

        view.on_agent_turn_ended(SessionTurnOutcome::Completed);

        // Background work is still tracked, so busy stays true.
        assert!(view.state().busy);
        assert!(!view.state().main_turn_active);
    }

    #[test]
    fn should_record_approval_pending_when_approval_interaction_becomes_pending() {
        let mut view = SessionActivityView::new();

        view.on_interaction_pending(SessionPendingInteraction::Approval);

        let state = view.state();
        assert_eq!(
            state.pending_interaction,
            SessionPendingInteraction::Approval
        );

        let changes = view.take_pending_changes();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].cause, SessionActivityCause::Interaction);
    }

    #[test]
    fn should_clear_pending_interaction_when_interaction_is_resolved() {
        let mut view = SessionActivityView::new();
        view.on_interaction_pending(SessionPendingInteraction::Approval);
        view.take_pending_changes();

        view.on_interaction_resolved();

        assert_eq!(
            view.state().pending_interaction,
            SessionPendingInteraction::None
        );
        let changes = view.take_pending_changes();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].cause, SessionActivityCause::Interaction);
    }

    #[test]
    fn should_emit_no_change_event_when_same_interaction_becomes_pending_again() {
        let mut view = SessionActivityView::new();
        view.on_interaction_pending(SessionPendingInteraction::Approval);
        view.take_pending_changes();

        // Setting the same interaction kind again should be a no-op.
        view.on_interaction_pending(SessionPendingInteraction::Approval);

        assert!(view.take_pending_changes().is_empty());
    }

    #[test]
    fn should_emit_no_change_event_when_turn_starts_again_without_state_change() {
        let mut view = SessionActivityView::new();
        view.on_agent_turn_started();
        view.take_pending_changes();

        // Calling on_agent_turn_started again with no intervening end is a
        // no-op because the state is unchanged.
        view.on_agent_turn_started();

        assert!(view.take_pending_changes().is_empty());
    }

    #[test]
    fn should_return_all_changes_and_clear_buffer_when_taking_pending_changes() {
        let mut view = SessionActivityView::new();

        view.on_agent_turn_started();
        view.on_interaction_pending(SessionPendingInteraction::Approval);
        view.on_agent_turn_ended(SessionTurnOutcome::Completed);

        let changes = view.take_pending_changes();
        assert_eq!(changes.len(), 3);
        assert_eq!(changes[0].cause, SessionActivityCause::TurnStarted);
        assert_eq!(changes[1].cause, SessionActivityCause::Interaction);
        assert_eq!(changes[2].cause, SessionActivityCause::TurnEnded);

        // Buffer is now empty.
        assert!(view.take_pending_changes().is_empty());
    }

    #[test]
    fn should_emit_no_change_event_when_agent_lifecycle_changes_while_idle() {
        let mut view = SessionActivityView::new();

        // No turn, no background — lifecycle change is a no-op.
        view.on_agent_lifecycle_change();

        assert!(view.take_pending_changes().is_empty());
    }

    #[test]
    fn should_stay_busy_when_agent_lifecycle_changes_while_background_active() {
        let mut view = SessionActivityView::new();
        view.on_background_added();
        view.take_pending_changes();

        view.on_agent_lifecycle_change();

        // Background is still tracked, so busy stays true — no change.
        assert!(view.state().busy);
        assert!(view.take_pending_changes().is_empty());
    }

    #[test]
    fn should_update_pending_interaction_when_approval_replaces_question() {
        let mut view = SessionActivityView::new();
        view.on_interaction_pending(SessionPendingInteraction::Question);
        view.take_pending_changes();

        view.on_interaction_pending(SessionPendingInteraction::Approval);

        assert_eq!(
            view.state().pending_interaction,
            SessionPendingInteraction::Approval
        );
        let changes = view.take_pending_changes();
        assert_eq!(changes.len(), 1);
    }
}
