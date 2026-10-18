//! Goal management service.
//!
//! A session-scoped singleton holding at most one [`Goal`] at a time,
//! mirroring the TypeScript `goal` domain. The goal answers "what is the
//! agent currently working toward" and carries an optional completion
//! criterion plus optional usage budgets (turns / tokens / wall-clock).

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::error::{EngineError, EngineResult};
use crate::llm::LlmUsage;

/// Injected when the user cancels the active goal (TS verbatim).
pub const GOAL_CANCELLED_REMINDER: &str = "The user cancelled the current goal. Ignore earlier active-goal reminders for that goal. Handle the next user request normally unless the user starts or resumes a goal.";

/// Injected on the one grace step granted when the hard budget is
/// reached mid-turn (TS verbatim).
pub const GOAL_BUDGET_STOP_REMINDER: &str = "The goal's hard budget was reached and the goal is now blocked; the user can resume it with /goal resume. Stop immediately. Do not call any more tools: they will be rejected. Write a brief final status message summarizing the progress so far.";

/// Lifecycle status of a goal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GoalStatus {
    /// The goal is being pursued.
    Active,
    /// The goal is temporarily on hold.
    Paused,
    /// The goal has been achieved.
    Complete,
    /// The goal cannot currently be pursued (e.g. missing prerequisites).
    Blocked,
}

/// The wire-log status string for a [`GoalStatus`] (`goal.create` /
/// `goal.update` ops).
///
/// Both write sides (the agent-side goal tools and the RPC handlers) must
/// use this mapping so the persisted strings round-trip through the reducer
/// (`GoalUpdate`) and the resume path's `parse_goal_status` without
/// warnings. `Complete` maps to the wire spelling `"completed"` (not
/// `"complete"`, the tool-schema spelling).
pub fn goal_status_wire(status: GoalStatus) -> &'static str {
    match status {
        GoalStatus::Active => "active",
        GoalStatus::Paused => "paused",
        GoalStatus::Complete => "completed",
        GoalStatus::Blocked => "blocked",
    }
}

/// The protocol `goalSnapshotSchema` status string for a [`GoalStatus`] —
/// the wire-event vocabulary, distinct from [`goal_status_wire`]'s
/// persisted-op spelling (`"complete"` here vs `"completed"` there). The
/// two contracts are pinned separately (the protocol's `goalSnapshotSchema`
/// vs the persisted `goal.update` op round-trip), so the mappings are
/// deliberately not shared.
pub fn goal_status_snapshot_wire(status: GoalStatus) -> &'static str {
    match status {
        GoalStatus::Active => "active",
        GoalStatus::Paused => "paused",
        GoalStatus::Complete => "complete",
        GoalStatus::Blocked => "blocked",
    }
}

/// Serialize a goal to the TS `GoalSnapshot` wire shape — the payload the
/// `event.goal.updated` notification carries and the `goal.get` RPC returns.
///
/// The core `Goal` carries no id, so a stable pseudo-id is synthesized (the
/// engine supports only a single active goal at a time, so `"goal-1"` is a
/// stable stand-in for the goal's identity on the wire); the budget struct's
/// `wall_clock_ms` is renamed to the camelCase wire key.
pub fn goal_snapshot_wire(goal: &Goal, manager: &GoalManager) -> serde_json::Value {
    let budget = goal.budget.map(|budget| {
        // The absent limits stay absent — never null placeholders (the
        // fixture's limits-only spelling marks each member optional, and
        // the TS kernel omits unset limits).
        let mut limits = serde_json::Map::new();
        if let Some(turns) = budget.turns {
            limits.insert("turns".to_string(), serde_json::json!(turns));
        }
        if let Some(tokens) = budget.tokens {
            limits.insert("tokens".to_string(), serde_json::json!(tokens));
        }
        if let Some(wall_clock_ms) = budget.wall_clock_ms {
            limits.insert("wallClockMs".to_string(), serde_json::json!(wall_clock_ms));
        }
        serde_json::Value::Object(limits)
    });
    let mut snapshot = serde_json::Map::new();
    // Hardcoded pseudo-id: the core `Goal` carries no id, and the engine
    // currently supports only a single active goal at a time, so "goal-1"
    // is a stable stand-in for the goal's identity on the wire.
    snapshot.insert("goalId".to_string(), serde_json::json!("goal-1"));
    snapshot.insert("objective".to_string(), serde_json::json!(goal.objective));
    // The optional criterion stays absent — never a null placeholder (the
    // protocol's `GoalSnapshot.completionCriterion` is `z.string().
    // optional()`, and the TS kernel emits the absent spelling).
    if let Some(criterion) = goal.completion_criterion.as_ref() {
        snapshot.insert(
            "completionCriterion".to_string(),
            serde_json::json!(criterion),
        );
    }
    snapshot.insert(
        "status".to_string(),
        serde_json::json!(goal_status_snapshot_wire(goal.status)),
    );
    snapshot.insert(
        "turnsUsed".to_string(),
        serde_json::json!(manager.used_turns()),
    );
    snapshot.insert(
        "tokensUsed".to_string(),
        serde_json::json!(manager.used_tokens()),
    );
    snapshot.insert("wallClockMs".to_string(), serde_json::json!(0));
    snapshot.insert("budget".to_string(), serde_json::json!(budget));
    serde_json::Value::Object(snapshot)
}

