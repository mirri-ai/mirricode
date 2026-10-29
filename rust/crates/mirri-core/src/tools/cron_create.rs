//! `CronCreateTool` — schedule a session-scoped cron task.
//!
//! Wraps [`CronManager::add`]: the model supplies a 5-field cron expression,
//! a prompt to inject when the task fires, and whether it should recur. The
//! tool registers the task on the session-scoped [`CronManager`] and returns
//! the generated id and next fire time.
//!
//! The `session_id` is baked in at construction time so the model never has
//! to supply it; every task created through this tool belongs to the same
//! session.

use std::sync::Arc;

use crate::cron::{CronManager, CronTimezone, format_local_iso_with_offset};
use crate::error::EngineResult;
use crate::tool::{
    ExecutionPlan, Tool, ToolErrorResult, ToolExecution, ToolInputDisplay, ToolResult,
};

/// `CronCreate` — schedule a cron task that injects a prompt into the session.
pub struct CronCreateTool {
    /// The session-scoped cron manager this tool mutates.
    cron: Arc<CronManager>,
    /// The session this tool's tasks belong to.
    session_id: String,
}

impl CronCreateTool {
    /// Create the tool bound to the session-scoped cron manager.
    ///
    /// The `session_id` is captured at construction time so the model never
    /// has to supply it; every task created through this tool belongs to the
    /// same session.
    pub fn new(cron: Arc<CronManager>, session_id: String) -> Self {
        Self { cron, session_id }
    }
}

impl Tool for CronCreateTool {
    fn name(&self) -> &str {
        "CronCreate"
    }

    fn description(&self) -> &str {
        "Schedule a cron task that injects a prompt into the session at the \
         times specified by a standard 5-field cron expression (minute hour \
         day-of-month month day-of-week) evaluated in the host's local \
         timezone, or the explicit timezone argument when one is given. Set \
         recurring=false for a one-shot reminder."
    }

