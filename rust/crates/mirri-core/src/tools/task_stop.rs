//! `TaskStopTool` — stop a background task.
//!
//! Stops the task **gracefully** (D-63): the cancel signal (when the
//! registering call site attached one — the Agent tool and background Bash
//! both do) fires first, and the task gets the kill-grace window to settle
//! naturally. A task that completes or fails inside the window keeps that
//! honest terminal status; only an unsettled task escalates to
//! [`TaskStatus::Stopped`] (wire: `killed`). Tasks already in a terminal
//! status are reported as-is without re-stopping; a task stopped here keeps
//! that terminal status against any later settle (first terminal wins).
//! Unknown task ids are reported as errors.

use std::sync::Arc;
use std::time::Duration;

use crate::error::EngineResult;
use crate::task::{TaskManager, TaskStatus};
use crate::tool::{ExecutionPlan, Tool, ToolExecution, ToolInputDisplay, ToolResult};
use crate::tools::tool_execution_error;

/// Format a [`TaskStatus`] as a lowercase label for plain-text output.
fn status_label(status: &TaskStatus) -> String {
    match status {
        TaskStatus::Running => "running".to_string(),
        TaskStatus::Completed { exit_code } => format!("completed (exit_code: {exit_code})"),
        TaskStatus::Failed(detail) => format!("failed: {detail}"),
        TaskStatus::Stopped => "stopped".to_string(),
    }
}

/// `TaskStop` — stop a background task.
pub struct TaskStopTool {
    /// The session-scoped task manager this tool mutates.
    tasks: Arc<TaskManager>,
    /// The grace window before an unsettled stop escalates to
    /// `stopped`/killed (D-63). The server assembly overrides this with the
    /// configured `task.killGracePeriodMs`; the default is the TS
    /// `taskService` 5s.
    grace: Duration,
}

impl TaskStopTool {
    /// Create the tool bound to the session-scoped task manager, with the
    /// default 5s kill-grace window.
    pub fn new(tasks: Arc<TaskManager>) -> Self {
        Self {
            tasks,
            grace: Duration::from_millis(crate::config::DEFAULT_KILL_GRACE_MS),
        }
    }

    /// Override the kill-grace window (the server assembly injects the
    /// configured `task.killGracePeriodMs` here).
    pub fn with_grace(mut self, grace: Duration) -> Self {
        self.grace = grace;
        self
    }
}

impl Tool for TaskStopTool {
    fn name(&self) -> &str {
        "TaskStop"
    }

    fn description(&self) -> &str {
        "Stop a background task by id — marks it stopped and cancels the \
         underlying work when the task carries a cancel signal (agent \
         tasks do). Use TaskList to discover task ids."
    }