/// Optional resource budget attached to a goal.
///
/// Every field is optional; an absent field means "no limit for this
/// resource". `turns` and `tokens` are engine-accounting units,
/// `wall_clock_ms` is elapsed time since the goal became active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct GoalBudget {
    /// Maximum number of agent turns allowed for this goal.
    pub turns: Option<u64>,
    /// Maximum number of LLM tokens allowed for this goal.
    pub tokens: Option<u64>,
    /// Maximum wall-clock time in milliseconds.
    pub wall_clock_ms: Option<u64>,
}

/// The single active goal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Goal {
    /// What the agent is supposed to achieve.
    pub objective: String,
    /// Optional, measurable condition that marks the goal as achieved.
    pub completion_criterion: Option<String>,
    /// Current lifecycle status.
    pub status: GoalStatus,
    /// Resource budget, if one was set.
    pub budget: Option<GoalBudget>,
    /// Optional terminal reason recorded when the goal is `Blocked` or
    /// `Paused`. Cleared when the goal returns to `Active` or `Complete`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_reason: Option<String>,
}

/// Session-scoped goal manager.
///
/// Holds at most one [`Goal`] behind a `Mutex` so a single `Arc<GoalManager>`
/// can be shared across agents and async tasks.
///
/// In addition to the goal itself, the manager tracks accumulated usage
/// (turns and tokens) since the goal became active. This enables budget
/// enforcement: the host can call [`record_step_usage`](Self::record_step_usage)
/// after each step and check whether the goal's budget has been exhausted.
#[derive(Debug, Default)]
pub struct GoalManager {
    goal: Mutex<Option<Goal>>,
    /// Accumulated turns used since the current goal was created.
    used_turns: AtomicU64,
    /// Accumulated tokens (input + output) used since the current goal was created.
    used_tokens: AtomicU64,
    /// Wall-clock timer start point, set when a budget with a non-zero
    /// `wall_clock_ms` is first attached. Preserved across subsequent
    /// `set_budget` calls and reset to `None` by `clear()`.
    start_time: Mutex<Option<Instant>>,
}

impl GoalManager {
    /// Create a manager with no active goal.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a new active goal.
    ///
    /// Returns an error if a goal already exists (active or otherwise) —
    /// callers must [`GoalManager::clear`] first. This keeps the invariant
    /// "at most one goal exists at a time" enforced by the manager rather
    /// than by callers.
    ///
    /// Creating a goal also resets the accumulated usage counters.
    pub fn create(&self, objective: String, criterion: Option<String>) -> EngineResult<()> {
        let mut guard = self
            .goal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.is_some() {
            return Err(EngineError::RequestInvalid(
                "a goal already exists; clear it before creating a new one".to_string(),
            ));
        }
        *guard = Some(Goal {
            objective,
            completion_criterion: criterion,
            status: GoalStatus::Active,
            budget: None,
            terminal_reason: None,
        });
        // Reset usage counters for the new goal.
        self.used_turns.store(0, Ordering::SeqCst);
        self.used_tokens.store(0, Ordering::SeqCst);
        tracing::debug!("goal created");
        Ok(())
    }

    /// The current goal, or `None` if no goal has been created.
    pub fn get(&self) -> Option<Goal> {
        let guard = self
            .goal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.clone()
    }

    /// Update the status of the current goal.
    ///
    /// No-op (with a warning) when no goal exists — the method is infallible
    /// by design, matching the TypeScript `UpdateGoal` surface.
    pub fn update(&self, status: GoalStatus) {
        let mut guard = self
            .goal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match guard.as_mut() {
            Some(goal) => {
                goal.status = status;
                // Clear any stale terminal reason when leaving Blocked/Paused.
                if status != GoalStatus::Blocked && status != GoalStatus::Paused {
                    goal.terminal_reason = None;
                }
                tracing::debug!(?status, "goal status updated");
            }
            None => tracing::warn!("goal status update ignored: no active goal"),
        }
    }