    fn parameters(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "cron": {
                        "type": "string",
                        "description": "Standard 5-field cron expression (minute hour day-of-month month day-of-week) evaluated in the host's local timezone (or the explicit timezone argument)."
                    },
                    "prompt": {
                        "type": "string",
                        "description": "The prompt to inject into the session when the task fires."
                    },
                    "recurring": {
                        "type": "boolean",
                        "default": true,
                        "description": "Whether the task reschedules itself after firing (true) or is removed after its first fire (false). Defaults to true."
                    },
                    "timezone": {
                        "type": "string",
                        "description": "Optional IANA timezone name (e.g. 'Asia/Shanghai') the cron expression is evaluated in. Defaults to the host's local timezone."
                    }
                },
                "required": ["cron", "prompt"],
                "additionalProperties": false
            })
        })
    }

    fn resolve_execution(&self, input: serde_json::Value) -> EngineResult<ToolExecution> {
        let Some(cron_expr) = input.get("cron").and_then(serde_json::Value::as_str) else {
            return Ok(ToolExecution::Error(ToolErrorResult {
                message: "CronCreate requires a string 'cron' argument".to_string(),
            }));
        };
        let Some(prompt) = input.get("prompt").and_then(serde_json::Value::as_str) else {
            return Ok(ToolExecution::Error(ToolErrorResult {
                message: "CronCreate requires a string 'prompt' argument".to_string(),
            }));
        };
        // `recurring` defaults to true when absent so a bare cron + prompt is
        // treated as a recurring schedule, matching the TS CronCreate tool.
        let recurring = input
            .get("recurring")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        // An optional IANA name the expression is evaluated in; absent or
        // null means the host's local calendar (B5 / DEC-3).
        let timezone = input
            .get("timezone")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);

        let cron_expr = cron_expr.to_string();
        let prompt = prompt.to_string();
        let summary = match &timezone {
            Some(timezone) => format!("cron {cron_expr} [{timezone}]: {prompt}"),
            None => format!("cron {cron_expr}: {prompt}"),
        };

        let cron_manager = Arc::clone(&self.cron);
        let session_id = self.session_id.clone();
        let plan = ExecutionPlan {
            accesses: Vec::new(),
            display: ToolInputDisplay::Generic { summary },
            approval_rule: "CronCreate".to_string(),
            stop_batch_after_this: false,
        };

        Ok(ToolExecution::Runnable {
            plan,
            execute: Box::new(move |_ctx| {
                Box::pin(async move {
                    match cron_manager
                        .add(cron_expr, prompt, session_id, recurring, timezone)
                        .await
                    {
                        Ok(id) => {
                            // `add` inserts the task and returns its id, but a
                            // concurrent `check_due` or `CronDeleteTool` can
                            // remove the task between the two locks. Treat a
                            // missing task as a graceful error rather than
                            // panicking.
                            let task = match cron_manager.get(&id) {
                                Some(t) => t,
                                None => {
                                    return ToolResult::error(
                                        "Scheduled task was removed before its details could be retrieved.".to_string(),
                                    );
                                }
                            };
                            // The next fire is reported as a local ISO instant
                            // with a numeric offset (post-jitter, in the
                            // task's calendar) — the shape the TS
                            // `formatLocalIsoWithOffset` produces.
                            let tz = CronTimezone::parse(task.timezone.as_deref())
                                .map(|declared| declared.resolve())
                                .unwrap_or_else(|_| CronTimezone::Local.resolve());
                            let mut payload = serde_json::json!({
                                "id": task.id,
                                "cron": task.cron,
                                "recurring": task.recurring,
                                "nextFireAt": task
                                    .next_fire_at
                                    .map(|at| format_local_iso_with_offset(at, tz)),
                            });
                            if let Some(timezone) = &task.timezone {
                                payload["timezone"] = serde_json::json!(timezone);
                            }
                            ToolResult::success(
                                serde_json::to_string_pretty(&payload)
                                    .unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}")),
                            )
                        }
                        Err(error) => ToolResult::error(format!("CronCreate failed: {error}")),
                    }
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

    async fn execute_tool(tool: &CronCreateTool, input: serde_json::Value) -> ToolResult {
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
    async fn should_register_a_recurring_task_when_given_a_valid_cron_expression() {
        let manager = Arc::new(CronManager::new());
        let tool = CronCreateTool::new(Arc::clone(&manager), "test-session".to_string());
        let input = serde_json::json!({
            "cron": "0 9 * * *",
            "prompt": "morning briefing",
            "recurring": true
        });

        let result = execute_tool(&tool, input).await;

        assert!(!result.is_error);
        let payload: serde_json::Value = serde_json::from_str(&output_text(&result)).unwrap();
        let id = payload["id"].as_str().expect("id present");
        assert!(id.starts_with("cron-"));
        assert_eq!(payload["cron"], "0 9 * * *");
        assert_eq!(payload["recurring"], true);
        assert!(
            payload["nextFireAt"].is_string(),
            "nextFireAt is a timestamp"
        );
        // The task is stored on the shared manager with the baked-in session id.
        let task = manager.get(id).expect("task is registered");
        assert_eq!(task.prompt, "morning briefing");
        assert_eq!(task.session_id, "test-session");
    }

    #[tokio::test]
    async fn should_default_to_recurring_when_recurring_is_omitted() {
        let manager = Arc::new(CronManager::new());
        let tool = CronCreateTool::new(Arc::clone(&manager), "test-session".to_string());
        let input = serde_json::json!({
            "cron": "*/5 * * * *",
            "prompt": "tick"
        });

        let result = execute_tool(&tool, input).await;

        assert!(!result.is_error);
        let payload: serde_json::Value = serde_json::from_str(&output_text(&result)).unwrap();
        assert_eq!(payload["recurring"], true);
        let id = payload["id"].as_str().unwrap();
        assert!(manager.get(id).unwrap().recurring);
    }

    #[tokio::test]
    async fn should_register_a_one_shot_task_when_recurring_is_false() {
        let manager = Arc::new(CronManager::new());
        let tool = CronCreateTool::new(Arc::clone(&manager), "test-session".to_string());
        let input = serde_json::json!({
            "cron": "*/5 * * * *",
            "prompt": "once",
            "recurring": false
        });

        let result = execute_tool(&tool, input).await;

        assert!(!result.is_error);
        let payload: serde_json::Value = serde_json::from_str(&output_text(&result)).unwrap();
        assert_eq!(payload["recurring"], false);
        let id = payload["id"].as_str().unwrap();
        assert!(!manager.get(id).unwrap().recurring);
    }

    #[tokio::test]
    async fn should_return_an_execution_error_when_cron_argument_is_missing() {
        let manager = Arc::new(CronManager::new());
        let tool = CronCreateTool::new(Arc::clone(&manager), "test-session".to_string());
        let input = serde_json::json!({ "prompt": "hello" });

        let execution = tool.resolve_execution(input).expect("resolve");
        let ToolExecution::Error(error) = execution else {
            panic!("expected synthetic error execution");
        };

        assert!(error.message.contains("cron"));
        assert!(
            manager.list().is_empty(),
            "no task registered on phase-1 error"
        );
    }

    #[tokio::test]
    async fn should_return_an_execution_error_when_prompt_argument_is_missing() {
        let manager = Arc::new(CronManager::new());
        let tool = CronCreateTool::new(Arc::clone(&manager), "test-session".to_string());
        let input = serde_json::json!({ "cron": "0 9 * * *" });

        let execution = tool.resolve_execution(input).expect("resolve");
        let ToolExecution::Error(error) = execution else {
            panic!("expected synthetic error execution");
        };

        assert!(error.message.contains("prompt"));
        assert!(manager.list().is_empty());
    }

    #[tokio::test]
    async fn should_return_an_error_result_when_the_cron_expression_is_invalid() {
        let manager = Arc::new(CronManager::new());
        let tool = CronCreateTool::new(Arc::clone(&manager), "test-session".to_string());
        let input = serde_json::json!({
            "cron": "not a cron",
            "prompt": "boom"
        });

        let result = execute_tool(&tool, input).await;

        assert!(result.is_error);
        assert!(output_text(&result).contains("CronCreate failed"));
        assert!(manager.list().is_empty(), "no task registered on failure");
    }

    // -----------------------------------------------------------------------
    // timezone argument (B5 T9)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn should_register_a_task_with_an_explicit_timezone_when_timezone_is_given() {
        let manager = Arc::new(CronManager::new());
        let tool = CronCreateTool::new(Arc::clone(&manager), "test-session".to_string());
        let input = serde_json::json!({
            "cron": "0 9 * * *",
            "prompt": "morning briefing",
            "timezone": "Asia/Shanghai"
        });

        let result = execute_tool(&tool, input).await;

        assert!(!result.is_error);
        let payload: serde_json::Value = serde_json::from_str(&output_text(&result)).unwrap();
        assert_eq!(payload["timezone"], "Asia/Shanghai");
        // nextFireAt is local ISO with offset (9am Shanghai, +/- the
        // deterministic per-id jitter)
        let next = payload["nextFireAt"].as_str().unwrap();
        assert!(next.ends_with("+08:00"), "local ISO with offset: {next}");
        let task = manager.get(payload["id"].as_str().unwrap()).unwrap();
        assert_eq!(task.timezone.as_deref(), Some("Asia/Shanghai"));
    }

    #[tokio::test]
    async fn should_return_an_error_result_when_the_timezone_is_unknown() {
        let manager = Arc::new(CronManager::new());
        let tool = CronCreateTool::new(Arc::clone(&manager), "test-session".to_string());
        let input = serde_json::json!({
            "cron": "0 9 * * *",
            "prompt": "p",
            "timezone": "Not/A_Zone"
        });

        let result = execute_tool(&tool, input).await;

        assert!(result.is_error);
        assert!(output_text(&result).contains("timezone"));
        assert!(manager.list().is_empty(), "no task registered on failure");
    }

    #[tokio::test]
    async fn should_default_to_local_when_timezone_is_omitted() {
        let manager = Arc::new(CronManager::new());
        let tool = CronCreateTool::new(Arc::clone(&manager), "test-session".to_string());

        let result = execute_tool(
            &tool,
            serde_json::json!({
                "cron": "*/5 * * * *", "prompt": "tick"
            }),
        )
        .await;

        let payload: serde_json::Value = serde_json::from_str(&output_text(&result)).unwrap();
        assert!(
            payload.get("timezone").is_none() || payload["timezone"].is_null(),
            "no explicit timezone in the payload"
        );
        let task = manager.get(payload["id"].as_str().unwrap()).unwrap();
        assert_eq!(task.timezone, None);
    }
}
