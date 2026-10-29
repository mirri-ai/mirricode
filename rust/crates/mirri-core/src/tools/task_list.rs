//! `TaskListTool` — list background tasks.
//!
//! Reads the session's [`TaskManager`] and returns a compact plain-text list
//! of tracked background tasks. Honors `active_only` (default true — list only
//! non-terminal tasks) and `limit` (1–100, default 20). Mirrors the TS
//! `formatTaskList` output: a header line plus per-task `key: value` records
//! (snake_case dialect) carrying kind-specific fields.

use std::sync::Arc;

use crate::error::EngineResult;
use crate::task::{AgentTask, TaskKind, TaskManager};
use crate::tool::{ExecutionPlan, Tool, ToolExecution, ToolInputDisplay, ToolResult};

/// Default number of records TaskList shows.
const DEFAULT_LIMIT: usize = 20;

/// Format a single task as plain-text `key: value` lines (snake_case
/// dialect), carrying the common fields plus the kind-specific ones:
/// process → command/exit_code, agent → agent_id/subagent_type, question →
/// question_count. Fields the engine does not track (detached, timeout_ms,
/// pid, model, tool_call_id) stay absent rather than degraded to nulls.
fn format_task(task: &AgentTask) -> String {
    let info = task.background_task_info();
    let mut lines = vec![
        format!("task_id: {}", task.id),
        format!("description: {}", task.description),
        format!("status: {}", info.status),
        format!("started_at: {}", info.started_at),
    ];
    if let Some(ended_at) = info.ended_at {
        lines.push(format!("ended_at: {ended_at}"));
    }
    if let Some(stop_reason) = &info.stop_reason {
        lines.push(format!("stop_reason: {stop_reason}"));
    }
    lines.push(format!(
        "terminal_notification_suppressed: {}",
        task.notification_suppressed
    ));
    match &task.kind {
        TaskKind::Process { command, .. } => {
            lines.push("kind: process".to_string());
            if let Some(command) = command {
                lines.push(format!("command: {command}"));
            }
            if let Some(exit_code) = info.exit_code {
                lines.push(format!("exit_code: {exit_code}"));
            }
        }
        TaskKind::Agent {
            agent_id,
            subagent_type,
            ..
        } => {
            lines.push("kind: agent".to_string());
            if let Some(agent_id) = agent_id {
                lines.push(format!("agent_id: {agent_id}"));
            }
            if let Some(subagent_type) = subagent_type {
                lines.push(format!("subagent_type: {subagent_type}"));
            }
        }
        TaskKind::Question { question_count } => {
            lines.push("kind: question".to_string());
            lines.push(format!("question_count: {question_count}"));
        }
    }
    lines.join("\n")
}

/// Format the (already filtered/capped) task list as plain text.
///
/// The header distinguishes the active-only view
/// (`active_background_tasks: N`) from the full view
/// (`background_tasks: N`); records are separated by `---`.
fn format_task_list(tasks: &[AgentTask], active_only: bool, limit: usize) -> String {
    let shown: Vec<&AgentTask> = tasks
        .iter()
        .filter(|task| !active_only || !task.status.is_terminal())
        .take(limit)
        .collect();
    let header = if active_only {
        format!("active_background_tasks: {}", shown.len())
    } else {
        format!("background_tasks: {}", shown.len())
    };
    if shown.is_empty() {
        return format!("{header}\nNo background tasks found.");
    }
    let body = shown
        .iter()
        .map(|task| format_task(task))
        .collect::<Vec<_>>()
        .join("\n---\n");
    format!("{header}\n{body}")
}

/// `TaskList` — list running background tasks.
pub struct TaskListTool {
    /// The session-scoped task manager this tool reads.
    tasks: Arc<TaskManager>,
}

impl TaskListTool {
    /// Create the tool bound to the session-scoped task manager.
    pub fn new(tasks: Arc<TaskManager>) -> Self {
        Self { tasks }
    }
}

impl Tool for TaskListTool {
    fn name(&self) -> &str {
        "TaskList"
    }

    fn description(&self) -> &str {
        "List background tasks and their current status. Use this tool to \
         discover which background tasks exist and where each one stands. \
         Returns a task ID, status, and description, plus kind-specific \
         fields (command, PID, exit code for shell tasks; agent ids for \
         subagents). Prefer the default active_only=true. Use the returned \
         ids with TaskStop and TaskOutput."
    }