    /// Update the status of the current goal with an optional terminal reason.
    ///
    /// When `status` is `Blocked` or `Paused`, the `reason` (if provided) is
    /// stored on the goal as `terminal_reason`. For any other status the
    /// terminal reason is cleared. No-op (with a warning) when no goal exists.
    pub fn update_with_reason(&self, status: GoalStatus, reason: Option<String>) {
        let mut guard = self
            .goal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match guard.as_mut() {
            Some(goal) => {
                goal.status = status;
                if status == GoalStatus::Blocked || status == GoalStatus::Paused {
                    goal.terminal_reason = reason;
                } else {
                    goal.terminal_reason = None;
                }
                tracing::debug!(?status, "goal status updated with reason");
            }
            None => tracing::warn!("goal status update ignored: no active goal"),
        }
    }

    /// Attach a budget to the current goal.
    ///
    /// No-op (with a warning) when no goal exists.
    ///
    /// If the budget includes a non-zero `wall_clock_ms` and no wall-clock
    /// timer is currently running, the timer is started. If a timer is
    /// already running (from a prior `set_budget` call), the original start
    /// time is preserved — the timer is NOT reset on budget changes.
    pub fn set_budget(&self, budget: GoalBudget) {
        let mut guard = self
            .goal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match guard.as_mut() {
            Some(goal) => {
                goal.budget = Some(budget);
                // Start the wall-clock timer when a non-zero wall-clock
                // budget is first attached. Subsequent calls keep the
                // original start time so the budget window is measured
                // from the first `set_budget`, not the most recent.
                if budget.wall_clock_ms.is_some_and(|ms| ms > 0) {
                    let mut start = self
                        .start_time
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if start.is_none() {
                        *start = Some(Instant::now());
                    }
                }
                tracing::debug!("goal budget set");
            }
            None => tracing::warn!("goal budget update ignored: no active goal"),
        }
    }

    /// Remove the current goal, returning the manager to the empty state.
    ///
    /// Also resets the accumulated usage counters and the wall-clock timer.
    pub fn clear(&self) {
        let mut guard = self
            .goal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = None;
        self.used_turns.store(0, Ordering::SeqCst);
        self.used_tokens.store(0, Ordering::SeqCst);
        *self
            .start_time
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        tracing::debug!("goal cleared");
    }

    // -----------------------------------------------------------------
    // Usage tracking and budget enforcement
    // -----------------------------------------------------------------

    /// Record token usage from a completed step and check if the goal's
    /// budget has been exhausted.
    ///
    /// The `usage` parameter carries input and output token counts. Each call
    /// increments the turn counter by 1 and the token counter by the sum of
    /// input and output tokens.
    ///
    /// Returns:
    /// - `Ok(None)` — no goal is active, or the goal's budget is not exhausted.
    /// - `Ok(Some(true))` — the goal's budget has been exhausted by this step.
    ///   The caller should stop the turn.
    /// - `Ok(Some(false))` — the goal is active with budget remaining.
    /// - `Err(_)` — should not happen with current implementation; reserved
    ///   for future use.
    pub fn record_step_usage(&self, usage: LlmUsage) -> EngineResult<Option<bool>> {
        let tokens_delta = usage.input_tokens + usage.output_tokens;
        let new_used_turns = self.used_turns.fetch_add(1, Ordering::SeqCst) + 1;
        let new_used_tokens =
            self.used_tokens.fetch_add(tokens_delta, Ordering::SeqCst) + tokens_delta;

        let guard = self
            .goal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        match guard.as_ref() {
            Some(goal) => {
                if let Some(budget) = goal.budget {
                    let turns_exceeded = budget.turns.is_some_and(|max| new_used_turns >= max);
                    let tokens_exceeded = budget.tokens.is_some_and(|max| new_used_tokens >= max);
                    let wall_clock_exceeded = self.is_wall_clock_exceeded(budget);
                    if turns_exceeded || tokens_exceeded || wall_clock_exceeded {
                        return Ok(Some(true));
                    }
                }
                Ok(Some(false))
            }
            None => Ok(None),
        }
    }

