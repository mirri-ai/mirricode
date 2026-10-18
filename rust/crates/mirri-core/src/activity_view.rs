//! Per-agent read-only activity projection.
//!
//! Ports the TypeScript `activityView` domain. [`AgentActivityView`] is a
//! per-agent, read-only, event-folded read model of "what this agent is
//! doing" — the current turn with its live phase/stream/step/retry/
//! pending-approval/tool-call detail and the latest turn outcome. The view
//! owns no authoritative state: every fact is folded from events fed through
//! the `on_*` methods, and it can be discarded and rebuilt at any time.
//!
//! Bound at Agent scope — one instance per agent, dying with it.

use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::origin::PromptOrigin;

/// Returns the current wall-clock time in milliseconds since the Unix epoch.
///
/// Uses `unwrap_or_default` so a system clock set before 1970 does not panic;
/// the fallback is a zero duration (i.e. timestamp `0`).
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

/// Coarse phase of the agent's current turn.
///
/// Maps to the TypeScript `TurnPhase` union: `'running' | 'streaming' |
/// 'tool_call' | 'retrying'`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnPhase {
    /// The turn is running but not actively streaming or in a tool call.
    Running,
    /// The model is actively streaming tokens.
    Streaming,
    /// One or more tool calls are in flight.
    ToolCall,
    /// The turn is waiting to retry after a failure.
    Retrying,
}

/// Which stream is currently active during the [`TurnPhase::Streaming`] phase.
///
/// Maps to the TypeScript `stream` field on `ActivityTurnState`:
/// `'assistant' | 'thinking' | 'tool_call'`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamKind {
    /// Assistant text tokens.
    Assistant,
    /// Thinking / reasoning tokens.
    Thinking,
    /// Tool-call argument tokens.
    ToolCall,
}

/// Why the current turn is ending prematurely.
///
/// Maps to the TypeScript `endingReason` field on `ActivityTurnState`:
/// `'aborted' | 'max_steps' | 'error'`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndingReason {
    /// The turn was aborted (e.g. by a cancel token).
    Aborted,
    /// The maximum step count was reached.
    MaxSteps,
    /// An unrecoverable error occurred.
    Error,
}

/// Why the last turn ended.
///
/// Maps to the TypeScript `TurnEndReason` union (`'completed' | 'cancelled' |
/// 'failed' | 'blocked'`) with explicit `MaxSteps` and `Error` variants for
/// the Rust port, giving consumers finer-grained outcome detail without
/// overloading `Failed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnEndReason {
    /// The turn completed normally.
    Completed,
    /// The turn was cancelled by the user or system.
    Cancelled,
    /// The turn failed with an error.
    Failed,
    /// The turn was blocked (e.g. by the `before_step` hook or a paused
    /// stop).
    Blocked,
    /// The turn was stopped because the maximum step count was reached.
    MaxSteps,
    /// The turn ended due to an error condition distinct from `Failed`.
    Error,
}

/// Lifecycle status of the activity view itself.
///
/// Maps to the TypeScript `ActivityViewLifecycle` union:
/// `'ready' | 'disposed'`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityViewLifecycle {
    /// The view is active and receiving events.
    Ready,
    /// The view has been disposed and will no longer update.
    Disposed,
}

// ---------------------------------------------------------------------------
// Structs
// ---------------------------------------------------------------------------

/// A reference to an in-flight tool call within the current turn.
///
/// Maps to the TypeScript `ToolCallRef` interface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallRef {
    /// Unique identifier for the tool call.
    pub tool_call_id: String,
    /// Name of the tool being called (e.g. `"Read"`, `"Bash"`).
    pub name: String,
    /// Milliseconds since Unix epoch when the tool call started.
    pub since: u64,
}

/// A reference to a pending approval request within the current turn.
///
/// Maps to the TypeScript `ApprovalRef` interface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRef {
    /// Unique identifier for the approval request.
    pub approval_id: String,
    /// The tool call that triggered this approval, if any.
    pub tool_call_id: Option<String>,
    /// Milliseconds since Unix epoch when the approval was requested.
    pub since: u64,
}

