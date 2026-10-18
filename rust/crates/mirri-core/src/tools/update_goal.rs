//! `UpdateGoalTool` — update the status of the current goal.
//!
//! The model supplies a new lifecycle status; the tool calls
//! [`GoalManager::update`]. Valid statuses are `active`, `complete`, and
//! `blocked`. Pausing is reserved for the user — the model can never supply
//! `paused`; it resumes a user-paused goal by calling `UpdateGoal` with
//! `active` when the user asks to continue. An unknown status is rejected
//! in phase 1 (before any mutation), and updating when no goal exists is
//! reported as an error so the model knows to create one first.

use std::sync::Arc;

use mirri_proto::WireOp;

use crate::error::EngineResult;
use crate::goal::{GoalManager, GoalStatus, goal_snapshot_wire, goal_status_wire};
use crate::scope::SessionWireLog;
use crate::session::events::{EventBus, SessionEvent};
use crate::tool::{
    ExecutionPlan, Tool, ToolErrorResult, ToolExecution, ToolInputDisplay, ToolResult,
};

/// `UpdateGoal` — update the status of the current goal.
///
/// Bound to the session-scoped [`GoalManager`]. Use `complete` when the
/// objective has been achieved and `blocked` when prerequisites are missing.
pub struct UpdateGoalTool {
    /// The session-scoped goal manager this tool mutates.
    goal: Arc<GoalManager>,
    /// The session's wire-log recorder. When attached, status mutations are
    /// persisted as `goal.update` ops (write-before-mutate) so a resumed
    /// session replays them back into the goal manager.
    wire_recorder: Option<SessionWireLog>,
    /// The session's event bus. When attached, a successful status change
    /// announces the new snapshot on the wire (`event.goal.updated`, actor
    /// `model`).
    event_bus: Option<Arc<EventBus>>,
}

impl UpdateGoalTool {
    /// Create the tool bound to the session-scoped goal manager.
    pub fn new(goal: Arc<GoalManager>) -> Self {
        Self {
            goal,
            wire_recorder: None,
            event_bus: None,
        }
    }

    /// Attach the session's wire-log recorder so status mutations persist.
    pub fn with_wire_recorder(mut self, recorder: SessionWireLog) -> Self {
        self.wire_recorder = Some(recorder);
        self
    }

    /// Attach the session's event bus so goal mutations are announced.
    pub fn with_event_bus(mut self, event_bus: Option<Arc<EventBus>>) -> Self {
        self.event_bus = event_bus;
        self
    }
}

impl Tool for UpdateGoalTool {
    fn name(&self) -> &str {
        "UpdateGoal"
    }

    fn description(&self) -> &str {
        "Update the status of the current goal. Valid statuses: 'active', \
         'complete', 'blocked'. Use 'complete' when the objective \
         has been achieved."
    }

