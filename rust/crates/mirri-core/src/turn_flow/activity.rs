//! `ActivityTracker` — the engine-side fold feeding the activity view.
//!
//! The TS reference (`agent-core-v2/src/agent/activityView/activityView.ts`)
//! is a per-agent, read-only, event-folded read model that owns no
//! authoritative state. This module mirrors that positioning for the rust
//! engine: the tracker holds one shared [`AgentActivityView`] per agent and
//! folds it from the engine's own observation points — the [`LoopHooks`]
//! surface (`TurnFlowHooks`), the turn lifecycle in [`TurnFlow`], and the
//! loop's live events (`StepRetrying`, `TurnInterrupted`). The hook points
//! are the fact source; the semantics are equivalent to the TS bus fold.
//!
//! **Observe-only**: the tracker never alters loop control flow. Every
//! recording method locks the view for a short synchronous critical section
//! and returns; failures are impossible (the lock is poisoned-tolerant).
//!
//! Concurrency: the view sits behind a `std::sync::Mutex` so an
//! `agent.activity` read (which must not lock the agent's `TurnFlow` — a
//! fire-mode turn holds that mutex for the whole turn) can snapshot it
//! mid-turn. `background` is *derived*, not folded: the snapshot reads the
//! task manager and the compaction orchestrator live, mirroring the TS view's
//! seeding from the owning services.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::activity_view::{
    ActivityRetryState, ActivityViewLifecycle, AgentActivityState, AgentActivityView,
    BackgroundRef, EndingReason, TurnEndReason,
};
use crate::compaction::FullCompaction;
use crate::loop_engine::{LiveEvent, TurnStopReason};
use crate::origin::PromptOrigin;
use crate::session::agent_status::{AgentStatusPublisher, phase_from_activity};
use crate::task::{TaskManager, TaskStatus};

/// Milliseconds since the Unix epoch.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Map a rust [`TurnStopReason`] to the klient `lastTurn.reason` vocabulary.
///
/// The klient contract (`turnEndReasonSchema`) admits exactly four words:
/// `completed` / `cancelled` / `failed` / `blocked`. The rust stop reasons
/// map as follows (locked by the tests below and mirrored in the
/// `authoritative-methods.tsv` note for `agent.activity`):
///
/// | rust `TurnStopReason` | klient `reason` |
/// |---|---|
/// | `EndTurn` | `completed` |
/// | `Aborted` | `cancelled` |
/// | `Paused` | `blocked` |
/// | `BlockedByHook` | `blocked` |
/// | `MaxTokens` | `failed` |
/// | `Filtered` | `failed` |
/// | `Unknown` | `failed` |
///
/// A turn that ends by engine error (including the `MaxStepsExceeded`
/// overflow surfaced as `Err`) is recorded as `failed` — there is no stop
/// reason to map in that arm, so the tracker passes [`TurnEndReason::Failed`]
/// directly.
pub fn map_stop_reason(reason: &TurnStopReason) -> TurnEndReason {
    match reason {
        TurnStopReason::EndTurn => TurnEndReason::Completed,
        TurnStopReason::Aborted => TurnEndReason::Cancelled,
        // The activity view keeps the cancelled family for watchdog stalls
        // (it is a cancellation); the wire outcome distinguishes the source
        // (`stalled`, ADR-004) and the consumers render from the wire.
        TurnStopReason::Stalled => TurnEndReason::Cancelled,
        TurnStopReason::Paused | TurnStopReason::BlockedByHook => TurnEndReason::Blocked,
        TurnStopReason::MaxTokens | TurnStopReason::Filtered | TurnStopReason::Unknown => {
            TurnEndReason::Failed
        }
    }
}

/// Background kind label for session tasks tracked by the rust
/// [`TaskManager`].
///
/// The TS `BackgroundRef.kind` is a free string; the TS side folds task
/// events with kind `"task"`. The rust `AgentTask` carries no kind field, so
/// this label is chosen to match the TS vocabulary rather than inventing a
/// rust-specific one.
pub const BACKGROUND_KIND_TASK: &str = "task";

/// Background kind label for an in-flight compaction, mirroring the TS
/// side's `'compaction'` label.
pub const BACKGROUND_KIND_COMPACTION: &str = "compaction";