    /// Whether the current goal's budget has been exhausted.
    ///
    /// Returns `false` if no goal is active or if the goal has no budget.
    /// Returns `true` if any budget dimension (turns, tokens, or wall-clock)
    /// has been met or exceeded.
    pub fn is_budget_exhausted(&self) -> bool {
        let guard = self
            .goal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(goal) = guard.as_ref() else {
            return false;
        };
        let Some(budget) = goal.budget else {
            return false;
        };
        let used_turns = self.used_turns.load(Ordering::SeqCst);
        let used_tokens = self.used_tokens.load(Ordering::SeqCst);
        let turns_exhausted = budget.turns.is_some_and(|max| used_turns >= max);
        let tokens_exhausted = budget.tokens.is_some_and(|max| used_tokens >= max);
        if turns_exhausted || tokens_exhausted {
            return true;
        }
        self.is_wall_clock_exceeded(budget)
    }

    /// Check whether the wall-clock budget has been exceeded.
    ///
    /// Returns `false` if `wall_clock_ms` is `None` or zero, or if no timer
    /// has been started. Returns `true` if the elapsed time since the timer
    /// was started is greater than or equal to `wall_clock_ms`.
    ///
    /// This is a shared helper used by both [`record_step_usage`](Self::record_step_usage)
    /// and [`is_budget_exhausted`](Self::is_budget_exhausted). It locks
    /// `start_time` (a different mutex from `goal`), so it is safe to call
    /// while holding the `goal` guard.
    fn is_wall_clock_exceeded(&self, budget: GoalBudget) -> bool {
        if let Some(max_ms) = budget.wall_clock_ms
            && max_ms > 0
        {
            let start_guard = self
                .start_time
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(start) = *start_guard {
                return start.elapsed().as_millis() as u64 >= max_ms;
            }
        }
        false
    }

    /// Whether the current goal's status is `Complete`.
    ///
    /// Returns `false` if no goal is active.
    pub fn is_goal_complete(&self) -> bool {
        let guard = self
            .goal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard
            .as_ref()
            .is_some_and(|g| g.status == GoalStatus::Complete)
    }

    /// Whether the current goal is active (status == `Active`).
    ///
    /// Returns `false` if no goal is active or if the goal is in a
    /// non-active status (Paused, Complete, Blocked).
    pub fn is_goal_active(&self) -> bool {
        let guard = self
            .goal
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard
            .as_ref()
            .is_some_and(|g| g.status == GoalStatus::Active)
    }

    /// Accumulated turns used since the current goal was created.
    pub fn used_turns(&self) -> u64 {
        self.used_turns.load(Ordering::SeqCst)
    }

    /// Restore the accumulated usage counters from replayed wire state.
    ///
    /// A resume seeds the counters with the goal's replayed `turnsUsed` /
    /// `tokensUsed` totals so budget enforcement continues from where the
    /// restart left off, instead of reopening the limit from zero (F11).
    /// Meant for a freshly created goal during restore; calling it on a live
    /// goal simply overwrites the running totals.
    pub fn restore_usage(&self, turns: u64, tokens: u64) {
        self.used_turns.store(turns, Ordering::SeqCst);
        self.used_tokens.store(tokens, Ordering::SeqCst);
        tracing::debug!(turns, tokens, "goal usage counters restored");
    }

    /// Accumulated tokens used since the current goal was created.
    pub fn used_tokens(&self) -> u64 {
        self.used_tokens.load(Ordering::SeqCst)
    }

