//! `CronDeleteTool` — remove a session-scoped cron task.
//!
//! Wraps [`CronManager::remove`]: the model supplies the task id returned by
//! `CronCreate` (or discovered via `CronList`). Unknown ids are reported as
//! errors so the model can correct itself.

use std::sync::Arc;

use crate::cron::CronManager;
use crate::error::EngineResult;
use crate::tool::{
    ExecutionPlan, Tool, ToolErrorResult, ToolExecution, ToolInputDisplay, ToolResult,
};

/// `CronDelete` — remove a cron task by id.
pub struct CronDeleteTool {
    /// The session-scoped cron manager this tool mutates.
    cron: Arc<CronManager>,
}

impl CronDeleteTool {
    /// Create the tool bound to the session-scoped cron manager.
    pub fn new(cron: Arc<CronManager>) -> Self {
        Self { cron }
    }
}

impl Tool for CronDeleteTool {
    fn name(&self) -> &str {
        "CronDelete"
    }

    fn description(&self) -> &str {
        "Remove a cron task by id. Use CronList to discover task ids. Returns \
         an error when no task with the given id exists."
    }

    fn parameters(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "ID of the cron task to remove."
                    }
                },
                "required": ["id"],
                "additionalProperties": false
            })
        })
    }

    fn resolve_execution(&self, input: serde_json::Value) -> EngineResult<ToolExecution> {
        let Some(id) = input.get("id").and_then(serde_json::Value::as_str) else {
            return Ok(ToolExecution::Error(ToolErrorResult {
                message: "CronDelete requires a string 'id' argument".to_string(),
            }));
        };
        let id = id.to_string();

        let cron_manager = Arc::clone(&self.cron);
        let summary = format!("delete cron task {id}");
        let plan = ExecutionPlan {
            accesses: Vec::new(),
            display: ToolInputDisplay::Generic { summary },
            approval_rule: "CronDelete".to_string(),
            stop_batch_after_this: false,
        };

        Ok(ToolExecution::Runnable {
            plan,
            execute: Box::new(move |_ctx| {
                Box::pin(async move {
                    if !cron_manager.remove(&id).await {
                        return ToolResult::error(format!(
                            "CronDelete failed: task '{id}' not found"
                        ));
                    }
                    let payload = serde_json::json!({
                        "id": id,
                        "deleted": true
                    });
                    ToolResult::success(
                        serde_json::to_string_pretty(&payload)
                            .unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}")),
                    )
                })
            }),
        })
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

    async fn execute_tool(tool: &CronDeleteTool, input: serde_json::Value) -> ToolResult {
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
    async fn should_remove_a_task_when_given_a_known_id() {
        let manager = Arc::new(CronManager::new());
        let id = manager
            .add(
                "0 9 * * *".into(),
                "briefing".into(),
                "s".into(),
                true,
                None,
            )
            .await
            .unwrap();
        let tool = CronDeleteTool::new(Arc::clone(&manager));

        let result = execute_tool(&tool, serde_json::json!({ "id": id })).await;

        assert!(!result.is_error);
        let payload: serde_json::Value = serde_json::from_str(&output_text(&result)).unwrap();
        assert_eq!(payload["id"], id);
        assert_eq!(payload["deleted"], true);
        assert!(manager.get(&id).is_none(), "task is removed from manager");
    }

    #[tokio::test]
    async fn should_return_an_error_result_when_the_id_is_unknown() {
        let manager = Arc::new(CronManager::new());
        let tool = CronDeleteTool::new(Arc::clone(&manager));

        let result = execute_tool(&tool, serde_json::json!({ "id": "cron-nope" })).await;

        assert!(result.is_error);
        assert!(output_text(&result).contains("not found"));
    }

    #[tokio::test]
    async fn should_return_an_execution_error_when_id_argument_is_missing() {
        let manager = Arc::new(CronManager::new());
        let tool = CronDeleteTool::new(Arc::clone(&manager));

        let execution = tool
            .resolve_execution(serde_json::json!({}))
            .expect("resolve");
        let ToolExecution::Error(error) = execution else {
            panic!("expected synthetic error execution");
        };

        assert!(error.message.contains("id"));
    }
}
