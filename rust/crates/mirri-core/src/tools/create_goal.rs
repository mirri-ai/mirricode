//! `CreateGoalTool` — create a new session-scoped goal.
//!
//! The model supplies an objective (and optional completion criterion); the
//! tool calls [`GoalManager::create`] and returns a confirmation. Only one
//! goal may exist at a time — a second [`CreateGoalTool`] invocation without
//! clearing first returns an error the model can see, so it never silently
//! overwrites an in-progress goal.

use std::sync::Arc;

use mirri_proto::WireOp;

use crate::error::EngineResult;
use crate::goal::{GoalManager, GoalStatus, goal_snapshot_wire};
use crate::scope::SessionWireLog;
use crate::session::events::EventBus;
use crate::session::events::SessionEvent;
use crate::tool::{
    ExecutionPlan, Tool, ToolErrorResult, ToolExecution, ToolInputDisplay, ToolResult,
};

/// `CreateGoal` — create a new session goal.
///
/// Bound to the session-scoped [`GoalManager`], so every agent in the session
/// observes the same goal. Creating a goal resets the accumulated usage
/// counters tracked by the manager.
pub struct CreateGoalTool {
    /// The session-scoped goal manager this tool mutates.
    goal: Arc<GoalManager>,
    /// The session's wire-log recorder. When attached, goal creation is
    /// persisted as a `goal.create` op (write-before-mutate) so a resumed
    /// session replays it back into the goal manager.
    wire_recorder: Option<SessionWireLog>,
    /// The session's event bus. When attached, a successful create announces
    /// the new goal on the wire (`event.goal.updated`, actor `model`).
    event_bus: Option<Arc<EventBus>>,
}

impl CreateGoalTool {
    /// Create the tool bound to the session-scoped goal manager.
    pub fn new(goal: Arc<GoalManager>) -> Self {
        Self {
            goal,
            wire_recorder: None,
            event_bus: None,
        }
    }

    /// Attach the session's wire-log recorder so goal creation persists.
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

impl Tool for CreateGoalTool {
    fn name(&self) -> &str {
        "CreateGoal"
    }

    fn description(&self) -> &str {
        "Create a new session goal describing what the agent is working toward. \
         Only one goal may exist at a time; if a goal already exists, update or \
         clear it before creating a new one. Optionally include a measurable \
         completion criterion."
    }