    /// Wall-clock milliseconds elapsed since the goal's budget timer was
    /// started.
    ///
    /// Returns 0 if no wall-clock timer is running (no goal, or goal has no
    /// wall-clock budget).
    pub fn wall_clock_ms(&self) -> u64 {
        let start = self
            .start_time
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        start.map(|s| s.elapsed().as_millis() as u64).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_serialize_a_goal_to_the_protocol_snapshot_shape() {
        let manager = GoalManager::new();
        manager
            .create(
                "Ship the release".to_string(),
                Some("All tests pass".to_string()),
            )
            .unwrap();
        let snapshot = goal_snapshot_wire(&manager.get().unwrap(), &manager);
        // The protocol `goalSnapshotSchema` spelling — `complete`, not the
        // persisted op's `completed`.
        assert_eq!(snapshot["status"], "active");
        assert_eq!(snapshot["goalId"], "goal-1");
        assert_eq!(snapshot["objective"], "Ship the release");
        assert_eq!(snapshot["completionCriterion"], "All tests pass");
        assert_eq!(snapshot["turnsUsed"], 0);
        assert_eq!(snapshot["tokensUsed"], 0);
        assert!(snapshot["budget"].is_null());
    }

    #[test]
    fn should_serialize_the_budget_with_the_camelcase_wire_keys() {
        let manager = GoalManager::new();
        manager.create("Budgeted".to_string(), None).unwrap();
        manager.set_budget(GoalBudget {
            turns: Some(5),
            tokens: Some(1000),
            wall_clock_ms: Some(60_000),
        });
        let snapshot = goal_snapshot_wire(&manager.get().unwrap(), &manager);
        assert_eq!(snapshot["budget"]["turns"], 5);
        assert_eq!(snapshot["budget"]["tokens"], 1000);
        assert_eq!(snapshot["budget"]["wallClockMs"], 60_000);
    }

    #[test]
    fn should_serialize_the_complete_status_with_the_protocol_spelling() {
        let manager = GoalManager::new();
        manager.create("Done".to_string(), None).unwrap();
        manager.update(GoalStatus::Complete);
        let snapshot = goal_snapshot_wire(&manager.get().unwrap(), &manager);
        assert_eq!(snapshot["status"], "complete");
    }

    #[test]
    fn should_return_no_goal_when_getting_from_new_manager() {
        let manager = GoalManager::new();
        assert!(manager.get().is_none());
    }

    #[test]
    fn should_create_active_goal_with_criterion_when_no_goal_exists() {
        let manager = GoalManager::new();
        manager
            .create(
                "Refactor the scheduler".to_string(),
                Some("All tests pass".to_string()),
            )
            .unwrap();
        let goal = manager.get().unwrap();
        assert_eq!(goal.objective, "Refactor the scheduler");
        assert_eq!(goal.completion_criterion.as_deref(), Some("All tests pass"));
        assert_eq!(goal.status, GoalStatus::Active);
        assert!(goal.budget.is_none());
    }

    #[test]
    fn should_create_goal_without_criterion_when_none_provided() {
        let manager = GoalManager::new();
        manager.create("Ship it".to_string(), None).unwrap();
        assert!(manager.get().unwrap().completion_criterion.is_none());
    }

    #[test]
    fn should_reject_create_when_goal_already_exists() {
        let manager = GoalManager::new();
        manager.create("First goal".to_string(), None).unwrap();
        let error = manager.create("Second goal".to_string(), None).unwrap_err();
        assert!(error.to_string().contains("goal already exists"));
        // The original goal is left untouched.
        assert_eq!(manager.get().unwrap().objective, "First goal");
    }

    #[test]
    fn should_change_status_when_updating_active_goal() {
        let manager = GoalManager::new();
        manager.create("Do the thing".to_string(), None).unwrap();
        manager.update(GoalStatus::Complete);
        assert_eq!(manager.get().unwrap().status, GoalStatus::Complete);
    }

    #[test]
    fn should_be_silent_no_op_when_updating_without_goal() {
        let manager = GoalManager::new();
        manager.update(GoalStatus::Paused);
        assert!(manager.get().is_none());
    }

    #[test]
    fn should_attach_budget_when_setting_budget_on_active_goal() {
        let manager = GoalManager::new();
        manager.create("Done by Friday".to_string(), None).unwrap();
        manager.set_budget(GoalBudget {
            turns: Some(50),
            tokens: None,
            wall_clock_ms: Some(86_400_000),
        });
        let budget = manager.get().unwrap().budget.unwrap();
        assert_eq!(budget.turns, Some(50));
        assert_eq!(budget.tokens, None);
        assert_eq!(budget.wall_clock_ms, Some(86_400_000));
    }

    #[test]
    fn should_be_empty_again_when_clearing_existing_goal() {
        let manager = GoalManager::new();
        manager.create("Ephemeral".to_string(), None).unwrap();
        manager.clear();
        assert!(manager.get().is_none());
        // And a fresh goal can be created afterwards.
        manager.create("Replacement".to_string(), None).unwrap();
        assert_eq!(manager.get().unwrap().objective, "Replacement");
    }

    // --- Usage tracking and budget enforcement tests (Gap 5) ---

    #[test]
    fn should_return_not_exhausted_when_recording_step_usage_with_remaining_budget() {
        let manager = GoalManager::new();
        manager.create("Do the thing".to_string(), None).unwrap();
        manager.set_budget(GoalBudget {
            turns: Some(10),
            tokens: Some(10_000),
            wall_clock_ms: None,
        });

        let result = manager.record_step_usage(LlmUsage {
            input_tokens: 100,
            output_tokens: 50,
            ..Default::default()
        });
        assert!(result.is_ok());
        assert_eq!(
            result.unwrap(),
            Some(false),
            "budget should not be exhausted"
        );
        assert_eq!(manager.used_turns(), 1);
        assert_eq!(manager.used_tokens(), 150);
    }

    #[test]
    fn should_return_exhausted_when_recording_step_usage_with_exhausted_turn_budget() {
        let manager = GoalManager::new();
        manager.create("Do the thing".to_string(), None).unwrap();
        manager.set_budget(GoalBudget {
            turns: Some(2),
            tokens: None,
            wall_clock_ms: None,
        });

        // Step 1 — not exhausted (used_turns = 1, max = 2, 1 < 2).
        let result = manager.record_step_usage(LlmUsage {
            input_tokens: 100,
            output_tokens: 50,
            ..Default::default()
        });
        assert_eq!(result.unwrap(), Some(false));

        // Step 2 — exhausted (used_turns = 2, max = 2, 2 >= 2).
        let result = manager.record_step_usage(LlmUsage {
            input_tokens: 100,
            output_tokens: 50,
            ..Default::default()
        });
        assert_eq!(
            result.unwrap(),
            Some(true),
            "turn budget should be exhausted"
        );
    }

    #[test]
    fn should_return_exhausted_when_recording_step_usage_with_exhausted_token_budget() {
        let manager = GoalManager::new();
        manager.create("Do the thing".to_string(), None).unwrap();
        manager.set_budget(GoalBudget {
            turns: None,
            tokens: Some(200),
            wall_clock_ms: None,
        });

        // Step 1 — 150 tokens used, not exhausted (150 < 200).
        let result = manager.record_step_usage(LlmUsage {
            input_tokens: 100,
            output_tokens: 50,
            ..Default::default()
        });
        assert_eq!(result.unwrap(), Some(false));

        // Step 2 — 300 tokens total, exhausted (300 >= 200).
        let result = manager.record_step_usage(LlmUsage {
            input_tokens: 100,
            output_tokens: 50,
            ..Default::default()
        });
        assert_eq!(
            result.unwrap(),
            Some(true),
            "token budget should be exhausted"
        );
    }

    #[test]
    fn should_return_none_when_recording_step_usage_without_goal() {
        let manager = GoalManager::new();
        let result = manager.record_step_usage(LlmUsage {
            input_tokens: 100,
            output_tokens: 50,
            ..Default::default()
        });
        assert_eq!(result.unwrap(), None);
    }

    #[test]
    fn should_return_not_exhausted_when_recording_step_usage_without_budget() {
        let manager = GoalManager::new();
        manager.create("Do the thing".to_string(), None).unwrap();
        // No budget set — unlimited.
        let result = manager.record_step_usage(LlmUsage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            ..Default::default()
        });
        assert_eq!(result.unwrap(), Some(false));
    }