    fn parameters(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "status": {
                        "type": "string",
                        "enum": ["active", "complete", "blocked"],
                        "description": "New lifecycle status for the goal."
                    }
                },
                "required": ["status"],
                "additionalProperties": false
            })
        })
    }

    fn resolve_execution(&self, input: serde_json::Value) -> EngineResult<ToolExecution> {
        let Some(status_str) = input.get("status").and_then(serde_json::Value::as_str) else {
            return Ok(ToolExecution::Error(ToolErrorResult {
                message: "UpdateGoal requires a 'status' string argument".to_string(),
            }));
        };
        let status = match status_str {
            "active" => GoalStatus::Active,
            "complete" => GoalStatus::Complete,
            "blocked" => GoalStatus::Blocked,
            other => {
                return Ok(ToolExecution::Error(ToolErrorResult {
                    message: format!(
                        "UpdateGoal 'status' must be one of active, complete, blocked; got '{other}'"
                    ),
                }));
            }
        };
        // Take ownership so the label can outlive `input` and be moved into
        // the execute closure.
        let status_label = status_str.to_string();

        let goal = Arc::clone(&self.goal);
        let wire_recorder = self.wire_recorder.clone();
        let event_bus = self.event_bus.clone();
        let plan = ExecutionPlan {
            accesses: Vec::new(),
            display: ToolInputDisplay::Generic {
                summary: format!("Update goal status to {status_label}"),
            },
            approval_rule: "UpdateGoal".to_string(),
            // A lifecycle change (especially to a terminal state like
            // `complete` or `blocked`) can invalidate work planned later in
            // the same batch.
            stop_batch_after_this: true,
        };

        Ok(ToolExecution::Runnable {
            plan,
            execute: Box::new(move |_ctx| {
                Box::pin(async move {
                    if goal.get().is_none() {
                        return ToolResult::error("No active goal to update; create a goal first.");
                    }
                    // TS parity (goal service guards): terminal
                    // transitions require an ACTIVE goal — a user pause
                    // (or any other non-active state) is never
                    // overwritten by the model. Resuming
                    // (paused/blocked → active) stays allowed: the
                    // paused-goal note tells the model to call UpdateGoal
                    // with 'active' when the user asks to continue.
                    if matches!(status, GoalStatus::Complete | GoalStatus::Blocked)
                        && goal
                            .get()
                            .is_some_and(|goal| goal.status != GoalStatus::Active)
                    {
                        return ToolResult::error(match status {
                            GoalStatus::Complete => "Goal not completed: no active goal.",
                            GoalStatus::Blocked => "Goal not blocked: no active goal.",
                            _ => unreachable!(),
                        });
                    }
                    // Write-before-mutate: persist the status change before
                    // touching the in-memory goal, so a crash between the two
                    // leaves the wire log ahead of memory (resume treats the
                    // wire log as the source of truth).
                    if let Some(recorder) = wire_recorder.as_ref() {
                        recorder
                            .record_op(WireOp::GoalUpdate {
                                goal_id: None,
                                status: Some(goal_status_wire(status).to_string()),
                                reason: None,
                                turns_used: None,
                                tokens_used: None,
                                wall_clock_ms: None,
                                budget_limits: None,
                                actor: Some("agent".to_string()),
                            })
                            .await;
                    }
                    goal.update(status);
                    // Announce the status change on the session bus (the
                    // `goal.updated` wire event, actor `model` — the tool
                    // path is a model-initiated mutation). The snapshot
                    // carries the protocol spelling of the new status.
                    if let Some(event_bus) = event_bus.as_ref()
                        && let Some(current) = goal.get()
                    {
                        event_bus.publish(SessionEvent::GoalUpdated {
                            snapshot: goal_snapshot_wire(&current, &goal),
                            change: Some(serde_json::json!({
                                "kind": "lifecycle",
                                "status": status_label,
                                "actor": "model",
                            })),
                        });
                    }
                    ToolResult::success(format!("Goal status updated to {status_label}."))
                })
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::goal::GoalStatus;
    use crate::tool::ToolOutput;

    fn output_text(result: &ToolResult) -> String {
        match &result.output {
            ToolOutput::Text(text) => text.clone(),
            ToolOutput::Parts(_) => panic!("expected text output"),
        }
    }

    async fn execute_tool(tool: &UpdateGoalTool, input: serde_json::Value) -> ToolResult {
        let execution = tool.resolve_execution(input).expect("resolve");
        let ToolExecution::Runnable { execute, .. } = execution else {
            panic!("expected runnable execution");
        };
        let ctx = crate::tool::ExecutableToolContext {
            wait_registry: None,
            turn_id: "test-turn".to_string(),
            tool_call_id: "test-call".to_string(),
            cancel: crate::cancel::CancelToken::default(),
            progress: crate::tool::ToolProgressHandle::default(),
        };
        execute(ctx).await
    }

    #[tokio::test]
    async fn should_update_status_when_a_goal_exists() {
        let manager = Arc::new(GoalManager::new());
        manager.create("Do the thing".to_string(), None).unwrap();
        let tool = UpdateGoalTool::new(Arc::clone(&manager));
        let input = serde_json::json!({ "status": "complete" });
        let result = execute_tool(&tool, input).await;
        assert!(!result.is_error, "{}", output_text(&result));
        assert_eq!(manager.get().unwrap().status, GoalStatus::Complete);
    }

    #[tokio::test]
    async fn should_update_to_each_valid_status_when_given() {
        let manager = Arc::new(GoalManager::new());
        manager.create("Multi-state".to_string(), None).unwrap();
        let tool = UpdateGoalTool::new(Arc::clone(&manager));

        for status in ["active", "blocked", "active", "complete"] {
            let result = execute_tool(&tool, serde_json::json!({ "status": status })).await;
            assert!(
                !result.is_error,
                "status '{status}' failed: {}",
                output_text(&result)
            );
            let expected = match status {
                "active" => GoalStatus::Active,
                "blocked" => GoalStatus::Blocked,
                "complete" => GoalStatus::Complete,
                _ => unreachable!(),
            };
            assert_eq!(
                manager.get().unwrap().status,
                expected,
                "after setting {status}"
            );
        }
    }

    #[tokio::test]
    async fn should_reject_complete_when_the_goal_is_paused_by_the_user() {
        let manager = Arc::new(GoalManager::new());
        manager.create("Do the thing".to_string(), None).unwrap();
        manager.update(GoalStatus::Paused);
        let tool = UpdateGoalTool::new(Arc::clone(&manager));

        let result = execute_tool(&tool, serde_json::json!({ "status": "complete" })).await;

        assert!(result.is_error, "{}", output_text(&result));
        assert!(
            output_text(&result).contains("no active goal"),
            "{}",
            output_text(&result)
        );
        // The user's pause is never overwritten by the model.
        assert_eq!(manager.get().unwrap().status, GoalStatus::Paused);
    }

    #[tokio::test]
    async fn should_reject_blocked_when_the_goal_is_paused_by_the_user() {
        let manager = Arc::new(GoalManager::new());
        manager.create("Do the thing".to_string(), None).unwrap();
        manager.update(GoalStatus::Paused);
        let tool = UpdateGoalTool::new(Arc::clone(&manager));

        let result = execute_tool(&tool, serde_json::json!({ "status": "blocked" })).await;

        assert!(result.is_error, "{}", output_text(&result));
        assert_eq!(manager.get().unwrap().status, GoalStatus::Paused);
    }

    #[tokio::test]
    async fn should_write_nothing_when_a_terminal_transition_is_rejected_against_a_paused_goal() {
        use crate::session::events::EventBus;
        use mirri_persistence::{AppendLogStore, JsonlAppendLog};

        // Given a user-paused goal with both a wire recorder and an event
        // bus attached to the tool
        let manager = Arc::new(GoalManager::new());
        manager.create("Do the thing".to_string(), None).unwrap();
        manager.update(GoalStatus::Paused);

        let tmp = tempfile::tempdir().expect("tempdir must be creatable");
        let writer = JsonlAppendLog::create(&tmp.path().join("wire.jsonl"))
            .await
            .expect("create wire log");
        let recorder = SessionWireLog::default();
        assert!(recorder.install(writer).await, "recorder slot installed");
        let bus = Arc::new(EventBus::new(16));
        let mut rx = bus.subscribe();
        let tool = UpdateGoalTool::new(Arc::clone(&manager))
            .with_wire_recorder(recorder)
            .with_event_bus(Some(bus));

        // When the model calls UpdateGoal with 'complete' — rejected by the
        // guard because the goal is user-paused, not active
        let result = execute_tool(&tool, serde_json::json!({ "status": "complete" })).await;

        // Then the tool errors, the recorder wrote no op, and the bus
        // published nothing: a rejected transition must neither persist
        // (resume replays the wire log as the source of truth) nor announce.
        assert!(result.is_error, "{}", output_text(&result));
        let log = JsonlAppendLog::open(&tmp.path().join("wire.jsonl"))
            .await
            .expect("open wire log");
        let ops: Vec<_> = log
            .read_all()
            .await
            .expect("read wire log")
            .into_iter()
            .filter(|op| !matches!(op, WireOp::Metadata { .. }))
            .collect();
        assert!(ops.is_empty(), "no goal.update op may be recorded: {ops:?}");
        assert!(
            rx.try_recv().is_err(),
            "no event.goal.updated may be published"
        );
        // The user's pause is untouched.
        assert_eq!(manager.get().unwrap().status, GoalStatus::Paused);
    }

    #[tokio::test]
    async fn should_allow_resume_from_paused_when_the_model_is_asked_to_continue() {
        let manager = Arc::new(GoalManager::new());
        manager.create("Do the thing".to_string(), None).unwrap();
        manager.update(GoalStatus::Paused);
        let tool = UpdateGoalTool::new(Arc::clone(&manager));

        let result = execute_tool(&tool, serde_json::json!({ "status": "active" })).await;

        assert!(!result.is_error, "{}", output_text(&result));
        assert_eq!(manager.get().unwrap().status, GoalStatus::Active);
    }

    #[tokio::test]
    async fn should_reject_paused_as_a_model_supplied_status() {
        let manager = Arc::new(GoalManager::new());
        manager.create("Do the thing".to_string(), None).unwrap();
        let tool = UpdateGoalTool::new(Arc::clone(&manager));

        let execution = tool
            .resolve_execution(serde_json::json!({ "status": "paused" }))
            .expect("resolve");
        let ToolExecution::Error(error) = execution else {
            panic!("expected error execution");
        };
        assert!(error.message.contains("paused"));
        assert_eq!(manager.get().unwrap().status, GoalStatus::Active);
    }

    #[tokio::test]
    async fn should_return_an_error_when_no_goal_exists() {
        let tool = UpdateGoalTool::new(Arc::new(GoalManager::new()));
        let input = serde_json::json!({ "status": "complete" });
        let result = execute_tool(&tool, input).await;
        assert!(result.is_error);
        assert!(
            output_text(&result).contains("No active goal"),
            "{}",
            output_text(&result)
        );
    }

    #[tokio::test]
    async fn should_reject_an_invalid_status_when_status_is_unknown() {
        let manager = Arc::new(GoalManager::new());
        manager.create("Do the thing".to_string(), None).unwrap();
        let tool = UpdateGoalTool::new(Arc::clone(&manager));
        let execution = tool
            .resolve_execution(serde_json::json!({ "status": "cancelled" }))
            .expect("resolve");
        let ToolExecution::Error(error) = execution else {
            panic!("expected error execution");
        };
        assert!(error.message.contains("cancelled"));
        // The original status is untouched.
        assert_eq!(manager.get().unwrap().status, GoalStatus::Active);
    }

    #[tokio::test]
    async fn should_return_a_phase1_error_when_status_is_missing() {
        let manager = Arc::new(GoalManager::new());
        manager.create("Do the thing".to_string(), None).unwrap();
        let tool = UpdateGoalTool::new(Arc::clone(&manager));
        let execution = tool
            .resolve_execution(serde_json::json!({}))
            .expect("resolve");
        let ToolExecution::Error(error) = execution else {
            panic!("expected error execution");
        };
        assert!(error.message.contains("status"));
    }

    #[tokio::test]
    async fn should_stop_the_batch_when_planned() {
        let manager = Arc::new(GoalManager::new());
        manager.create("Do the thing".to_string(), None).unwrap();
        let tool = UpdateGoalTool::new(Arc::clone(&manager));
        let execution = tool
            .resolve_execution(serde_json::json!({ "status": "complete" }))
            .expect("resolve");
        let ToolExecution::Runnable { plan, .. } = execution else {
            panic!("expected runnable execution");
        };
        assert!(
            plan.stop_batch_after_this,
            "a status change should stop the rest of the batch"
        );
    }

    #[tokio::test]
    async fn should_publish_goal_updated_with_the_new_status_when_an_event_bus_is_attached() {
        use crate::session::events::{EventBus, SessionEvent};

        let manager = Arc::new(GoalManager::new());
        manager.create("Do the thing".to_string(), None).unwrap();
        let bus = Arc::new(EventBus::new(16));
        let mut rx = bus.subscribe();
        let tool = UpdateGoalTool::new(Arc::clone(&manager)).with_event_bus(Some(bus));
        let result = execute_tool(&tool, serde_json::json!({ "status": "complete" })).await;
        assert!(!result.is_error, "{}", output_text(&result));

        let event = rx.try_recv().expect("goal.updated should be published");
        let SessionEvent::GoalUpdated { snapshot, change } = event else {
            panic!("expected GoalUpdated, got {event:?}");
        };
        // The protocol snapshot spelling — `complete`, not the persisted
        // op's `completed`.
        assert_eq!(snapshot["status"], "complete");
        let change = change.expect("change present");
        assert_eq!(change["kind"], "lifecycle");
        assert_eq!(change["status"], "complete");
        assert_eq!(change["actor"], "model");
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn should_not_publish_goal_updated_when_no_goal_exists() {
        use crate::session::events::EventBus;

        let manager = Arc::new(GoalManager::new());
        let bus = Arc::new(EventBus::new(16));
        let mut rx = bus.subscribe();
        let tool = UpdateGoalTool::new(Arc::clone(&manager)).with_event_bus(Some(bus));
        let result = execute_tool(&tool, serde_json::json!({ "status": "complete" })).await;
        assert!(result.is_error);
        assert!(rx.try_recv().is_err(), "no event expected on failure");
    }
}