    fn parameters(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "objective": {
                        "type": "string",
                        "description": "What the agent is supposed to achieve."
                    },
                    "completion_criterion": {
                        "type": "string",
                        "description": "Optional, measurable condition that marks the goal as achieved."
                    }
                },
                "required": ["objective"],
                "additionalProperties": false
            })
        })
    }

    fn resolve_execution(&self, input: serde_json::Value) -> EngineResult<ToolExecution> {
        let Some(objective) = input.get("objective").and_then(serde_json::Value::as_str) else {
            return Ok(ToolExecution::Error(ToolErrorResult {
                message: "CreateGoal requires an 'objective' string argument".to_string(),
            }));
        };
        let objective = objective.trim();
        if objective.is_empty() {
            return Ok(ToolExecution::Error(ToolErrorResult {
                message: "CreateGoal 'objective' must not be empty".to_string(),
            }));
        }
        // Take ownership so the string can outlive `input` and be moved into
        // the execute closure.
        let objective = objective.to_string();
        let criterion = input
            .get("completion_criterion")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);

        let goal = Arc::clone(&self.goal);
        let wire_recorder = self.wire_recorder.clone();
        let event_bus = self.event_bus.clone();
        let plan = ExecutionPlan {
            accesses: Vec::new(),
            display: ToolInputDisplay::GoalStart {
                objective: objective.clone(),
                completion_criterion: criterion.clone(),
            },
            approval_rule: "CreateGoal".to_string(),
            // A new goal changes the agent's direction; later calls in the
            // same batch were planned against the old context.
            stop_batch_after_this: true,
        };

        Ok(ToolExecution::Runnable {
            plan,
            execute: Box::new(move |_ctx| {
                Box::pin(async move {
                    // Write-before-mutate: persist the goal.create op before
                    // touching the in-memory goal, so a crash between the two
                    // leaves the wire log ahead of memory (resume treats the
                    // wire log as the source of truth). Only a goalless
                    // manager can be created into, so a failed create (a
                    // pre-existing goal) records nothing.
                    if goal.get().is_none()
                        && let Some(recorder) = wire_recorder.as_ref()
                    {
                        recorder
                            .record_op(WireOp::GoalCreate {
                                goal_id: format!("goal-{}", uuid::Uuid::new_v4()),
                                objective: objective.clone(),
                                completion_criterion: criterion.clone(),
                                status: Some("active".to_string()),
                                actor: Some("agent".to_string()),
                                budget_limits: None,
                            })
                            .await;
                    }
                    match goal.create(objective, criterion) {
                        Ok(()) => {
                            // Re-read the stored goal so the confirmation
                            // reflects exactly what the manager persisted
                            // (including the default `Active` status).
                            match goal.get() {
                                Some(stored) => {
                                    // Announce the new goal on the session
                                    // bus (the `goal.updated` wire event).
                                    // The tool path is a model-initiated
                                    // mutation, so the actor is `model`.
                                    if let Some(event_bus) = event_bus.as_ref() {
                                        event_bus.publish(SessionEvent::GoalUpdated {
                                            snapshot: goal_snapshot_wire(&stored, &goal),
                                            change: Some(serde_json::json!({
                                                "kind": "lifecycle",
                                                "status": "active",
                                                "actor": "model",
                                            })),
                                        });
                                    }
                                    ToolResult::success(format!(
                                        "Goal created.\nObjective: {}\nStatus: {}\nCompletion criterion: {}",
                                        stored.objective,
                                        status_name(stored.status),
                                        stored.completion_criterion.as_deref().unwrap_or("(none)"),
                                    ))
                                }
                                None => ToolResult::success("Goal created."),
                            }
                        }
                        Err(error) => ToolResult::error(error.to_string()),
                    }
                })
            }),
        })
    }
}

