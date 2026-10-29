//! `SetGoalBudgetTool` — attach a resource budget to the current goal.
//!
//! The model supplies optional turn, token, and wall-clock limits; the tool
//! calls [`GoalManager::set_budget`]. At least one limit must be provided —
//! an all-absent budget imposes no limits and is therefore a no-op the model
//! should not perform. When any budget dimension is exhausted the host stops
//! the current turn (enforced by [`GoalManager::record_step_usage`], not by
//! this tool).

use std::sync::Arc;

use mirri_proto::WireOp;

use crate::error::EngineResult;
use crate::goal::{GoalBudget, GoalManager, goal_snapshot_wire};
use crate::scope::SessionWireLog;
use crate::session::events::{EventBus, SessionEvent};
use crate::tool::{
    ExecutionPlan, Tool, ToolErrorResult, ToolExecution, ToolInputDisplay, ToolResult,
};

/// `SetGoalBudget` — set the resource budget for the current goal.
///
/// Bound to the session-scoped [`GoalManager`]. A no-op (reported as an
/// error) when no goal is active, so the model is told to create a goal
/// first rather than silently dropping the budget.
pub struct SetGoalBudgetTool {
    /// The session-scoped goal manager this tool mutates.
    goal: Arc<GoalManager>,
    /// The session's wire-log recorder. When attached, budget changes are
    /// persisted as `goal.update` ops (write-before-mutate) so a resumed
    /// session replays the budget back into the goal manager.
    wire_recorder: Option<SessionWireLog>,
    /// The session's event bus. When attached, a successful budget change
    /// announces the new snapshot on the wire (`event.goal.updated`, actor
    /// `model`).
    event_bus: Option<Arc<EventBus>>,
}

impl SetGoalBudgetTool {
    /// Create the tool bound to the session-scoped goal manager.
    pub fn new(goal: Arc<GoalManager>) -> Self {
        Self {
            goal,
            wire_recorder: None,
            event_bus: None,
        }
    }

    /// Attach the session's wire-log recorder so budget changes persist.
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

impl Tool for SetGoalBudgetTool {
    fn name(&self) -> &str {
        "SetGoalBudget"
    }

    fn description(&self) -> &str {
        "Attach a resource budget (turns, tokens, and/or wall-clock milliseconds) \
         to the current goal. At least one limit must be provided. When any \
         budget dimension is exhausted, the host stops the current turn."
    }

    fn parameters(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "turns": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Maximum number of agent turns allowed for this goal."
                    },
                    "tokens": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Maximum number of LLM tokens allowed for this goal."
                    },
                    "wall_clock_ms": {
                        "type": "integer",
                        "minimum": 0,
                        "description": "Maximum wall-clock time in milliseconds."
                    }
                },
                "additionalProperties": false
            })
        })
    }

    fn resolve_execution(&self, input: serde_json::Value) -> EngineResult<ToolExecution> {
        let turns = input.get("turns").and_then(serde_json::Value::as_u64);
        let tokens = input.get("tokens").and_then(serde_json::Value::as_u64);
        let wall_clock_ms = input
            .get("wall_clock_ms")
            .and_then(serde_json::Value::as_u64);

        // An all-absent budget imposes no limits, so reject it up front —
        // the model should not call this tool with nothing to set.
        if turns.is_none() && tokens.is_none() && wall_clock_ms.is_none() {
            return Ok(ToolExecution::Error(ToolErrorResult {
                message:
                    "SetGoalBudget requires at least one of 'turns', 'tokens', or 'wall_clock_ms'"
                        .to_string(),
            }));
        }

        let budget = GoalBudget {
            turns,
            tokens,
            wall_clock_ms,
        };

        let goal = Arc::clone(&self.goal);
        let wire_recorder = self.wire_recorder.clone();
        let event_bus = self.event_bus.clone();
        let plan = ExecutionPlan {
            accesses: Vec::new(),
            display: ToolInputDisplay::Generic {
                summary: format!(
                    "Set goal budget: turns={}, tokens={}, wall_clock_ms={}",
                    fmt_opt(turns),
                    fmt_opt(tokens),
                    fmt_opt(wall_clock_ms),
                ),
            },
            approval_rule: "SetGoalBudget".to_string(),
            // Setting a budget constrains future work but does not invalidate
            // calls already planned in this batch.
            stop_batch_after_this: false,
        };

        Ok(ToolExecution::Runnable {
            plan,
            execute: Box::new(move |_ctx| {
                Box::pin(async move {
                    if goal.get().is_none() {
                        return ToolResult::error(
                            "No active goal to set a budget for; create a goal first.",
                        );
                    }
                    // Write-before-mutate: persist the budget as a
                    // `goal.update` op before attaching it in memory. The
                    // limits use the Rust-kernel budget spelling
                    // (`turns` / `tokens` / `wallClockMs`) the resume path
                    // parses back into a `GoalBudget`.
                    if let Some(recorder) = wire_recorder.as_ref() {
                        recorder
                            .record_op(WireOp::GoalUpdate {
                                goal_id: None,
                                status: None,
                                reason: None,
                                turns_used: None,
                                tokens_used: None,
                                wall_clock_ms: None,
                                budget_limits: Some(serde_json::json!({
                                    "turns": budget.turns,
                                    "tokens": budget.tokens,
                                    "wallClockMs": budget.wall_clock_ms,
                                })),
                                actor: Some("agent".to_string()),
                            })
                            .await;
                    }
                    goal.set_budget(budget);
                    // Announce the budget change on the session bus (the
                    // `goal.updated` wire event, actor `model`). The budget
                    // is a lifecycle change that leaves the status
                    // untouched, so the change carries no `status` key and
                    // the snapshot's `budget` object carries the new
                    // limits.
                    if let Some(event_bus) = event_bus.as_ref()
                        && let Some(current) = goal.get()
                    {
                        event_bus.publish(SessionEvent::GoalUpdated {
                            snapshot: goal_snapshot_wire(&current, &goal),
                            change: Some(serde_json::json!({
                                "kind": "lifecycle",
                                "actor": "model",
                            })),
                        });
                    }
                    ToolResult::success("Goal budget set.")
                })
            }),
        })
    }
}