/// Per-agent activity fold — one instance per agent, owned by (and dying
/// with) the agent's [`TurnFlow`](super::TurnFlow).
///
/// The recording methods are called from the engine's observation points;
/// [`snapshot`](Self::snapshot) is what the `agent.activity` RPC reads.
pub struct ActivityTracker {
    /// The folded read model. `Mutex` (not the turn flow's mutex) so reads
    /// succeed while a turn holds the `TurnFlow` lock.
    view: Mutex<AgentActivityView>,
    /// The agent's compaction orchestrator — read live at snapshot time to
    /// derive the in-flight-compaction background entry.
    compaction: Arc<Mutex<FullCompaction>>,
    /// Start timestamp of the current compaction, recorded when one begins
    /// (both the auto path in `before_step` and the manual
    /// `request_manual_compaction` path). Cleared opportunistically at
    /// snapshot time when the orchestrator is no longer compacting.
    compaction_since: Mutex<Option<u64>>,
    /// The agent's task manager — read live at snapshot time to derive the
    /// running-task background entries. Set through `TurnFlow`'s
    /// `set_task_manager`, hence interior mutability.
    task_manager: Mutex<Option<Arc<TaskManager>>>,
    /// The structured agent-status publisher (CR4) — the
    /// `event.agent.status.updated` emitter. Set through `TurnFlow`'s
    /// `set_status_publisher` (the same `Mutex<Option<..>>`-slot shape
    /// `set_task_manager` uses), so hosts can wire it before a turn
    /// starts without rebuilding the tracker.
    status_publisher: Mutex<Option<Arc<AgentStatusPublisher>>>,
    /// Epoch-ms timestamp of the most recent observation point — stamped by
    /// every recording method below. The liveness face consumers outside the
    /// view (the subagent heartbeat forwarding in the `Agent` tool) poll it
    /// to distinguish a live, working agent from one that stopped
    /// reporting. `0` means no observation has fired yet.
    last_activity_ms: AtomicU64,
}

impl ActivityTracker {
    /// Create a tracker sharing the agent's compaction orchestrator.
    pub fn new(compaction: Arc<Mutex<FullCompaction>>) -> Self {
        Self {
            view: Mutex::new(AgentActivityView::new()),
            compaction,
            compaction_since: Mutex::new(None),
            task_manager: Mutex::new(None),
            status_publisher: Mutex::new(None),
            last_activity_ms: AtomicU64::new(0),
        }
    }

    /// Stamp the activity clock — called by every recording method.
    fn touch(&self) {
        self.last_activity_ms.store(now_ms(), Ordering::Relaxed);
    }

    /// The epoch-ms timestamp of the most recent observation point
    /// (`0` when none has fired yet).
    pub fn last_activity_ms(&self) -> u64 {
        self.last_activity_ms.load(Ordering::Relaxed)
    }

    /// Wire (or clear) the task manager whose running tasks appear in the
    /// `background` list. Called by
    /// [`TurnFlow::set_task_manager`](super::TurnFlow::set_task_manager).
    pub fn set_task_manager(&self, task_manager: Option<Arc<TaskManager>>) {
        *self.task_manager.lock().unwrap_or_else(|e| e.into_inner()) = task_manager;
    }

    /// Wire (or clear) the structured agent-status publisher (CR4) —
    /// the `event.agent.status.updated` emitter. Called by
    /// [`TurnFlow::set_status_publisher`](super::TurnFlow::set_status_publisher);
    /// hosts wire it before the agent's first turn, so every phase
    /// transition below publishes from then on.
    pub fn set_status_publisher(&self, publisher: Option<Arc<AgentStatusPublisher>>) {
        *self
            .status_publisher
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = publisher;
    }

    /// Publish one `event.agent.status.updated` frame for the folded
    /// phase — the tail call of every phase-transition recording method.
    ///
    /// Lock discipline: the view mutex is taken only for the snapshot
    /// clone and released before the publisher is invoked; the publisher
    /// itself never holds two guards at once (see
    /// [`AgentStatusPublisher`]). No frame is published while any of this
    /// tracker's locks is held.
    ///
    /// Suppression (the binding-ledger contract): a disposed view keeps
    /// its turn — publishing its phase would report a stale agent — so a
    /// disposed tracker publishes nothing, mirroring the TS suppression
    /// of the `disposal` lifecycles.
    fn publish_status(&self) {
        let Some(publisher) = self
            .status_publisher
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        else {
            return;
        };
        // The view-only state (no task-manager / compaction derivation —
        // the phase needs only the turn fold), cloned under the view
        // mutex and released before the publish.
        let state = self
            .view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .state()
            .clone();
        if state.lifecycle == ActivityViewLifecycle::Disposed {
            return;
        }
        publisher.publish(phase_from_activity(&state));
    }