/// Retry state captured when a step fails and will be retried.
///
/// Maps to the TypeScript `ActivityRetryState` interface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityRetryState {
    /// The attempt number that failed (1-based).
    pub failed_attempt: u32,
    /// The next attempt number that will be tried.
    pub next_attempt: u32,
    /// Maximum number of attempts before giving up.
    pub max_attempts: u32,
    /// Delay in milliseconds before the next attempt.
    pub delay_ms: u64,
    /// Name of the error that caused the retry, if available.
    pub error_name: Option<String>,
    /// HTTP-style status code associated with the failure, if any.
    pub status_code: Option<u16>,
}

/// The live state of the agent's current turn.
///
/// Maps to the TypeScript `ActivityTurnState` interface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityTurnState {
    /// Sequential turn identifier.
    pub turn_id: u32,
    /// Where this turn originated (user input, skill, injection, etc.).
    pub origin: PromptOrigin,
    /// Current phase of the turn.
    pub phase: TurnPhase,
    /// Which stream is active, if in the [`TurnPhase::Streaming`] phase.
    pub stream: Option<StreamKind>,
    /// Current step number within the turn.
    pub step: u32,
    /// Whether the turn is in the process of ending.
    pub ending: bool,
    /// Why the turn is ending, if `ending` is true.
    pub ending_reason: Option<EndingReason>,
    /// Retry state, if the turn is in the [`TurnPhase::Retrying`] phase.
    pub retry: Option<ActivityRetryState>,
    /// Approval requests currently pending within this turn.
    pub pending_approvals: Vec<ApprovalRef>,
    /// Tool calls currently in flight within this turn.
    pub active_tool_calls: Vec<ToolCallRef>,
    /// Milliseconds since Unix epoch when the turn started.
    pub since: u64,
}

/// The outcome of the most recently completed turn.
///
/// Maps to the TypeScript `ActivityLastTurnState` interface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityLastTurnState {
    /// The turn identifier that completed.
    pub turn_id: u32,
    /// Why the turn ended.
    pub reason: TurnEndReason,
    /// Duration of the turn in milliseconds, if known.
    pub duration_ms: Option<u64>,
    /// Milliseconds since Unix epoch when the turn ended.
    pub at: u64,
}

/// A reference to background work tracked by the agent.
///
/// Maps to the TypeScript `BackgroundRef` interface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundRef {
    /// Kind of background work (e.g. `"process"`, `"compaction"`).
    pub kind: String,
    /// Unique identifier for this background item.
    pub id: String,
    /// Milliseconds since Unix epoch when the background work started.
    pub since: u64,
}

/// The complete activity state for an agent — the current snapshot returned
/// by [`AgentActivityView::state`].
///
/// Maps to the TypeScript `AgentActivityState` interface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentActivityState {
    /// Lifecycle status of the view itself.
    pub lifecycle: ActivityViewLifecycle,
    /// The current turn, if one is active.
    pub turn: Option<ActivityTurnState>,
    /// The outcome of the most recently completed turn.
    pub last_turn: Option<ActivityLastTurnState>,
    /// Background work currently tracked by this agent.
    pub background: Vec<BackgroundRef>,
}