    #[test]
    fn should_return_false_when_checking_budget_exhaustion_before_limit() {
        let manager = GoalManager::new();
        manager.create("Do the thing".to_string(), None).unwrap();
        manager.set_budget(GoalBudget {
            turns: Some(5),
            tokens: None,
            wall_clock_ms: None,
        });

        // Record 3 steps — not exhausted.
        for _ in 0..3 {
            let _ = manager.record_step_usage(LlmUsage::default());
        }
        assert!(!manager.is_budget_exhausted());
    }

    #[test]
    fn should_return_true_when_checking_budget_exhaustion_at_limit() {
        let manager = GoalManager::new();
        manager.create("Do the thing".to_string(), None).unwrap();
        manager.set_budget(GoalBudget {
            turns: Some(3),
            tokens: None,
            wall_clock_ms: None,
        });

        // Record 3 steps — exhausted (3 >= 3).
        for _ in 0..3 {
            let _ = manager.record_step_usage(LlmUsage::default());
        }
        assert!(manager.is_budget_exhausted());
    }

    #[test]
    fn should_return_true_when_checking_completed_goal() {
        let manager = GoalManager::new();
        manager.create("Done".to_string(), None).unwrap();
        manager.update(GoalStatus::Complete);
        assert!(manager.is_goal_complete());
        assert!(!manager.is_goal_active());
    }

    #[test]
    fn should_return_true_when_checking_active_goal() {
        let manager = GoalManager::new();
        manager.create("In progress".to_string(), None).unwrap();
        assert!(manager.is_goal_active());
        assert!(!manager.is_goal_complete());
    }

    #[test]
    fn should_return_false_when_checking_goal_status_without_goal() {
        let manager = GoalManager::new();
        assert!(!manager.is_goal_active());
        assert!(!manager.is_goal_complete());
    }