    /// Record that a turn started (folds `turn`, clears `lastTurn`).
    pub fn on_turn_started(&self, turn_id: u64, origin: PromptOrigin) {
        self.touch();
        self.view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .on_turn_started(turn_id as u32, origin);
        self.publish_status();
    }

    /// Record that the turn ended, mapping the stop reason to the klient
    /// vocabulary. Callers with an engine error (no stop reason) pass
    /// [`TurnEndReason::Failed`] directly.
    pub fn on_turn_ended(&self, turn_id: u64, reason: TurnEndReason, duration_ms: Option<u64>) {
        self.touch();
        self.view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .on_turn_ended(turn_id as u32, reason, duration_ms);
        self.publish_status();
    }

    /// Record the current step number (`before_step`).
    pub fn on_step(&self, step: u32) {
        self.touch();
        self.view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .on_step(step);
    }

    /// Record that an LLM request is in flight (`pre_llm_request` → phase
    /// `streaming`).
    pub fn on_llm_request(&self) {
        self.touch();
        self.view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .on_llm_request();
        self.publish_status();
    }

    /// Record that the LLM responded (`post_llm_request` → phase
    /// `running`).
    pub fn on_llm_response(&self) {
        self.touch();
        self.view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .on_llm_response();
        self.publish_status();
    }

    /// Record that a tool call entered its execution window
    /// (`prepare_tool_execution`).
    pub fn on_tool_call(&self, tool_call_id: &str, name: &str) {
        self.touch();
        self.view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .on_tool_call(tool_call_id.to_string(), name.to_string());
        self.publish_status();
    }

    /// Record that a tool call left its execution window
    /// (`finalize_tool_result`).
    pub fn on_tool_call_end(&self, tool_call_id: &str) {
        self.touch();
        self.view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .on_tool_call_end(tool_call_id);
        self.publish_status();
    }

    /// Record a step retry (the `StepRetrying` live event the
    /// `chat_with_retry` loop emits).
    pub fn on_retry(&self, retry: ActivityRetryState) {
        self.touch();
        self.view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .on_retry(retry);
        self.publish_status();
    }

    /// Record that a reverse-RPC approval wait began
    /// (`authorize_tool_execution` entering the `approval.ask` await).
    ///
    /// The engine→client `approval.ask` request carries no request id at the
    /// [`ReverseRpcCallback`](crate::reverse_rpc::ReverseRpcCallback) seam,
    /// so the tool call id — the unique identifier of the awaited approval —
    /// doubles as the `approvalId`.
    pub fn on_approval_pending(&self, tool_call_id: &str) {
        self.touch();
        self.view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .on_approval_pending(crate::activity_view::ApprovalRef {
                approval_id: tool_call_id.to_string(),
                tool_call_id: Some(tool_call_id.to_string()),
                since: now_ms(),
            });
        self.publish_status();
    }

    /// Record that the approval wait resolved (any decision — approved,
    /// rejected, errored, or cancelled).
    pub fn on_approval_resolved(&self, approval_id: &str) {
        self.touch();
        self.view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .on_approval_resolved(approval_id);
        self.publish_status();
    }

    /// Record that the turn entered its ending phase
    /// (`should_continue_after_stop`).
    pub fn on_ending(&self, reason: Option<EndingReason>) {
        self.touch();
        self.view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .on_ending(reason);
        self.publish_status();
    }

    /// Record that the ending decision was to continue the turn (steers
    /// flushed / goal budget remaining) — the ending flags are cleared.
    pub fn on_ending_cleared(&self) {
        self.touch();
        self.view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .on_ending_cleared();
        self.publish_status();
    }

    /// Record that a compaction began (auto `before_step` path or the
    /// manual `request_manual_compaction` path).
    pub fn on_compaction_started(&self) {
        *self
            .compaction_since
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(now_ms());
    }