impl Default for AgentActivityState {
    fn default() -> Self {
        Self {
            lifecycle: ActivityViewLifecycle::Ready,
            turn: None,
            last_turn: None,
            background: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// AgentActivityView
// ---------------------------------------------------------------------------

/// Per-agent read-only activity projection.
///
/// Folds events from the agent's own event bus into a live snapshot of
/// "what this agent is doing". The view owns no authoritative state — every
/// fact is derived from the `on_*` methods and can be discarded and rebuilt
/// at any time.
///
/// # Example
///
/// ```
/// use mirri_core::activity_view::{AgentActivityView, StreamKind, TurnPhase};
/// use mirri_core::origin::PromptOrigin;
///
/// let mut view = AgentActivityView::new();
/// view.on_turn_started(1, PromptOrigin::User);
/// view.on_streaming(StreamKind::Assistant);
/// assert_eq!(view.state().turn.as_ref().unwrap().phase, TurnPhase::Streaming);
/// ```
#[derive(Debug)]
pub struct AgentActivityView {
    state: AgentActivityState,
}

impl AgentActivityView {
    /// Create a new activity view in the [`ActivityViewLifecycle::Ready`]
    /// state with no active turn, no last-turn outcome, and no background
    /// work.
    pub fn new() -> Self {
        Self {
            state: AgentActivityState::default(),
        }
    }

    /// The current activity state snapshot (read-only).
    pub fn state(&self) -> &AgentActivityState {
        &self.state
    }

    /// Record that a new turn has started.
    ///
    /// Clears any previous `last_turn` outcome and initializes a fresh
    /// [`ActivityTurnState`] in the [`TurnPhase::Running`] phase with step 0,
    /// no pending approvals, and no active tool calls.
    pub fn on_turn_started(&mut self, turn_id: u32, origin: PromptOrigin) {
        self.state.turn = Some(ActivityTurnState {
            turn_id,
            origin,
            phase: TurnPhase::Running,
            stream: None,
            step: 0,
            ending: false,
            ending_reason: None,
            retry: None,
            pending_approvals: Vec::new(),
            active_tool_calls: Vec::new(),
            since: now_ms(),
        });
        self.state.last_turn = None;
    }

    /// Record the current step number within the turn (`before_step` fold).
    ///
    /// No-op if no turn is active.
    pub fn on_step(&mut self, step: u32) {
        if let Some(turn) = self.state.turn.as_mut() {
            turn.step = step;
        }
    }

    /// Record that an LLM request is in flight (`pre_llm_request` fold).
    ///
    /// Sets the phase to [`TurnPhase::Streaming`]. The klient `stream` field
    /// (which stream is active) is not sourced — the hook point cannot
    /// distinguish assistant/thinking/tool-call deltas — so `stream` stays
    /// `None` (the schema marks it optional).
    pub fn on_llm_request(&mut self) {
        if let Some(turn) = self.state.turn.as_mut() {
            turn.phase = TurnPhase::Streaming;
            turn.retry = None;
        }
    }

    /// Record that the LLM responded (`post_llm_request` fold).
    ///
    /// Sets the phase back to [`TurnPhase::Running`] and clears any retry
    /// state — a response means the retry streak ended.
    pub fn on_llm_response(&mut self) {
        if let Some(turn) = self.state.turn.as_mut() {
            turn.phase = TurnPhase::Running;
            turn.stream = None;
            turn.retry = None;
        }
    }

    /// Record that the turn entered its ending phase
    /// (`should_continue_after_stop` fold).
    ///
    /// Sets `ending` and, when known, the `endingReason`. No-op if no turn
    /// is active.
    pub fn on_ending(&mut self, reason: Option<EndingReason>) {
        if let Some(turn) = self.state.turn.as_mut() {
            turn.ending = true;
            turn.ending_reason = reason;
        }
    }

    /// Record that the ending decision was to continue the turn (steers
    /// flushed, or goal budget remaining) — the ending flags are cleared.
    pub fn on_ending_cleared(&mut self) {
        if let Some(turn) = self.state.turn.as_mut() {
            turn.ending = false;
            turn.ending_reason = None;
        }
    }

    /// Record that the model is actively streaming tokens.
    ///
    /// Sets the phase to [`TurnPhase::Streaming`] and records which stream
    /// is active. Clears any retry state. No-op if no turn is active.
    pub fn on_streaming(&mut self, stream: StreamKind) {
        let Some(turn) = self.state.turn.as_mut() else {
            return;
        };
        turn.phase = TurnPhase::Streaming;
        turn.stream = Some(stream);
        turn.retry = None;
    }

    /// Record that a tool call has started.
    ///
    /// Sets the phase to [`TurnPhase::ToolCall`], adds the tool call to the
    /// active list, and clears any retry state. No-op if no turn is active.
    pub fn on_tool_call(&mut self, tool_call_id: String, name: String) {
        let Some(turn) = self.state.turn.as_mut() else {
            return;
        };
        turn.phase = TurnPhase::ToolCall;
        turn.stream = None;
        turn.retry = None;
        turn.active_tool_calls.push(ToolCallRef {
            tool_call_id,
            name,
            since: now_ms(),
        });
    }

    /// Record that a tool call has completed.
    ///
    /// Removes the tool call from the active list. If no tool calls remain,
    /// the phase returns to [`TurnPhase::Running`]; otherwise it stays
    /// [`TurnPhase::ToolCall`]. Clears stream and retry state. No-op if no
    /// turn is active or the tool call is not found.
    pub fn on_tool_call_end(&mut self, tool_call_id: &str) {
        let Some(turn) = self.state.turn.as_mut() else {
            return;
        };
        turn.active_tool_calls
            .retain(|tc| tc.tool_call_id != tool_call_id);
        turn.phase = if turn.active_tool_calls.is_empty() {
            TurnPhase::Running
        } else {
            TurnPhase::ToolCall
        };
        turn.stream = None;
        turn.retry = None;
    }

    /// Record that the current step is retrying after a failure.
    ///
    /// Sets the phase to [`TurnPhase::Retrying`], clears the stream, and
    /// stores the retry details. No-op if no turn is active.
    pub fn on_retry(&mut self, retry_state: ActivityRetryState) {
        let Some(turn) = self.state.turn.as_mut() else {
            return;
        };
        turn.phase = TurnPhase::Retrying;
        turn.stream = None;
        turn.retry = Some(retry_state);
    }

    /// Record that an approval request is pending.
    ///
    /// Adds the approval to the pending list. No-op if no turn is active.
    pub fn on_approval_pending(&mut self, approval: ApprovalRef) {
        let Some(turn) = self.state.turn.as_mut() else {
            return;
        };
        turn.pending_approvals.push(approval);
    }

    /// Record that a pending approval has been resolved.
    ///
    /// Removes the approval from the pending list. No-op if no turn is
    /// active or the approval is not found.
    pub fn on_approval_resolved(&mut self, approval_id: &str) {
        let Some(turn) = self.state.turn.as_mut() else {
            return;
        };
        turn.pending_approvals
            .retain(|a| a.approval_id != approval_id);
    }

    /// Record that a turn has ended.
    ///
    /// Moves the current turn to `last_turn` with the given reason and
    /// duration. If `duration_ms` is `None` and the turn ID matches the
    /// active turn, the duration is computed from the turn's `since`
    /// timestamp. If the turn ID does not match the active turn, only
    /// `last_turn` is updated (the active turn, if any, is left alone).
    pub fn on_turn_ended(&mut self, turn_id: u32, reason: TurnEndReason, duration_ms: Option<u64>) {
        let at = now_ms();
        let computed_duration = match &self.state.turn {
            Some(turn) if turn.turn_id == turn_id => {
                Some(duration_ms.unwrap_or_else(|| at.saturating_sub(turn.since)))
            }
            _ => duration_ms,
        };

        self.state.last_turn = Some(ActivityLastTurnState {
            turn_id,
            reason,
            duration_ms: computed_duration,
            at,
        });

        if matches!(&self.state.turn, Some(turn) if turn.turn_id == turn_id) {
            self.state.turn = None;
        }
    }

    /// Record that background work has been added.
    pub fn on_background_added(&mut self, kind: String, id: String) {
        self.state.background.push(BackgroundRef {
            kind,
            id,
            since: now_ms(),
        });
    }

    /// Record that background work has been removed.
    ///
    /// Removes the matching background entry by ID. If no entry matches,
    /// the call is a no-op.
    pub fn on_background_removed(&mut self, id: &str) {
        self.state.background.retain(|b| b.id != id);
    }

    /// Dispose the view.
    ///
    /// Sets the lifecycle to [`ActivityViewLifecycle::Disposed`]. The view
    /// will no longer update, though the last snapshot remains available via
    /// [`state`](Self::state).
    pub fn dispose(&mut self) {
        self.state.lifecycle = ActivityViewLifecycle::Disposed;
    }
}

impl Default for AgentActivityView {
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
    fn should_be_ready_with_no_turn_when_querying_new_view() {
        let view = AgentActivityView::new();

        let state = view.state();

        assert_eq!(state.lifecycle, ActivityViewLifecycle::Ready);
        assert!(state.turn.is_none());
        assert!(state.last_turn.is_none());
        assert!(state.background.is_empty());
    }

    #[test]
    fn should_have_running_turn_when_querying_state_after_turn_started() {
        let mut view = AgentActivityView::new();

        view.on_turn_started(1, PromptOrigin::User);

        let turn = view.state().turn.as_ref().expect("turn should be active");
        assert_eq!(turn.turn_id, 1);
        assert_eq!(turn.origin, PromptOrigin::User);
        assert_eq!(turn.phase, TurnPhase::Running);
        assert!(turn.stream.is_none());
        assert_eq!(turn.step, 0);
        assert!(!turn.ending);
        assert!(turn.ending_reason.is_none());
        assert!(turn.retry.is_none());
        assert!(turn.pending_approvals.is_empty());
        assert!(turn.active_tool_calls.is_empty());
    }

    /// Given an active turn, when the step number and ending fold points
    /// fire, then `step` tracks the loop's 1-based counter and the ending
    /// flags toggle with the continue decision.
    #[test]
    fn should_track_step_and_ending_flags_when_fold_points_fire() {
        let mut view = AgentActivityView::new();
        view.on_turn_started(1, PromptOrigin::User);

        view.on_step(3);
        assert_eq!(view.state().turn.as_ref().unwrap().step, 3);

        view.on_ending(Some(EndingReason::Aborted));
        let turn = view.state().turn.as_ref().unwrap();
        assert!(turn.ending);
        assert_eq!(turn.ending_reason, Some(EndingReason::Aborted));

        view.on_ending_cleared();
        let turn = view.state().turn.as_ref().unwrap();
        assert!(!turn.ending);
        assert!(turn.ending_reason.is_none());
    }

    /// Given an active turn, when the LLM request/response fold points
    /// fire, then the phase toggles streaming → running.
    #[test]
    fn should_toggle_streaming_phase_when_llm_fold_points_fire() {
        let mut view = AgentActivityView::new();
        view.on_turn_started(1, PromptOrigin::User);

        view.on_llm_request();
        assert_eq!(
            view.state().turn.as_ref().unwrap().phase,
            TurnPhase::Streaming
        );

        view.on_llm_response();
        assert_eq!(
            view.state().turn.as_ref().unwrap().phase,
            TurnPhase::Running
        );
    }

    #[test]
    fn should_enter_streaming_phase_with_kind_when_streaming_on_running_turn() {
        let mut view = AgentActivityView::new();
        view.on_turn_started(1, PromptOrigin::User);

        view.on_streaming(StreamKind::Assistant);

        let turn = view.state().turn.as_ref().unwrap();
        assert_eq!(turn.phase, TurnPhase::Streaming);
        assert_eq!(turn.stream, Some(StreamKind::Assistant));
    }

    #[test]
    fn should_ignore_streaming_when_no_turn_active() {
        let mut view = AgentActivityView::new();

        view.on_streaming(StreamKind::Thinking);

        assert!(view.state().turn.is_none());
    }

    #[test]
    fn should_switch_to_tool_call_phase_when_tool_call_starts_on_streaming_turn() {
        let mut view = AgentActivityView::new();
        view.on_turn_started(1, PromptOrigin::User);
        view.on_streaming(StreamKind::Assistant);

        view.on_tool_call("tc-1".to_string(), "Read".to_string());

        let turn = view.state().turn.as_ref().unwrap();
        assert_eq!(turn.phase, TurnPhase::ToolCall);
        assert!(turn.stream.is_none());
        assert_eq!(turn.active_tool_calls.len(), 1);
        assert_eq!(turn.active_tool_calls[0].tool_call_id, "tc-1");
        assert_eq!(turn.active_tool_calls[0].name, "Read");
    }

    #[test]
    fn should_return_to_running_phase_when_last_active_tool_call_ends() {
        let mut view = AgentActivityView::new();
        view.on_turn_started(1, PromptOrigin::User);
        view.on_tool_call("tc-1".to_string(), "Read".to_string());

        view.on_tool_call_end("tc-1");

        let turn = view.state().turn.as_ref().unwrap();
        assert_eq!(turn.phase, TurnPhase::Running);
        assert!(turn.active_tool_calls.is_empty());
    }

    #[test]
    fn should_stay_in_tool_call_phase_when_only_one_of_multiple_tool_calls_ends() {
        let mut view = AgentActivityView::new();
        view.on_turn_started(1, PromptOrigin::User);
        view.on_tool_call("tc-1".to_string(), "Read".to_string());
        view.on_tool_call("tc-2".to_string(), "Grep".to_string());

        view.on_tool_call_end("tc-1");

        let turn = view.state().turn.as_ref().unwrap();
        assert_eq!(turn.phase, TurnPhase::ToolCall);
        assert_eq!(turn.active_tool_calls.len(), 1);
        assert_eq!(turn.active_tool_calls[0].tool_call_id, "tc-2");
    }

    #[test]
    fn should_ignore_tool_call_when_no_turn_active() {
        let mut view = AgentActivityView::new();

        view.on_tool_call("tc-1".to_string(), "Read".to_string());

        assert!(view.state().turn.is_none());
    }

    #[test]
    fn should_enter_retrying_phase_with_state_when_retry_on_running_turn() {
        let mut view = AgentActivityView::new();
        view.on_turn_started(1, PromptOrigin::User);

        let retry_state = ActivityRetryState {
            failed_attempt: 1,
            next_attempt: 2,
            max_attempts: 3,
            delay_ms: 500,
            error_name: Some("rate_limited".to_string()),
            status_code: Some(429),
        };
        view.on_retry(retry_state.clone());

        let turn = view.state().turn.as_ref().unwrap();
        assert_eq!(turn.phase, TurnPhase::Retrying);
        assert!(turn.stream.is_none());
        assert_eq!(turn.retry.as_ref().unwrap(), &retry_state);
    }

    #[test]
    fn should_ignore_retry_when_no_turn_active() {
        let mut view = AgentActivityView::new();

        view.on_retry(ActivityRetryState {
            failed_attempt: 1,
            next_attempt: 2,
            max_attempts: 3,
            delay_ms: 100,
            error_name: None,
            status_code: None,
        });

        assert!(view.state().turn.is_none());
    }

    #[test]
    fn should_remove_approval_from_pending_list_when_resolved() {
        let mut view = AgentActivityView::new();
        view.on_turn_started(1, PromptOrigin::User);
        view.on_approval_pending(ApprovalRef {
            approval_id: "ap-1".to_string(),
            tool_call_id: Some("tc-1".to_string()),
            since: now_ms(),
        });

        let turn = view.state().turn.as_ref().unwrap();
        assert_eq!(turn.pending_approvals.len(), 1);

        view.on_approval_resolved("ap-1");

        let turn = view.state().turn.as_ref().unwrap();
        assert!(turn.pending_approvals.is_empty());
    }

    #[test]
    fn should_ignore_approval_pending_when_no_turn_active() {
        let mut view = AgentActivityView::new();

        view.on_approval_pending(ApprovalRef {
            approval_id: "ap-1".to_string(),
            tool_call_id: None,
            since: now_ms(),
        });

        assert!(view.state().turn.is_none());
    }

    #[test]
    fn should_clear_turn_and_set_last_turn_when_turn_ends() {
        let mut view = AgentActivityView::new();
        view.on_turn_started(1, PromptOrigin::User);

        view.on_turn_ended(1, TurnEndReason::Completed, Some(1500));

        assert!(view.state().turn.is_none());
        let last = view.state().last_turn.as_ref().unwrap();
        assert_eq!(last.turn_id, 1);
        assert_eq!(last.reason, TurnEndReason::Completed);
        assert_eq!(last.duration_ms, Some(1500));
    }

    #[test]
    fn should_compute_duration_when_turn_ends_without_duration() {
        let mut view = AgentActivityView::new();
        view.on_turn_started(1, PromptOrigin::User);

        view.on_turn_ended(1, TurnEndReason::Completed, None);

        let last = view.state().last_turn.as_ref().unwrap();
        // Duration should be computed (non-zero in practice, but we only
        // check that it is Some).
        assert!(last.duration_ms.is_some());
    }

    #[test]
    fn should_clear_last_turn_when_new_turn_starts() {
        let mut view = AgentActivityView::new();
        view.on_turn_started(1, PromptOrigin::User);
        view.on_turn_ended(1, TurnEndReason::Cancelled, None);
        assert!(view.state().last_turn.is_some());

        view.on_turn_started(2, PromptOrigin::User);

        assert!(view.state().last_turn.is_none());
        assert_eq!(view.state().turn.as_ref().unwrap().turn_id, 2);
    }

    #[test]
    fn should_update_only_last_turn_when_ending_turn_with_mismatched_id() {
        let mut view = AgentActivityView::new();
        view.on_turn_started(1, PromptOrigin::User);

        view.on_turn_ended(99, TurnEndReason::Failed, None);

        // Active turn is left intact.
        assert!(view.state().turn.is_some());
        assert_eq!(view.state().turn.as_ref().unwrap().turn_id, 1);
        // Last turn is still recorded.
        let last = view.state().last_turn.as_ref().unwrap();
        assert_eq!(last.turn_id, 99);
        assert_eq!(last.reason, TurnEndReason::Failed);
    }

    #[test]
    fn should_contain_added_background_entry_when_querying_state() {
        let mut view = AgentActivityView::new();

        view.on_background_added("process".to_string(), "bash-1".to_string());

        let state = view.state();
        assert_eq!(state.background.len(), 1);
        assert_eq!(state.background[0].kind, "process");
        assert_eq!(state.background[0].id, "bash-1");
    }

    #[test]
    fn should_empty_background_list_when_entry_removed() {
        let mut view = AgentActivityView::new();
        view.on_background_added("process".to_string(), "bash-1".to_string());

        view.on_background_removed("bash-1");

        assert!(view.state().background.is_empty());
    }

    #[test]
    fn should_remove_only_matching_background_when_one_of_multiple_removed() {
        let mut view = AgentActivityView::new();
        view.on_background_added("process".to_string(), "bash-1".to_string());
        view.on_background_added("compaction".to_string(), "full-compaction".to_string());

        view.on_background_removed("bash-1");

        let state = view.state();
        assert_eq!(state.background.len(), 1);
        assert_eq!(state.background[0].id, "full-compaction");
    }

    #[test]
    fn should_set_disposed_lifecycle_when_disposing_active_view() {
        let mut view = AgentActivityView::new();
        view.on_turn_started(1, PromptOrigin::User);

        view.dispose();

        assert_eq!(view.state().lifecycle, ActivityViewLifecycle::Disposed);
        // The turn state is still visible (read-only snapshot).
        assert!(view.state().turn.is_some());
    }

    #[test]
    fn should_clear_stream_and_retry_state_when_tool_call_starts_during_thinking() {
        let mut view = AgentActivityView::new();
        view.on_turn_started(1, PromptOrigin::User);
        view.on_streaming(StreamKind::Thinking);

        view.on_tool_call("tc-1".to_string(), "Bash".to_string());

        let turn = view.state().turn.as_ref().unwrap();
        assert!(turn.stream.is_none());
        assert!(turn.retry.is_none());
    }
}