/// Lowercase display name for a [`GoalStatus`], matching its `snake_case`
/// JSON serialization.
fn status_name(status: GoalStatus) -> &'static str {
    match status {
        GoalStatus::Active => "active",
        GoalStatus::Paused => "paused",
        GoalStatus::Complete => "complete",
        GoalStatus::Blocked => "blocked",
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

    async fn execute_tool(tool: &CreateGoalTool, input: serde_json::Value) -> ToolResult {
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
    async fn should_create_an_active_goal_when_given_a_valid_objective() {
        let manager = Arc::new(GoalManager::new());
        let tool = CreateGoalTool::new(Arc::clone(&manager));
        let input = serde_json::json!({
            "objective": "Refactor the scheduler",
            "completion_criterion": "All tests pass"
        });
        let result = execute_tool(&tool, input).await;
        assert!(!result.is_error, "{}", output_text(&result));
        let stored = manager.get().expect("goal should be stored");
        assert_eq!(stored.objective, "Refactor the scheduler");
        assert_eq!(
            stored.completion_criterion.as_deref(),
            Some("All tests pass")
        );
        assert_eq!(stored.status, GoalStatus::Active);
        assert!(stored.budget.is_none());
    }

    #[tokio::test]
    async fn should_store_no_criterion_when_none_is_provided() {
        let manager = Arc::new(GoalManager::new());
        let tool = CreateGoalTool::new(Arc::clone(&manager));
        let input = serde_json::json!({ "objective": "Ship it" });
        let result = execute_tool(&tool, input).await;
        assert!(!result.is_error);
        assert!(manager.get().unwrap().completion_criterion.is_none());
    }

    #[tokio::test]
    async fn should_return_a_phase1_error_when_objective_is_missing() {
        let manager = Arc::new(GoalManager::new());
        let tool = CreateGoalTool::new(Arc::clone(&manager));
        let execution = tool
            .resolve_execution(serde_json::json!({}))
            .expect("resolve");
        let ToolExecution::Error(error) = execution else {
            panic!("expected error execution");
        };
        assert!(error.message.contains("objective"));
        // Nothing was created.
        assert!(manager.get().is_none());
    }

    #[tokio::test]
    async fn should_return_a_phase1_error_when_objective_is_empty() {
        let manager = Arc::new(GoalManager::new());
        let tool = CreateGoalTool::new(Arc::clone(&manager));
        let execution = tool
            .resolve_execution(serde_json::json!({ "objective": "   " }))
            .expect("resolve");
        let ToolExecution::Error(error) = execution else {
            panic!("expected error execution");
        };
        assert!(error.message.contains("empty"));
        assert!(manager.get().is_none());
    }

    #[tokio::test]
    async fn should_return_an_error_when_a_goal_already_exists() {
        let manager = Arc::new(GoalManager::new());
        manager
            .create("First".to_string(), None)
            .expect("first create");
        let tool = CreateGoalTool::new(Arc::clone(&manager));
        let input = serde_json::json!({ "objective": "Second" });
        let result = execute_tool(&tool, input).await;
        assert!(result.is_error);
        assert!(
            output_text(&result).contains("goal already exists"),
            "{}",
            output_text(&result)
        );
        // The original goal is left untouched.
        assert_eq!(manager.get().unwrap().objective, "First");
    }

    #[tokio::test]
    async fn should_stop_the_batch_when_planned() {
        let manager = Arc::new(GoalManager::new());
        let tool = CreateGoalTool::new(Arc::clone(&manager));
        let input = serde_json::json!({ "objective": "Anything" });
        let execution = tool.resolve_execution(input).expect("resolve");
        let ToolExecution::Runnable { plan, .. } = execution else {
            panic!("expected runnable execution");
        };
        assert!(
            plan.stop_batch_after_this,
            "creating a goal should stop the rest of the batch"
        );
    }

    #[tokio::test]
    async fn should_publish_goal_updated_with_model_actor_when_an_event_bus_is_attached() {
        use crate::session::events::{EventBus, SessionEvent};

        let manager = Arc::new(GoalManager::new());
        let bus = Arc::new(EventBus::new(16));
        let mut rx = bus.subscribe();
        let tool = CreateGoalTool::new(Arc::clone(&manager)).with_event_bus(Some(Arc::clone(&bus)));
        let input = serde_json::json!({ "objective": "Refactor the scheduler" });
        let result = execute_tool(&tool, input).await;
        assert!(!result.is_error, "{}", output_text(&result));

        let event = rx.try_recv().expect("goal.updated should be published");
        let SessionEvent::GoalUpdated { snapshot, change } = event else {
            panic!("expected GoalUpdated, got {event:?}");
        };
        assert_eq!(snapshot["objective"], "Refactor the scheduler");
        assert_eq!(snapshot["status"], "active");
        assert_eq!(snapshot["goalId"], "goal-1");
        let change = change.expect("change present");
        assert_eq!(change["kind"], "lifecycle");
        assert_eq!(change["status"], "active");
        assert_eq!(change["actor"], "model");
        // Exactly one announcement.
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn should_not_publish_when_the_create_fails() {
        use crate::session::events::EventBus;

        let manager = Arc::new(GoalManager::new());
        manager
            .create("Existing".to_string(), None)
            .expect("seed goal");
        let bus = Arc::new(EventBus::new(16));
        let mut rx = bus.subscribe();
        let tool = CreateGoalTool::new(Arc::clone(&manager)).with_event_bus(Some(bus));
        let result = execute_tool(&tool, serde_json::json!({ "objective": "Second" })).await;
        assert!(result.is_error);
        // A rejected create publishes nothing — the goal is unchanged.
        assert!(rx.try_recv().is_err(), "no event expected on failure");
    }

    #[tokio::test]
    async fn should_not_panic_without_an_event_bus() {
        let manager = Arc::new(GoalManager::new());
        let tool = CreateGoalTool::new(Arc::clone(&manager));
        let result = execute_tool(&tool, serde_json::json!({ "objective": "Bare" })).await;
        assert!(!result.is_error);
        assert!(manager.get().is_some());
    }
}