    /// Fold a live event. `StepRetrying` is the only retry observation point
    /// the loop exposes (the retry backoff lives inside `chat_with_retry`,
    /// behind no hook); `TurnInterrupted` marks the abort ending.
    pub fn observe_live(&self, event: &LiveEvent) {
        match event {
            LiveEvent::StepRetrying {
                failed_attempt,
                next_attempt,
                max_attempts,
                delay_ms,
            } => self.on_retry(ActivityRetryState {
                failed_attempt: *failed_attempt,
                next_attempt: *next_attempt,
                max_attempts: *max_attempts,
                delay_ms: *delay_ms,
                // The live event carries no error classification, so both
                // optional fields stay absent (the klient schema marks
                // them optional).
                error_name: None,
                status_code: None,
            }),
            LiveEvent::TurnInterrupted { .. } => self.on_ending(Some(EndingReason::Aborted)),
            // The in-flight LLM heartbeat IS an activity observation: a
            // foreground subagent waiting on its provider's first token
            // keeps the activity clock moving, so the parent's
            // `drive_with_activity_forwarding` poll keeps forwarding
            // liveness to the parent turn's stall feed (the
            // session-fdf98268 incident bridge). Touch only — the view
            // and the status publisher stay untouched (the heartbeat is
            // high-frequency; publishing would flood the status stream).
            LiveEvent::LlmPending { .. } => self.touch(),
            // ADR-006 (the proposal's delta-touch task): a streaming
            // delta IS engine activity — a subagent mid-response (a slow
            // provider dripping tokens for minutes) keeps its activity
            // clock moving, so the parent turn's wait-registry probe sees
            // Advancing instead of killing a healthily-streaming child
            // (the pull-model equivalent of the main turn's per-delta
            // progress feed). Touch only — same anti-flood posture as
            // LlmPending (the view and the status publisher stay
            // untouched).
            LiveEvent::TextDelta { .. } | LiveEvent::ThinkDelta { .. } => self.touch(),
            _ => {}
        }
    }

    /// Dispose the view (the agent's session closed).
    pub fn dispose(&self) {
        self.view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .dispose();
    }

    /// The current activity state, with the `background` list derived live
    /// from the task manager and the compaction orchestrator.
    pub fn snapshot(&self) -> AgentActivityState {
        let mut state = self
            .view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .state()
            .clone();

        let mut background = Vec::new();

        // Running session tasks (kind "task", since = registration time).
        if let Some(tm) = self
            .task_manager
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            for task in tm.list() {
                if task.status == TaskStatus::Running {
                    background.push(BackgroundRef {
                        kind: BACKGROUND_KIND_TASK.to_string(),
                        id: task.id.clone(),
                        since: task.created_at.timestamp_millis().max(0) as u64,
                    });
                }
            }
        }

        // In-flight compaction (kind "compaction"). `since` is the recorded
        // start; when the orchestrator has finished but the start marker was
        // not cleared by an apply/cancel path, the marker is dropped here so
        // a stale timestamp can never leak into a later compaction.
        let compacting = self
            .compaction
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_compacting();
        let mut since = self
            .compaction_since
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if compacting {
            background.push(BackgroundRef {
                kind: BACKGROUND_KIND_COMPACTION.to_string(),
                id: BACKGROUND_KIND_COMPACTION.to_string(),
                since: since.unwrap_or_else(now_ms),
            });
        } else {
            *since = None;
        }