/// Render an optional `u64` for the display summary: the number, or an em-dash
/// when the limit was not provided.
fn fmt_opt(value: Option<u64>) -> String {
    match value {
        Some(v) => v.to_string(),
        None => "—".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolOutput;

    fn output_text(result: &ToolResult) -> String {
        match &result.output {
            ToolOutput::Text(text) => text.clone(),
            ToolOutput::Parts(_) => panic!("expected text output"),
        }
    }

    async fn execute_tool(tool: &SetGoalBudgetTool, input: serde_json::Value) -> ToolResult {
        let execution = tool.resolve_execution(input).expect("resolve");
        let ToolExecution::Runnable { execute, .. } = execution else {
            panic!("expected runnable execution");
        };
        let ctx = crate::tool::ExecutableToolContext {
            approval_metadata: None,
            wait_registry: None,
            turn_id: "test-turn".to_string(),
            tool_call_id: "test-call".to_string(),
            cancel: crate::cancel::CancelToken::default(),
            progress: crate::tool::ToolProgressHandle::default(),
        };
        execute(ctx).await
    }

    #[tokio::test]
    async fn should_attach_a_budget_when_a_goal_exists() {
        let manager = Arc::new(GoalManager::new());
        manager.create("Do work".to_string(), None).unwrap();
        let tool = SetGoalBudgetTool::new(Arc::clone(&manager));
        let input = serde_json::json!({ "turns": 50, "tokens": 10000 });
        let result = execute_tool(&tool, input).await;
        assert!(!result.is_error, "{}", output_text(&result));
        let budget = manager.get().unwrap().budget.unwrap();
        assert_eq!(budget.turns, Some(50));
        assert_eq!(budget.tokens, Some(10000));
        assert_eq!(budget.wall_clock_ms, None);
    }

    #[tokio::test]
    async fn should_attach_a_wall_clock_only_budget_when_only_that_is_given() {
        let manager = Arc::new(GoalManager::new());
        manager.create("Timed work".to_string(), None).unwrap();
        let tool = SetGoalBudgetTool::new(Arc::clone(&manager));
        let input = serde_json::json!({ "wall_clock_ms": 86_400_000 });
        let result = execute_tool(&tool, input).await;
        assert!(!result.is_error);
        let budget = manager.get().unwrap().budget.unwrap();
        assert_eq!(budget.wall_clock_ms, Some(86_400_000));
        assert_eq!(budget.turns, None);
        assert_eq!(budget.tokens, None);
    }

    #[tokio::test]
    async fn should_return_an_error_when_no_goal_exists() {
        let tool = SetGoalBudgetTool::new(Arc::new(GoalManager::new()));
        let input = serde_json::json!({ "turns": 50 });
        let result = execute_tool(&tool, input).await;
        assert!(result.is_error);
        assert!(
            output_text(&result).contains("No active goal"),
            "{}",
            output_text(&result)
        );
    }

    #[tokio::test]
    async fn should_reject_an_empty_budget_when_all_fields_are_absent() {
        let manager = Arc::new(GoalManager::new());
        manager.create("Do work".to_string(), None).unwrap();
        let tool = SetGoalBudgetTool::new(Arc::clone(&manager));
        let execution = tool
            .resolve_execution(serde_json::json!({}))
            .expect("resolve");
        let ToolExecution::Error(error) = execution else {
            panic!("expected error execution");
        };
        assert!(error.message.contains("at least one"));
        // No budget was attached.
        assert!(manager.get().unwrap().budget.is_none());
    }

    #[tokio::test]
    async fn should_not_stop_the_batch_when_planned() {
        let manager = Arc::new(GoalManager::new());
        manager.create("Do work".to_string(), None).unwrap();
        let tool = SetGoalBudgetTool::new(Arc::clone(&manager));
        let execution = tool
            .resolve_execution(serde_json::json!({ "turns": 10 }))
            .expect("resolve");
        let ToolExecution::Runnable { plan, .. } = execution else {
            panic!("expected runnable execution");
        };
        assert!(
            !plan.stop_batch_after_this,
            "setting a budget should not stop the rest of the batch"
        );
    }

    #[tokio::test]
    async fn should_publish_goal_updated_with_the_budget_when_an_event_bus_is_attached() {
        use crate::session::events::{EventBus, SessionEvent};

        let manager = Arc::new(GoalManager::new());
        manager.create("Do work".to_string(), None).unwrap();
        let bus = Arc::new(EventBus::new(16));
        let mut rx = bus.subscribe();
        let tool = SetGoalBudgetTool::new(Arc::clone(&manager)).with_event_bus(Some(bus));
        let result = execute_tool(&tool, serde_json::json!({ "turns": 50 })).await;
        assert!(!result.is_error, "{}", output_text(&result));

        let event = rx.try_recv().expect("goal.updated should be published");
        let SessionEvent::GoalUpdated { snapshot, change } = event else {
            panic!("expected GoalUpdated, got {event:?}");
        };
        assert_eq!(snapshot["budget"]["turns"], 50);
        let change = change.expect("change present");
        assert_eq!(change["kind"], "lifecycle");
        assert_eq!(change["actor"], "model");
        // A budget change leaves the status untouched — no `status` key.
        assert!(change.get("status").is_none());
        assert!(rx.try_recv().is_err());
    }
}