    #[test]
    fn should_restore_usage_counters_when_restore_usage_is_called() {
        // A resumed goal's replayed usage totals are seeded back into the
        // manager so budget enforcement continues from where the restart
        // left off instead of reopening the limit from zero.
        let manager = GoalManager::new();
        manager.create("Resumed work".to_string(), None).unwrap();
        manager.set_budget(GoalBudget {
            turns: Some(40),
            tokens: None,
            wall_clock_ms: None,
        });

        manager.restore_usage(30, 12_345);

        assert_eq!(manager.used_turns(), 30);
        assert_eq!(manager.used_tokens(), 12_345);
        assert!(
            !manager.is_budget_exhausted(),
            "30 of 40 turns must leave budget remaining"
        );

        // The restored counters keep counting from the restored base.
        let _ = manager.record_step_usage(LlmUsage {
            input_tokens: 10,
            output_tokens: 5,
            ..Default::default()
        });
        assert_eq!(manager.used_turns(), 31);
        assert_eq!(manager.used_tokens(), 12_360);
    }

    #[test]
    fn should_reset_usage_counters_when_clearing_goal() {
        let manager = GoalManager::new();
        manager.create("Temp".to_string(), None).unwrap();
        let _ = manager.record_step_usage(LlmUsage {
            input_tokens: 100,
            output_tokens: 50,
            ..Default::default()
        });
        assert_eq!(manager.used_turns(), 1);
        assert_eq!(manager.used_tokens(), 150);

        manager.clear();
        assert_eq!(manager.used_turns(), 0);
        assert_eq!(manager.used_tokens(), 0);
    }

    #[test]
    fn should_reset_usage_counters_when_creating_new_goal() {
        let manager = GoalManager::new();
        manager.create("First".to_string(), None).unwrap();
        let _ = manager.record_step_usage(LlmUsage {
            input_tokens: 100,
            output_tokens: 50,
            ..Default::default()
        });
        assert_eq!(manager.used_turns(), 1);

        manager.clear();
        manager.create("Second".to_string(), None).unwrap();
        assert_eq!(manager.used_turns(), 0, "counters should reset on new goal");
        assert_eq!(
            manager.used_tokens(),
            0,
            "counters should reset on new goal"
        );
    }

    #[test]
    fn should_report_exhausted_when_only_token_budget_exhausted() {
        // A goal with BOTH turns and tokens budgets. Exhaust ONLY the token
        // budget while turns remain under their limit. `is_budget_exhausted`
        // must return true because any dimension meeting its limit counts.
        let manager = GoalManager::new();
        manager.create("Dual budget".to_string(), None).unwrap();
        manager.set_budget(GoalBudget {
            turns: Some(2),
            tokens: Some(200),
            wall_clock_ms: None,
        });

        // Record 1 step with 250 tokens — turns (1) < 2, tokens (250) >= 200.
        let result = manager.record_step_usage(LlmUsage {
            input_tokens: 200,
            output_tokens: 50,
            ..Default::default()
        });
        assert_eq!(
            result.unwrap(),
            Some(true),
            "record_step_usage should report exhausted (tokens hit)"
        );
        assert_eq!(manager.used_turns(), 1, "only 1 turn used");
        assert!(manager.used_tokens() >= 200, "tokens should exceed 200");
        assert!(
            manager.is_budget_exhausted(),
            "token-only exhaustion should mark budget as exhausted"
        );
    }

    #[test]
    fn should_report_exhausted_when_only_turn_budget_exhausted() {
        // A fresh goal with BOTH turns and tokens budgets. Exhaust ONLY the
        // turn budget while tokens remain under their limit.
        let manager = GoalManager::new();
        manager
            .create("Dual budget again".to_string(), None)
            .unwrap();
        manager.set_budget(GoalBudget {
            turns: Some(2),
            tokens: Some(200),
            wall_clock_ms: None,
        });

        // Record 2 steps with low token usage — turns (2) >= 2, tokens < 200.
        for _ in 0..2 {
            let result = manager.record_step_usage(LlmUsage {
                input_tokens: 30,
                output_tokens: 20,
                ..Default::default()
            });
            // Step 1: turns=1 < 2, tokens=50 < 200 → not exhausted.
            // Step 2: turns=2 >= 2 → exhausted.
            assert!(result.is_ok());
        }
        assert_eq!(manager.used_turns(), 2, "2 turns used");
        assert!(
            manager.used_tokens() < 200,
            "tokens should be under 200 (got {})",
            manager.used_tokens()
        );
        assert!(
            manager.is_budget_exhausted(),
            "turn-only exhaustion should mark budget as exhausted"
        );
    }

    // --- Wall-clock budget enforcement tests (Gap A) ---