    fn parameters(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "task_id": {
                        "type": "string",
                        "description": "ID of the background task to stop."
                    }
                },
                "required": ["task_id"],
                "additionalProperties": false
            })
        })
    }

    fn resolve_execution(&self, input: serde_json::Value) -> EngineResult<ToolExecution> {
        let Some(task_id) = input.get("task_id").and_then(serde_json::Value::as_str) else {
            return Ok(tool_execution_error(
                "TaskStop requires a string 'task_id' argument",
            ));
        };
        let task_id = task_id.to_string();

        let tasks = Arc::clone(&self.tasks);
        let plan = ExecutionPlan {
            accesses: Vec::new(),
            display: ToolInputDisplay::TaskStop {
                task_id: task_id.clone(),
            },
            approval_rule: "TaskStop".to_string(),
            stop_batch_after_this: false,
        };

        let grace = self.grace;
        Ok(ToolExecution::Runnable {
            plan,
            execute: Box::new(move |_ctx| {
                let tasks = Arc::clone(&tasks);
                Box::pin(async move {
                    let Some(task) = tasks.get(&task_id) else {
                        return ToolResult::error(format!("Task not found: {task_id}"));
                    };

                    // If the task is already terminal, report its current
                    // status without re-stopping — matching the TS engine.
                    if task.status.is_terminal() {
                        let output = format!(
                            "task_id: {}\nstatus: {}\nreason: Task already in terminal state",
                            task_id,
                            status_label(&task.status)
                        );
                        return ToolResult::success(output);
                    }

                    // G25: the tool result is the model's only answer for
                    // this task — suppress the terminal notification BEFORE
                    // the stop (TS taskStopTool's order), so a task that
                    // settles inside the grace window never notifies either.
                    tasks.suppress_terminal_notification(&task_id);
                    // Graceful stop (D-63): fire the cancel signal and give
                    // the work the grace window to settle naturally — a task
                    // that completes/fails in-window keeps that honest
                    // terminal status; only an unsettled task escalates to
                    // `stopped` (wire: killed).
                    tasks.stop_with_grace(&task_id, grace).await;
                    let settled = tasks.get(&task_id).map(|task| task.status.clone());
                    let Some(status) = settled else {
                        return ToolResult::error(format!("Task not found: {task_id}"));
                    };
                    let output = format!(
                        "task_id: {}\nstatus: {}\nreason: {}",
                        task_id,
                        status_label(&status),
                        if status == TaskStatus::Stopped {
                            "Stopped by TaskStop (grace window expired)"
                        } else {
                            "Settled naturally within the stop grace window"
                        }
                    );
                    ToolResult::success(output)
                })
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::TaskKind;
    use crate::tool::ToolOutput;

    fn output_text(result: &ToolResult) -> String {
        match &result.output {
            ToolOutput::Text(text) => text.clone(),
            ToolOutput::Parts(_) => panic!("expected text output"),
        }
    }

    async fn execute_tool(tool: &TaskStopTool, input: serde_json::Value) -> ToolResult {
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
    async fn should_mark_the_task_as_stopped_when_stopping_a_known_task() {
        let tasks = Arc::new(TaskManager::new());
        let task_id = tasks.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "slow build".to_string(),
            "session-1".to_string(),
        );
        let tool = TaskStopTool::new(Arc::clone(&tasks));
        let result = execute_tool(&tool, serde_json::json!({ "task_id": task_id })).await;
        assert!(!result.is_error);
        let text = output_text(&result);
        assert!(text.contains(&format!("task_id: {task_id}")), "{text}");
        assert!(text.contains("status: stopped"), "{text}");
        assert!(text.contains("reason: Stopped by TaskStop"), "{text}");
        assert_eq!(tasks.get(&task_id).unwrap().status, TaskStatus::Stopped);
    }

    #[tokio::test]
    async fn should_return_error_result_when_stopping_an_unknown_task() {
        let tasks = Arc::new(TaskManager::new());
        let tool = TaskStopTool::new(Arc::clone(&tasks));
        let result = execute_tool(&tool, serde_json::json!({ "task_id": "task-nope" })).await;
        assert!(result.is_error);
        assert!(output_text(&result).contains("not found"));
    }

    #[tokio::test]
    async fn should_report_terminal_status_when_stopping_an_already_completed_task() {
        let tasks = Arc::new(TaskManager::new());
        let task_id = tasks.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "done build".to_string(),
            "session-1".to_string(),
        );
        tasks.complete(&task_id, 0);
        let tool = TaskStopTool::new(Arc::clone(&tasks));
        let result = execute_tool(&tool, serde_json::json!({ "task_id": task_id })).await;
        assert!(!result.is_error);
        let text = output_text(&result);
        assert!(text.contains(&format!("task_id: {task_id}")), "{text}");
        assert!(text.contains("status: completed (exit_code: 0)"), "{text}");
        assert!(
            text.contains("reason: Task already in terminal state"),
            "{text}"
        );
        // Status should remain Completed, not overwritten to Stopped.
        assert_eq!(
            tasks.get(&task_id).unwrap().status,
            TaskStatus::Completed { exit_code: 0 }
        );
    }

    /// Given a running background task stopped through the TaskStop tool,
    /// When the stop settles, Then the task's terminal notification is
    /// suppressed — the tool result is the model's only answer (G25), and
    /// the task never enters the due-notification set.
    #[tokio::test]
    async fn should_suppress_the_terminal_notification_when_stopping_a_task() {
        let tasks = Arc::new(TaskManager::new());
        let task_id = tasks
            .register_background(
                TaskKind::Process {
                    command: None,
                    dispatcher: None,
                },
                "stoppable".into(),
                "session-1".into(),
                None,
            )
            .unwrap();
        let tool = TaskStopTool::new(Arc::clone(&tasks)).with_grace(Duration::from_millis(20));
        let result = execute_tool(&tool, serde_json::json!({ "task_id": task_id })).await;
        assert!(!result.is_error);
        // The suppression flag is set (and would persist through a restart)
        assert!(
            tasks.get(&task_id).unwrap().notification_suppressed,
            "TaskStop must suppress the killed-notification"
        );
        // And the stopped task never enters the due set.
        use std::collections::HashSet;
        let due =
            crate::task_notification::due_notifications(&tasks, &HashSet::new(), &HashSet::new());
        assert!(
            due.is_empty(),
            "a TaskStop-stopped task must not notify: {due:?}"
        );
    }

    /// Given a running agent task with an attached cancel signal (the Agent
    /// tool's wiring), When TaskStop stops it gracefully, Then the signal
    /// fires and — the task never settling — the escalation lands Stopped
    /// after the grace window (G16 × D-63).
    #[tokio::test]
    async fn should_cancel_the_attached_signal_when_stopping_a_running_agent_task() {
        let tasks = Arc::new(TaskManager::new());
        let task_id = tasks.register(
            TaskKind::Agent {
                agent_id: Some("agent-running".to_string()),
                subagent_type: Some("coder".to_string()),
                parent_tool_call_id: None,
                swarm_index: None,
                model: None,
            },
            "agent work".to_string(),
            "session-1".to_string(),
        );
        let signal = crate::cancel::CancelToken::new();
        tasks.attach_signal(&task_id, signal.clone());

        let tool = TaskStopTool::new(Arc::clone(&tasks)).with_grace(Duration::from_millis(50));
        let result = execute_tool(&tool, serde_json::json!({ "task_id": task_id })).await;
        assert!(!result.is_error);
        let text = output_text(&result);
        assert!(text.contains("status: stopped"), "{text}");
        assert!(
            signal.is_cancelled(),
            "TaskStop must fire the agent task's cancel signal — a status flip alone leaves the subagent running"
        );
        assert_eq!(tasks.get(&task_id).unwrap().status, TaskStatus::Stopped);
    }

    /// Given a task whose attached signal lets it complete naturally within
    /// the grace window, When TaskStop stops it gracefully, Then the natural
    /// terminal status survives — the tool reports `completed`, not a
    /// premature killed flip (D-63: settle-first, escalate-later).
    #[tokio::test]
    async fn should_report_the_natural_completion_when_a_task_settles_inside_the_grace_window() {
        let tasks = Arc::new(TaskManager::new());
        let task_id = tasks.register(
            TaskKind::Agent {
                agent_id: Some("agent-quick".to_string()),
                subagent_type: Some("coder".to_string()),
                parent_tool_call_id: None,
                swarm_index: None,
                model: None,
            },
            "quick work".to_string(),
            "session-1".to_string(),
        );
        let signal = crate::cancel::CancelToken::new();
        tasks.attach_signal(&task_id, signal.clone());
        // Simulate the subagent observing cancellation and settling quickly.
        let mgr = Arc::clone(&tasks);
        let settle_id = task_id.clone();
        tokio::spawn(async move {
            signal.cancelled().await;
            tokio::time::sleep(Duration::from_millis(20)).await;
            mgr.complete(&settle_id, 0);
        });

        let tool = TaskStopTool::new(Arc::clone(&tasks)).with_grace(Duration::from_secs(2));
        let result = execute_tool(&tool, serde_json::json!({ "task_id": task_id })).await;
        assert!(!result.is_error);
        let text = output_text(&result);
        assert!(text.contains("status: completed (exit_code: 0)"), "{text}");
        assert!(
            !text.contains("status: stopped"),
            "a task that settles naturally must not be reported killed: {text}"
        );
        assert_eq!(
            tasks.get(&task_id).unwrap().status,
            TaskStatus::Completed { exit_code: 0 }
        );
    }
}