        state.background = background;
        state
    }

    /// The view's lifecycle (`ready` until disposed).
    pub fn lifecycle(&self) -> ActivityViewLifecycle {
        self.view
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .state()
            .lifecycle
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity_view::TurnEndReason;
    use crate::compaction::CompactionStrategy;
    use crate::loop_engine::TurnStopReason;
    use crate::task::TaskKind;

    fn tracker() -> ActivityTracker {
        ActivityTracker::new(Arc::new(Mutex::new(FullCompaction::new(
            CompactionStrategy::default(),
        ))))
    }

    /// Given a fresh tracker, when no observation has fired, then
    /// `last_activity_ms` reports zero — no activity yet.
    #[test]
    fn should_report_zero_last_activity_when_no_observation_has_fired() {
        let tracker = tracker();
        assert_eq!(tracker.last_activity_ms(), 0);
    }

    /// Given a stream that drips deltas mid-response (no step boundary, no
    /// tool call), when the deltas fold through `observe_live`, then the
    /// activity clock advances — a healthily-streaming subagent's wait is
    /// Advancing to the ADR-006 pull probe, never a stalled verdict (the
    /// pull-only e2e's unit anchor).
    #[test]
    fn should_touch_the_activity_clock_when_stream_deltas_arrive_mid_response() {
        let tracker = tracker();
        tracker.on_turn_started(1, PromptOrigin::User);
        let before = tracker.last_activity_ms();
        std::thread::sleep(std::time::Duration::from_millis(5));
        tracker.observe_live(&crate::loop_engine::LiveEvent::TextDelta {
            text: "chunk".to_string(),
        });
        let after_text = tracker.last_activity_ms();
        assert!(
            after_text > before,
            "a TextDelta must touch the activity clock: {after_text} !> {before}"
        );
        std::thread::sleep(std::time::Duration::from_millis(5));
        tracker.observe_live(&crate::loop_engine::LiveEvent::ThinkDelta {
            text: "think".to_string(),
        });
        let after_think = tracker.last_activity_ms();
        assert!(
            after_think > after_text,
            "a ThinkDelta must touch the activity clock: {after_think} !> {after_text}"
        );
    }

    /// Given a turn whose observation points fire over time, when the
    /// activity clock is read after each observation, then
    /// `last_activity_ms` advances monotonically — the liveness face the
    /// subagent heartbeat forwarding consumes (a live child never reads as
    /// stale).
    #[test]
    fn should_advance_last_activity_when_observation_points_fire() {
        let tracker = tracker();
        tracker.on_turn_started(1, PromptOrigin::User);
        let first = tracker.last_activity_ms();
        assert!(first > 0, "an observation must stamp the activity clock");
        std::thread::sleep(std::time::Duration::from_millis(5));
        tracker.on_step(1);
        std::thread::sleep(std::time::Duration::from_millis(5));
        tracker.on_tool_call("call_1", "Read");
        let second = tracker.last_activity_ms();
        assert!(
            second > first,
            "later observations must advance the clock (first={first}, second={second})"
        );
    }

    /// Given each rust stop reason, when mapped to the klient vocabulary,
    /// then the mapping is exactly the documented table (test-locked: any
    /// change to the mapping must update the comment above and the TSV
    /// note).
    #[test]
    fn should_map_every_stop_reason_to_the_klient_vocabulary_when_mapped() {
        assert_eq!(
            map_stop_reason(&TurnStopReason::EndTurn),
            TurnEndReason::Completed
        );
        assert_eq!(
            map_stop_reason(&TurnStopReason::Aborted),
            TurnEndReason::Cancelled
        );
        assert_eq!(
            map_stop_reason(&TurnStopReason::Paused),
            TurnEndReason::Blocked
        );
        assert_eq!(
            map_stop_reason(&TurnStopReason::BlockedByHook),
            TurnEndReason::Blocked
        );
        assert_eq!(
            map_stop_reason(&TurnStopReason::MaxTokens),
            TurnEndReason::Failed
        );
        assert_eq!(
            map_stop_reason(&TurnStopReason::Filtered),
            TurnEndReason::Failed
        );
        assert_eq!(
            map_stop_reason(&TurnStopReason::Unknown),
            TurnEndReason::Failed
        );
    }

    /// Given a fresh tracker, when snapshotted, then the state is the idle
    /// shape: ready lifecycle, no turn, no lastTurn, empty background.
    #[test]
    fn should_report_idle_state_when_no_turn_has_run() {
        let tracker = tracker();
        let state = tracker.snapshot();
        assert_eq!(state.lifecycle, ActivityViewLifecycle::Ready);
        assert!(state.turn.is_none());
        assert!(state.last_turn.is_none());
        assert!(state.background.is_empty());
    }

    /// Given a turn in flight, when steps and hooks are recorded, then the
    /// snapshot reflects the phase machine (streaming during the LLM
    /// request, tool_call during the tool window, running after).
    #[test]
    fn should_reflect_phase_transitions_when_hooks_fire() {
        let tracker = tracker();
        tracker.on_turn_started(1, PromptOrigin::User);
        tracker.on_step(1);
        tracker.on_llm_request();
        assert_eq!(
            tracker.snapshot().turn.as_ref().unwrap().phase,
            crate::activity_view::TurnPhase::Streaming
        );
        tracker.on_llm_response();
        tracker.on_tool_call("call_1", "Read");
        assert_eq!(
            tracker.snapshot().turn.as_ref().unwrap().phase,
            crate::activity_view::TurnPhase::ToolCall
        );
        assert_eq!(
            tracker
                .snapshot()
                .turn
                .as_ref()
                .unwrap()
                .active_tool_calls
                .len(),
            1
        );
        tracker.on_tool_call_end("call_1");
        assert_eq!(
            tracker.snapshot().turn.as_ref().unwrap().phase,
            crate::activity_view::TurnPhase::Running
        );
    }

    /// Given a pending approval, when resolved, then it leaves the pending
    /// list; while pending it is keyed by the tool call id.
    #[test]
    fn should_track_pending_approval_when_reverse_rpc_wait_begins() {
        let tracker = tracker();
        tracker.on_turn_started(1, PromptOrigin::User);
        tracker.on_approval_pending("call_9");
        let turn = tracker.snapshot().turn.unwrap();
        assert_eq!(turn.pending_approvals.len(), 1);
        assert_eq!(turn.pending_approvals[0].approval_id, "call_9");
        assert_eq!(
            turn.pending_approvals[0].tool_call_id.as_deref(),
            Some("call_9")
        );
        tracker.on_approval_resolved("call_9");
        assert!(
            tracker
                .snapshot()
                .turn
                .as_ref()
                .unwrap()
                .pending_approvals
                .is_empty()
        );
    }

    /// Given a StepRetrying live event, when observed, then the retry state
    /// is folded with the event's attempt numbers and no error
    /// classification (the event carries none).
    #[test]
    fn should_fold_retry_state_when_step_retrying_observed() {
        let tracker = tracker();
        tracker.on_turn_started(1, PromptOrigin::User);
        tracker.observe_live(&LiveEvent::StepRetrying {
            failed_attempt: 1,
            next_attempt: 2,
            max_attempts: 10,
            delay_ms: 500,
        });
        let turn = tracker.snapshot().turn.unwrap();
        assert_eq!(turn.phase, crate::activity_view::TurnPhase::Retrying);
        let retry = turn.retry.unwrap();
        assert_eq!(retry.failed_attempt, 1);
        assert_eq!(retry.next_attempt, 2);
        assert_eq!(retry.max_attempts, 10);
        assert_eq!(retry.delay_ms, 500);
        assert!(retry.error_name.is_none());
        assert!(retry.status_code.is_none());
    }

    /// Given a TurnInterrupted live event, when observed, then the turn is
    /// marked ending with the aborted reason.
    #[test]
    fn should_mark_ending_aborted_when_turn_interrupted_observed() {
        let tracker = tracker();
        tracker.on_turn_started(1, PromptOrigin::User);
        tracker.observe_live(&LiveEvent::TurnInterrupted {
            reason: "aborted".to_string(),
            attempted_steps: 3,
            active_step: Some(3),
            message: None,
        });
        let turn = tracker.snapshot().turn.unwrap();
        assert!(turn.ending);
        assert_eq!(turn.ending_reason, Some(EndingReason::Aborted));
    }

    /// Given a turn that ended, when the next state is read, then lastTurn
    /// carries the mapped reason and duration, and the turn is gone.
    #[test]
    fn should_retain_last_turn_when_turn_ended() {
        let tracker = tracker();
        tracker.on_turn_started(7, PromptOrigin::User);
        tracker.on_turn_ended(7, map_stop_reason(&TurnStopReason::EndTurn), Some(1500));
        let state = tracker.snapshot();
        assert!(state.turn.is_none());
        let last = state.last_turn.unwrap();
        assert_eq!(last.turn_id, 7);
        assert_eq!(last.reason, TurnEndReason::Completed);
        assert_eq!(last.duration_ms, Some(1500));
    }

    /// Given a disposed tracker, when snapshotted, then the lifecycle is
    /// disposed (the rest of the snapshot is unchanged).
    #[test]
    fn should_set_disposed_lifecycle_when_tracker_disposed() {
        let tracker = tracker();
        tracker.on_turn_started(1, PromptOrigin::User);
        tracker.dispose();
        assert_eq!(tracker.lifecycle(), ActivityViewLifecycle::Disposed);
        assert!(tracker.snapshot().turn.is_some());
    }

    /// Given running tasks, when snapshotted, then each appears in the
    /// background list under the "task" kind with its registration time.
    #[test]
    fn should_list_running_tasks_when_background_derived() {
        let tracker = tracker();
        let tm = Arc::new(TaskManager::new());
        let id = tm.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "build watcher".into(),
            "session-1".into(),
        );
        let finished = tm.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "done".into(),
            "session-1".into(),
        );
        tm.complete(&finished, 0);
        tracker.set_task_manager(Some(tm));

        let background = tracker.snapshot().background;
        assert_eq!(background.len(), 1, "only running tasks appear");
        assert_eq!(background[0].kind, "task");
        assert_eq!(background[0].id, id);
    }

    /// A status publisher over fresh default handles, sharing the given
    /// compaction orchestrator (the same handle relationship the
    /// production wiring builds), plus its bus subscription.
    fn status_publisher(
        compaction: Arc<Mutex<FullCompaction>>,
    ) -> (
        Arc<crate::session::agent_status::AgentStatusPublisher>,
        tokio::sync::broadcast::Receiver<crate::session::events::SessionEvent>,
    ) {
        let bus = Arc::new(crate::session::events::EventBus::new(
            crate::session::events::DEFAULT_EVENT_BUS_CAPACITY,
        ));
        let rx = bus.subscribe();
        let publisher = Arc::new(crate::session::agent_status::AgentStatusPublisher::new(
            bus,
            "main".to_string(),
            Arc::new(Mutex::new(crate::context_memory::ContextMemory::new())),
            compaction,
            Arc::new(Mutex::new(crate::usage::AgentUsageService::new())),
            Arc::new(std::sync::RwLock::new(
                crate::scope::config::AgentScopeConfig::default(),
            )),
            Arc::new(crate::plan::PlanManager::new()),
            Arc::new(Mutex::new(crate::swarm::SwarmModeService::new())),
        ));
        (publisher, rx)
    }

    /// Given a tracker with a wired publisher, when phase transitions are
    /// recorded, then each transition publishes one
    /// `event.agent.status.updated` frame carrying the phase derived
    /// from the fold — and the non-transition observation points
    /// (`on_step`) stay silent.
    #[test]
    fn should_publish_status_frames_when_phase_transitions_fire() {
        let compaction = Arc::new(Mutex::new(FullCompaction::new(
            CompactionStrategy::default(),
        )));
        let (publisher, mut rx) = status_publisher(Arc::clone(&compaction));
        let tracker = ActivityTracker::new(Arc::clone(&compaction));
        tracker.set_status_publisher(Some(publisher));

        tracker.on_turn_started(1, PromptOrigin::User);
        let frame = rx.try_recv().expect("turn start publishes a frame");
        let crate::session::events::SessionEvent::AgentStatusUpdated { phase, .. } = frame else {
            panic!("expected AgentStatusUpdated, got {frame:?}");
        };
        assert_eq!(phase, crate::session::agent_status::AgentPhase::Running);

        tracker.on_step(1);
        assert!(
            rx.try_recv().is_err(),
            "on_step is not a phase transition — no frame"
        );

        tracker.on_llm_request();
        let frame = rx.try_recv().expect("llm request publishes a frame");
        let crate::session::events::SessionEvent::AgentStatusUpdated { phase, .. } = frame else {
            panic!("expected AgentStatusUpdated, got {frame:?}");
        };
        assert_eq!(phase, crate::session::agent_status::AgentPhase::Streaming);

        tracker.on_turn_ended(1, map_stop_reason(&TurnStopReason::EndTurn), Some(10));
        let frame = rx.try_recv().expect("turn end publishes a frame");
        let crate::session::events::SessionEvent::AgentStatusUpdated { phase, .. } = frame else {
            panic!("expected AgentStatusUpdated, got {frame:?}");
        };
        assert_eq!(phase, crate::session::agent_status::AgentPhase::Ended);
    }

    /// Given a disposed tracker with a wired publisher, when a phase
    /// transition is recorded, then nothing is published — a disposed
    /// view keeps its turn, and publishing its phase would report a
    /// stale agent (the binding-ledger suppression).
    #[test]
    fn should_not_publish_status_when_the_view_is_disposed() {
        let compaction = Arc::new(Mutex::new(FullCompaction::new(
            CompactionStrategy::default(),
        )));
        let (publisher, mut rx) = status_publisher(Arc::clone(&compaction));
        let tracker = ActivityTracker::new(compaction);
        tracker.set_status_publisher(Some(publisher));
        tracker.on_turn_started(1, PromptOrigin::User);
        let _ = rx.try_recv(); // drain the pre-dispose frame

        tracker.dispose();
        tracker.on_llm_request();
        assert!(
            rx.try_recv().is_err(),
            "a disposed view must not publish a status frame"
        );
    }
}