    #[test]
    fn should_report_exhausted_when_wall_clock_budget_exceeded() {
        let manager = GoalManager::new();
        manager.create("Time-boxed task".to_string(), None).unwrap();
        manager.set_budget(GoalBudget {
            turns: None,
            tokens: None,
            wall_clock_ms: Some(10),
        });

        // Sleep long enough to exceed the 10ms wall-clock budget.
        std::thread::sleep(std::time::Duration::from_millis(20));

        assert!(
            manager.is_budget_exhausted(),
            "budget should be exhausted after wall-clock limit exceeded"
        );
    }

    #[test]
    fn should_not_report_exhausted_when_wall_clock_budget_not_set() {
        let manager = GoalManager::new();
        manager.create("No time limit".to_string(), None).unwrap();
        manager.set_budget(GoalBudget {
            turns: None,
            tokens: None,
            wall_clock_ms: None,
        });

        std::thread::sleep(std::time::Duration::from_millis(10));

        assert!(
            !manager.is_budget_exhausted(),
            "budget should not be exhausted when wall_clock_ms is not set"
        );
    }

    #[test]
    fn should_start_timer_when_set_budget_called() {
        let manager = GoalManager::new();
        manager.create("Timed task".to_string(), None).unwrap();

        // Before setting a budget, the timer should not be running.
        {
            let start = manager.start_time.lock().unwrap_or_else(|e| e.into_inner());
            assert!(
                start.is_none(),
                "start_time should be None before set_budget"
            );
        }

        manager.set_budget(GoalBudget {
            turns: None,
            tokens: None,
            wall_clock_ms: Some(60_000),
        });

        // After setting a budget with a non-zero wall_clock_ms, the timer
        // should be running.
        {
            let start = manager.start_time.lock().unwrap_or_else(|e| e.into_inner());
            assert!(
                start.is_some(),
                "start_time should be Some after set_budget with wall_clock_ms"
            );
        }
    }

    #[test]
    fn should_reset_timer_when_clear_called() {
        let manager = GoalManager::new();
        manager.create("Timed task".to_string(), None).unwrap();
        manager.set_budget(GoalBudget {
            turns: None,
            tokens: None,
            wall_clock_ms: Some(60_000),
        });

        // Verify the timer was started.
        {
            let start = manager.start_time.lock().unwrap_or_else(|e| e.into_inner());
            assert!(
                start.is_some(),
                "start_time should be Some after set_budget"
            );
        }

        manager.clear();

        // After clear, the timer should be reset.
        {
            let start = manager.start_time.lock().unwrap_or_else(|e| e.into_inner());
            assert!(start.is_none(), "start_time should be None after clear");
        }

        // And is_budget_exhausted should return false (no goal, no budget).
        assert!(
            !manager.is_budget_exhausted(),
            "budget should not be exhausted after clear"
        );
    }

    #[test]
    fn should_report_exhausted_when_record_step_usage_checks_wall_clock() {
        // Verify that record_step_usage also considers the wall-clock budget,
        // not just turns and tokens.
        let manager = GoalManager::new();
        manager.create("Time-boxed task".to_string(), None).unwrap();
        manager.set_budget(GoalBudget {
            turns: Some(1_000),
            tokens: Some(1_000_000),
            wall_clock_ms: Some(10),
        });

        // Sleep long enough to exceed the 10ms wall-clock budget.
        std::thread::sleep(std::time::Duration::from_millis(20));

        let result = manager.record_step_usage(LlmUsage {
            input_tokens: 10,
            output_tokens: 5,
            ..Default::default()
        });
        assert_eq!(
            result.unwrap(),
            Some(true),
            "record_step_usage should report exhausted when wall-clock budget exceeded"
        );
    }

    #[test]
    fn should_preserve_original_start_time_when_set_budget_called_again() {
        // Setting a budget a second time should NOT reset the wall-clock timer.
        let manager = GoalManager::new();
        manager.create("Timed task".to_string(), None).unwrap();
        manager.set_budget(GoalBudget {
            turns: None,
            tokens: None,
            wall_clock_ms: Some(60_000),
        });

        let first_start = {
            let start = manager.start_time.lock().unwrap_or_else(|e| e.into_inner());
            start.unwrap()
        };

        // Sleep a bit so a reset would be detectable.
        std::thread::sleep(std::time::Duration::from_millis(15));

        // Set a new budget — the timer should NOT reset.
        manager.set_budget(GoalBudget {
            turns: Some(10),
            tokens: None,
            wall_clock_ms: Some(60_000),
        });

        let second_start = {
            let start = manager.start_time.lock().unwrap_or_else(|e| e.into_inner());
            start.unwrap()
        };

        assert_eq!(
            first_start, second_start,
            "start_time should not change when set_budget is called again"
        );
    }
}
