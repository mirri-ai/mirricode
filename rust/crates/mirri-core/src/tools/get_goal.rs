//! `GetGoalTool` — read the current session goal.
//!
//! Returns the active goal's objective, status, completion criterion, and
//! budget, or a "no active goal" message when none exists. Read-only: it
//! never mutates the [`GoalManager`].

use std::sync::Arc;

use crate::error::EngineResult;
use crate::goal::{Goal, GoalBudget, GoalManager, GoalStatus};
use crate::tool::{ExecutionPlan, Tool, ToolExecution, ToolInputDisplay, ToolResult};

/// `GetGoal` — get the current session goal.
///
/// Bound to the session-scoped [`GoalManager`], so the returned state is the
/// same every agent in the session sees.
pub struct GetGoalTool {
    /// The session-scoped goal manager this tool reads.
    goal: Arc<GoalManager>,
}

impl GetGoalTool {
    /// Create the tool bound to the session-scoped goal manager.
    pub fn new(goal: Arc<GoalManager>) -> Self {
        Self { goal }
    }
}

impl Tool for GetGoalTool {
    fn name(&self) -> &str {
        "GetGoal"
    }

    fn description(&self) -> &str {
        "Get the current session goal (objective, status, completion criterion, \
         and budget). Returns a message when no goal is active."
    }

    fn parameters(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            })
        })
    }

    fn resolve_execution(&self, _input: serde_json::Value) -> EngineResult<ToolExecution> {
        let goal = Arc::clone(&self.goal);
        let plan = ExecutionPlan {
            accesses: Vec::new(),
            display: ToolInputDisplay::Generic {
                summary: "Get current goal".to_string(),
            },
            approval_rule: "GetGoal".to_string(),
            stop_batch_after_this: false,
        };

        Ok(ToolExecution::Runnable {
            plan,
            execute: Box::new(move |_ctx| {
                Box::pin(async move {
                    match goal.get() {
                        None => ToolResult::success("No active goal."),
                        Some(stored) => ToolResult::success(render_goal(&stored)),
                    }
                })
            }),
        })
    }
}

/// Render a [`Goal`] as a compact, human-readable summary for the model.
fn render_goal(goal: &Goal) -> String {
    [
        format!("Objective: {}", goal.objective),
        format!("Status: {}", status_name(goal.status)),
        format!(
            "Completion criterion: {}",
            goal.completion_criterion.as_deref().unwrap_or("(none)"),
        ),
        format!("Budget: {}", render_budget(goal.budget)),
    ]
    .join("\n")
}

/// Render an optional [`GoalBudget`] as a comma-separated `key=value` list.
///
/// `None` renders as `(none)`; a budget with every field absent renders as
/// `(unlimited)` since an all-`None` budget imposes no limits.
fn render_budget(budget: Option<GoalBudget>) -> String {
    match budget {
        None => "(none)".to_string(),
        Some(b) => {
            let mut parts = Vec::new();
            if let Some(turns) = b.turns {
                parts.push(format!("turns={turns}"));
            }
            if let Some(tokens) = b.tokens {
                parts.push(format!("tokens={tokens}"));
            }
            if let Some(ms) = b.wall_clock_ms {
                parts.push(format!("wall_clock_ms={ms}"));
            }
            if parts.is_empty() {
                "(unlimited)".to_string()
            } else {
                parts.join(", ")
            }
        }
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
    use crate::tool::ToolOutput;

    fn output_text(result: &ToolResult) -> String {
        match &result.output {
            ToolOutput::Text(text) => text.clone(),
            ToolOutput::Parts(_) => panic!("expected text output"),
        }
    }

    async fn execute_tool(tool: &GetGoalTool) -> ToolResult {
        let execution = tool
            .resolve_execution(serde_json::json!({}))
            .expect("resolve");
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
    async fn should_report_no_active_goal_when_none_exists() {
        let tool = GetGoalTool::new(Arc::new(GoalManager::new()));
        let result = execute_tool(&tool).await;
        assert!(!result.is_error);
        assert_eq!(output_text(&result), "No active goal.");
    }

    #[tokio::test]
    async fn should_return_goal_details_when_a_goal_exists() {
        let manager = Arc::new(GoalManager::new());
        manager
            .create(
                "Refactor scheduler".to_string(),
                Some("Tests pass".to_string()),
            )
            .unwrap();
        let tool = GetGoalTool::new(Arc::clone(&manager));
        let result = execute_tool(&tool).await;
        assert!(!result.is_error);
        let text = output_text(&result);
        assert!(text.contains("Objective: Refactor scheduler"), "{text}");
        assert!(text.contains("Status: active"), "{text}");
        assert!(text.contains("Completion criterion: Tests pass"), "{text}");
        assert!(text.contains("Budget: (none)"), "{text}");
    }

    #[tokio::test]
    async fn should_show_the_budget_when_one_is_set() {
        let manager = Arc::new(GoalManager::new());
        manager.create("Bounded work".to_string(), None).unwrap();
        manager.set_budget(GoalBudget {
            turns: Some(10),
            tokens: None,
            wall_clock_ms: Some(60_000),
        });
        let tool = GetGoalTool::new(Arc::clone(&manager));
        let result = execute_tool(&tool).await;
        let text = output_text(&result);
        assert!(text.contains("turns=10"), "{text}");
        assert!(text.contains("wall_clock_ms=60000"), "{text}");
        // The unset token budget must not appear.
        assert!(!text.contains("tokens="), "{text}");
    }

    #[tokio::test]
    async fn should_not_stop_the_batch_when_planned() {
        let tool = GetGoalTool::new(Arc::new(GoalManager::new()));
        let execution = tool
            .resolve_execution(serde_json::json!({}))
            .expect("resolve");
        let ToolExecution::Runnable { plan, .. } = execution else {
            panic!("expected runnable execution");
        };
        assert!(
            !plan.stop_batch_after_this,
            "GetGoal is read-only and should not stop the batch"
        );
    }
}