    fn parameters(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "active_only": {
                        "type": "boolean",
                        "default": true,
                        "description": "When true (default), list only tasks that have not reached a terminal status. Set false to also include finished and lost tasks."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 100,
                        "default": 20,
                        "description": "Maximum number of tasks to return. Accepts 1 to 100; defaults to 20."
                    }
                },
                "additionalProperties": false
            })
        })
    }

    fn resolve_execution(&self, input: serde_json::Value) -> EngineResult<ToolExecution> {
        let active_only = input
            .get("active_only")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        let limit = input
            .get("limit")
            .and_then(serde_json::Value::as_u64)
            .map(|v| v as usize)
            .unwrap_or(DEFAULT_LIMIT);

        let tasks = Arc::clone(&self.tasks);
        let plan = ExecutionPlan {
            accesses: Vec::new(),
            display: ToolInputDisplay::BackgroundTask,
            approval_rule: "TaskList".to_string(),
            stop_batch_after_this: false,
        };
        Ok(ToolExecution::Runnable {
            plan,
            execute: Box::new(move |_ctx| {
                Box::pin(async move {
                    let entries = tasks.list();
                    ToolResult::success(format_task_list(&entries, active_only, limit))
                })
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolOutput;

    fn execute_tool(tool: &TaskListTool) -> ToolResult {
        execute_tool_with_input(tool, serde_json::json!({}))
    }

    fn execute_tool_with_input(tool: &TaskListTool, input: serde_json::Value) -> ToolResult {
        let execution = tool.resolve_execution(input).unwrap();
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
        futures::executor::block_on(execute(ctx))
    }

    fn output_text(result: &ToolResult) -> String {
        match &result.output {
            ToolOutput::Text(text) => text.clone(),
            ToolOutput::Parts(_) => panic!("expected text output"),
        }
    }

    #[tokio::test]
    async fn should_list_running_tasks_with_common_fields() {
        let tasks = Arc::new(TaskManager::new());
        tasks.register(
            crate::task::TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "build watcher".to_string(),
            "session-1".to_string(),
        );
        let tool = TaskListTool::new(tasks);
        let result = execute_tool(&tool);
        assert!(!result.is_error);
        let text = output_text(&result);
        assert!(text.contains("task_id:"), "{text}");
        assert!(text.contains("description: build watcher"), "{text}");
        assert!(text.contains("status: running"), "{text}");
    }

    #[tokio::test]
    async fn should_only_list_running_tasks_when_active_only_is_true() {
        let tasks = Arc::new(TaskManager::new());
        let running_id = tasks.register(
            crate::task::TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "run".into(),
            "s".into(),
        );
        let done_id = tasks.register(
            crate::task::TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "done".into(),
            "s".into(),
        );
        tasks.complete(&done_id, 0);
        let tool = TaskListTool::new(Arc::clone(&tasks));
        let result = execute_tool(&tool);
        let text = output_text(&result);
        assert!(
            text.starts_with("active_background_tasks: 1"),
            "header: {text}"
        );
        assert!(text.contains(&running_id), "running present: {text}");
        assert!(!text.contains(&done_id), "completed hidden: {text}");
    }

    #[tokio::test]
    async fn should_include_completed_tasks_when_active_only_is_false() {
        let tasks = Arc::new(TaskManager::new());
        let done_id = tasks.register(
            crate::task::TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "done".into(),
            "s".into(),
        );
        tasks.complete(&done_id, 0);
        let tool = TaskListTool::new(Arc::clone(&tasks));
        let result = execute_tool_with_input(&tool, serde_json::json!({ "active_only": false }));
        let text = output_text(&result);
        assert!(text.starts_with("background_tasks: 1"), "header: {text}");
        assert!(
            text.contains(&done_id),
            "completed shown when active_only=false: {text}"
        );
    }

    #[tokio::test]
    async fn should_respect_limit_when_listing_tasks() {
        let tasks = Arc::new(TaskManager::new());
        for i in 0..5 {
            tasks.register(
                crate::task::TaskKind::Process {
                    command: None,
                    dispatcher: None,
                },
                format!("task {i}"),
                "s".to_string(),
            );
        }
        let tool = TaskListTool::new(Arc::clone(&tasks));
        let result = execute_tool_with_input(&tool, serde_json::json!({ "limit": 2 }));
        let text = output_text(&result);
        assert!(
            text.starts_with("active_background_tasks: 2"),
            "limit: {text}"
        );
    }

    #[tokio::test]
    async fn should_include_kind_specific_fields_when_listing() {
        let tasks = Arc::new(TaskManager::new());
        let id = tasks.register(
            crate::task::TaskKind::Process {
                command: Some("echo hi".into()),
                dispatcher: None,
            },
            "p".into(),
            "s".into(),
        );
        tasks.complete(&id, 0);
        let tool = TaskListTool::new(Arc::clone(&tasks));
        let result = execute_tool_with_input(&tool, serde_json::json!({ "active_only": false }));
        let text = output_text(&result);
        assert!(text.contains("kind: process"), "{text}");
        assert!(text.contains("command: echo hi"), "{text}");
        assert!(
            text.contains("exit_code"),
            "exit_code expected for completed process: {text}"
        );
    }

    #[tokio::test]
    async fn should_return_empty_message_when_no_active_tasks_match() {
        let tasks = Arc::new(TaskManager::new());
        let done_id = tasks.register(
            crate::task::TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "done".into(),
            "s".into(),
        );
        tasks.complete(&done_id, 0);
        let tool = TaskListTool::new(Arc::clone(&tasks));
        let result = execute_tool(&tool);
        let text = output_text(&result);
        assert!(text.contains("active_background_tasks: 0"), "{text}");
        assert!(text.contains("No background tasks found."), "{text}");
    }
}
