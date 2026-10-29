//! `CronListTool` — list session-scoped cron tasks.
//!
//! Wraps [`CronManager::list`]: returns every registered cron task (id, cron
//! expression, prompt, schedule, and next fire time). The cron manager is
//! session-scoped, so the list includes tasks created by any agent in the
//! session.

use std::sync::Arc;

use crate::cron::{CronManager, CronTimezone, format_local_iso_with_offset};
use crate::error::EngineResult;
use crate::tool::{ExecutionPlan, Tool, ToolExecution, ToolInputDisplay, ToolResult};

/// `CronList` — list all registered cron tasks.
pub struct CronListTool {
    /// The session-scoped cron manager this tool reads.
    cron: Arc<CronManager>,
}

impl CronListTool {
    /// Create the tool bound to the session-scoped cron manager.
    pub fn new(cron: Arc<CronManager>) -> Self {
        Self { cron }
    }
}

impl Tool for CronListTool {
    fn name(&self) -> &str {
        "CronList"
    }

    fn description(&self) -> &str {
        "List all registered cron tasks (their id, cron expression, prompt, \
         and next fire time). Use the returned ids with CronDelete."
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
        let cron_manager = Arc::clone(&self.cron);
        let plan = ExecutionPlan {
            accesses: Vec::new(),
            display: ToolInputDisplay::Generic {
                summary: "list cron tasks".to_string(),
            },
            approval_rule: "CronList".to_string(),
            stop_batch_after_this: false,
        };

        Ok(ToolExecution::Runnable {
            plan,
            execute: Box::new(move |_ctx| {
                Box::pin(async move {
                    let now = chrono::Utc::now();
                    let entries: Vec<serde_json::Value> = cron_manager
                        .list()
                        .iter()
                        .map(|task| {
                            // The next fire renders as a local ISO instant
                            // with a numeric offset in the task's calendar
                            // (the TS `formatLocalIsoWithOffset` shape).
                            let tz = CronTimezone::parse(task.timezone.as_deref())
                                .map(|declared| declared.resolve())
                                .unwrap_or_else(|_| CronTimezone::Local.resolve());
                            serde_json::json!({
                                "id": task.id,
                                "cron": task.cron,
                                "prompt": task.prompt,
                                "recurring": task.recurring,
                                "timezone": task.timezone,
                                "stale": cron_manager.is_stale(task, now),
                                "createdAt": task.created_at,
                                "lastFired": task.last_fired,
                                "nextFireAt": task
                                    .next_fire_at
                                    .map(|at| format_local_iso_with_offset(at, tz)),
                            })
                        })
                        .collect();
                    let payload = serde_json::json!({ "tasks": entries });
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

    async fn execute_tool(tool: &CronListTool) -> ToolResult {
        let execution = tool
            .resolve_execution(serde_json::json!({}))
            .expect("resolve");
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
    async fn should_return_an_empty_list_when_no_tasks_are_registered() {
        let tool = CronListTool::new(Arc::new(CronManager::new()));

        let result = execute_tool(&tool).await;

        assert!(!result.is_error);
        let payload: serde_json::Value = serde_json::from_str(&output_text(&result)).unwrap();
        assert_eq!(payload["tasks"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn should_return_all_registered_tasks_when_tasks_exist() {
        let manager = Arc::new(CronManager::new());
        manager
            .add(
                "0 9 * * *".into(),
                "morning".into(),
                "s".into(),
                true,
                Some("Asia/Shanghai".to_string()),
            )
            .await
            .unwrap();
        manager
            .add("*/5 * * * *".into(), "tick".into(), "s".into(), false, None)
            .await
            .unwrap();
        let tool = CronListTool::new(Arc::clone(&manager));

        let result = execute_tool(&tool).await;

        assert!(!result.is_error);
        let payload: serde_json::Value = serde_json::from_str(&output_text(&result)).unwrap();
        let tasks = payload["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 2);
        assert!(tasks.iter().any(|t| t["cron"] == "0 9 * * *"));
        assert!(tasks.iter().any(|t| t["cron"] == "*/5 * * * *"));
        assert!(
            tasks.iter().all(|t| t["nextFireAt"].is_string()),
            "every task has a next fire time"
        );
    }

    #[tokio::test]
    async fn should_expose_timezone_and_staleness_when_tasks_are_listed() {
        let manager = Arc::new(CronManager::new());
        manager
            .add(
                "0 9 * * *".into(),
                "morning".into(),
                "s".into(),
                true,
                Some("Asia/Shanghai".to_string()),
            )
            .await
            .unwrap();
        let tool = CronListTool::new(Arc::clone(&manager));

        let result = execute_tool(&tool).await;

        assert!(!result.is_error);
        let payload: serde_json::Value = serde_json::from_str(&output_text(&result)).unwrap();
        let entry = &payload["tasks"][0];
        assert_eq!(entry["timezone"], "Asia/Shanghai");
        assert_eq!(entry["stale"], false, "a fresh task is not stale");
        let next = entry["nextFireAt"].as_str().unwrap();
        assert!(
            next.ends_with("+08:00"),
            "nextFireAt in the task's calendar: {next}"
        );
    }
}
