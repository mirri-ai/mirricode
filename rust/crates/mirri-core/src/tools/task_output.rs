//! `TaskOutputTool` — read a background task's accumulated output.
//!
//! Asks the session's [`TaskManager`] for an output snapshot: the
//! persisted on-disk log is authoritative when one exists (a resumed
//! session reads the full, never-truncated history this way), and the
//! retained in-memory buffer is the fallback. The inline preview is a
//! bounded window over whichever source serves it; when the head was
//! dropped the model is pointed at the log path with the `Read` tool to
//! page through the rest. Unknown task ids are reported as errors.

use std::sync::Arc;

use crate::error::EngineResult;
use crate::task::{MAX_TASK_OUTPUT_BYTES, TaskManager, TaskStatus};
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

/// `TaskOutput` — read the accumulated output of a background task.
pub struct TaskOutputTool {
    /// The session-scoped task manager this tool reads.
    tasks: Arc<TaskManager>,
}

impl TaskOutputTool {
    /// Create the tool bound to the session-scoped task manager.
    pub fn new(tasks: Arc<TaskManager>) -> Self {
        Self { tasks }
    }
}

impl Tool for TaskOutputTool {
    fn name(&self) -> &str {
        "TaskOutput"
    }

    fn description(&self) -> &str {
        "Read the accumulated output of a background task by id. Use TaskList \
         to discover task ids. The inline view is capped at 1 MiB; larger \
         outputs are served as the tail plus the full log path, which can be \
         paged through with the Read tool."
    }

    fn parameters(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "task_id": {
                        "type": "string",
                        "description": "ID of the background task to read."
                    },
                    "block": {
                        "type": "boolean",
                        "default": false,
                        "description": "When true, wait for the task to finish before returning."
                    },
                    "timeout": {
                        "type": "integer",
                        "default": 30,
                        "minimum": 0,
                        "maximum": 3600,
                        "description": "Maximum seconds to wait when block is true (0-3600, default 30)."
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
                "TaskOutput requires a string 'task_id' argument",
            ));
        };
        let task_id = task_id.to_string();
        let block = input
            .get("block")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let timeout = input
            .get("timeout")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(30)
            .min(3600);

        let tasks = Arc::clone(&self.tasks);
        let plan = ExecutionPlan {
            accesses: Vec::new(),
            display: ToolInputDisplay::BackgroundTask,
            approval_rule: "TaskOutput".to_string(),
            stop_batch_after_this: false,
        };

        Ok(ToolExecution::Runnable {
            plan,
            execute: Box::new(move |ctx| {
                let tasks = Arc::clone(&tasks);
                Box::pin(async move {
                    // Resolve the task id (with the bare-uuid fallback)
                    // before doing anything else.
                    let Some(task_id) = tasks.resolve_id(&task_id) else {
                        return ToolResult::error(format!(
                            "TaskOutput failed: task '{task_id}' not found"
                        ));
                    };

                    // When block is true, wait for the task to finish (or
                    // time out) before reading the output.  When block is
                    // false, fall through immediately — the caller gets the
                    // current snapshot just like before. The wait is
                    // cooperative (ADR-004 batch b): the turn's cancel token
                    // ends it immediately, and the waited-on task's
                    // heartbeat advances report liveness to the stall
                    // watchdog (batch a).
                    let still_running = if block {
                        // ADR-006: the blocking wait registers its
                        // WaitingTask entry — the watchdog pulls the
                        // task's owner verdict (heartbeat advance /
                        // terminal / missing row) at window expiry
                        // instead of relying on this tool's push feed.
                        let _task_wait = ctx.wait_registry.as_ref().map(|registry| {
                            registry.enter(
                                crate::turn_flow::WaitKind::WaitingTask,
                                crate::turn_flow::WaitSubject::task(
                                    task_id.clone(),
                                    Arc::clone(&tasks),
                                ),
                            )
                        });
                        match tasks
                            .wait_for_completion_with(&task_id, timeout, Some(&ctx.cancel))
                            .await
                        {
                            crate::task::WaitResult::TimedOut => true,
                            crate::task::WaitResult::Completed => false,
                            // Task vanished while waiting — treat as not found.
                            crate::task::WaitResult::NotFound => {
                                return ToolResult::error(format!(
                                    "TaskOutput failed: task '{task_id}' not found"
                                ));
                            }
                            // The turn was cancelled while the task kept
                            // running: the wait ended, the turn settles
                            // through its own cancellation path. The task's
                            // eventual settlement still arrives via its
                            // notification — the terminal-suppression check
                            // below only fires for settled tasks, so the
                            // notification stays armed.
                            crate::task::WaitResult::Cancelled => {
                                return ToolResult::error(
                                    "TaskOutput failed: the turn was cancelled while waiting"
                                        .to_string(),
                                );
                            }
                        }
                    } else {
                        false
                    };

                    // G25 (the rule TaskStop already follows): once this
                    // tool result carries the task's terminal status, it is
                    // the model's only answer for the settlement — a
                    // terminal notification on top of it would double-report.
                    // A blocking read that observed the completion, or a
                    // non-blocking read of an already-settled task, hands
                    // the model that status here. Only detached
                    // (background-registered) tasks carry notifications at
                    // all, so foreground reads never need the suppression.
                    if tasks
                        .get(&task_id)
                        .is_some_and(|task| task.detached && task.status.is_terminal())
                    {
                        tasks.suppress_terminal_notification(&task_id);
                    }

                    // Read the (possibly updated) snapshot — the persisted
                    // log is authoritative when one exists, the retained
                    // in-memory buffer otherwise. The task was verified to
                    // exist above, but it could vanish between that check
                    // and this read (e.g. a future cleanup/eviction) —
                    // fail gracefully instead of panicking.
                    let Some(snapshot) = tasks.output_snapshot(&task_id, MAX_TASK_OUTPUT_BYTES)
                    else {
                        return ToolResult::error(format!(
                            "TaskOutput failed: task '{task_id}' not found"
                        ));
                    };

                    // Fetch the current task to get its status for the
                    // metadata header.
                    let task = tasks.get(&task_id);
                    let status_str = task
                        .as_ref()
                        .map(|t| status_label(&t.status))
                        .unwrap_or_else(|| "unknown".to_string());

                    let truncated = snapshot.truncated;

                    // Build the plain-text output:
                    //   metadata lines
                    //   <blank>
                    //   [Truncated. ...] banner (when truncated)
                    //   full-log hint (when one exists)
                    //   [output]
                    //   <preview>
                    let mut lines: Vec<String> = Vec::new();
                    lines.push(format!("task_id: {task_id}"));
                    lines.push(format!("status: {status_str}"));
                    lines.push(format!("output_bytes: {}", snapshot.output_size_bytes));
                    lines.push(format!("output_truncated: {truncated}"));
                    if still_running {
                        lines.push("message: task still running; partial output shown".to_string());
                    }
                    lines.push(String::new()); // blank line

                    if truncated {
                        if let Some(path) = &snapshot.output_path {
                            lines.push(format!("[Truncated. Full output: {}]", path.display()));
                        } else {
                            lines.push(
                                "[Truncated. No persisted full log is available for this task.]"
                                    .to_string(),
                            );
                        }
                    }
                    if snapshot.full_output_available
                        && let Some(path) = &snapshot.output_path
                    {
                        if truncated {
                            lines.push(format!(
                                    "Only the tail is shown above. Use the Read tool with this output_path to page through the full log (parameters: path, line_offset, n_lines; read about 300 lines per page): {}",
                                    path.display()
                                ));
                        } else {
                            lines.push(format!(
                                    "The preview above is the complete output. Use the Read tool with this output_path to re-read the full log later (parameters: path, line_offset, n_lines): {}",
                                    path.display()
                                ));
                        }
                    }
                    lines.push("[output]".to_string());
                    let preview = if snapshot.preview.is_empty() {
                        "[no output available]".to_string()
                    } else {
                        snapshot.preview
                    };
                    lines.push(preview);

                    ToolResult::success(lines.join("\n"))
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

    fn execute_tool(tool: &TaskOutputTool, input: serde_json::Value) -> ToolResult {
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
        futures::executor::block_on(execute(ctx))
    }

    /// Async variant of [`execute_tool`] — needed for tests that exercise the
    /// `block` / `timeout` path because `wait_for_completion` uses
    /// `tokio::time::sleep`, which requires a tokio runtime context.
    async fn execute_tool_async(tool: &TaskOutputTool, input: serde_json::Value) -> ToolResult {
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
    async fn should_return_the_output_when_reading_a_task_with_output() {
        let tasks = Arc::new(TaskManager::new());
        let task_id = tasks.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "build".to_string(),
            "session-1".to_string(),
        );
        tasks.append_output(&task_id, "compiling...\ndone\n");
        let tool = TaskOutputTool::new(Arc::clone(&tasks));
        let result = execute_tool(&tool, serde_json::json!({ "task_id": task_id }));
        assert!(!result.is_error);
        let text = output_text(&result);
        assert!(text.contains(&format!("task_id: {task_id}")), "{text}");
        assert!(text.contains("[output]"), "{text}");
        assert!(text.contains("compiling...\ndone\n"), "{text}");
        assert!(text.contains("output_truncated: false"), "{text}");
    }

    #[tokio::test]
    async fn should_return_error_result_when_reading_an_unknown_task() {
        let tasks = Arc::new(TaskManager::new());
        let tool = TaskOutputTool::new(Arc::clone(&tasks));
        let result = execute_tool(&tool, serde_json::json!({ "task_id": "task-nope" }));
        assert!(result.is_error);
        assert!(output_text(&result).contains("not found"));
    }

    /// Given a running task whose id is `task-<uuid>`, when TaskOutput is
    /// called with the bare uuid (the `task-` prefix stripped — the shape
    /// models send when they misremember the id format), then the prefixed
    /// id is recovered and the output is served instead of a not-found error.
    #[tokio::test]
    async fn should_serve_the_output_when_reading_a_task_with_a_bare_uuid_id() {
        let tasks = Arc::new(TaskManager::new());
        let task_id = tasks.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "build".to_string(),
            "session-1".to_string(),
        );
        tasks.append_output(&task_id, "partial\n");
        let bare = task_id
            .strip_prefix("task-")
            .expect("registered ids carry the task- prefix")
            .to_string();

        let tool = TaskOutputTool::new(Arc::clone(&tasks));
        let result = execute_tool(&tool, serde_json::json!({ "task_id": bare }));
        assert!(!result.is_error, "{}", output_text(&result));
        let text = output_text(&result);
        assert!(text.contains(&format!("task_id: {task_id}")), "{text}");
        assert!(text.contains("partial\n"), "{text}");
    }

    #[tokio::test]
    async fn should_return_immediately_when_block_is_false() {
        let tasks = Arc::new(TaskManager::new());
        let task_id = tasks.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "build".to_string(),
            "session-1".to_string(),
        );
        tasks.append_output(&task_id, "partial\n");
        let tool = TaskOutputTool::new(Arc::clone(&tasks));

        // block defaults to false — the call must return immediately even
        // though the task is still running.
        let start = std::time::Instant::now();
        let result = execute_tool_async(&tool, serde_json::json!({ "task_id": task_id })).await;
        let elapsed = start.elapsed();

        assert!(!result.is_error);
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "block=false should return immediately, took {elapsed:?}"
        );
        let text = output_text(&result);
        assert!(text.contains("partial\n"), "{text}");
        // No "message" line when not blocking.
        assert!(!text.contains("message:"), "{text}");
    }

    #[tokio::test]
    async fn should_wait_for_completion_when_block_is_true() {
        let tasks = Arc::new(TaskManager::new());
        let task_id = tasks.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "build".to_string(),
            "session-1".to_string(),
        );
        tasks.append_output(&task_id, "compiling...\n");

        // Spawn a task that completes the background task after a short
        // delay so the blocking call has something to wait for.
        let tasks_clone = Arc::clone(&tasks);
        let id_clone = task_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            tasks_clone.append_output(&id_clone, "done\n");
            tasks_clone.complete(&id_clone, 0);
        });

        let tool = TaskOutputTool::new(Arc::clone(&tasks));
        let result = execute_tool_async(
            &tool,
            serde_json::json!({ "task_id": task_id, "block": true, "timeout": 10 }),
        )
        .await;

        assert!(!result.is_error);
        let text = output_text(&result);
        // The output should include everything appended before completion.
        assert!(text.contains("compiling...\ndone\n"), "{text}");
        // No "message" line when the task completed.
        assert!(!text.contains("message:"), "{text}");
        assert!(text.contains("[output]"), "{text}");
    }

    #[tokio::test]
    async fn should_suppress_terminal_notification_when_blocking_read_settles_the_task() {
        // Given a detached (background-registered) task that completes while
        // a blocking TaskOutput call waits for it
        let tasks = Arc::new(TaskManager::new());
        let task_id = tasks
            .register_background(
                TaskKind::Process {
                    command: None,
                    dispatcher: None,
                },
                "build".to_string(),
                "session-1".to_string(),
                None,
            )
            .expect("background registration");
        let tasks_clone = Arc::clone(&tasks);
        let id_clone = task_id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            tasks_clone.complete(&id_clone, 0);
        });

        let tool = TaskOutputTool::new(Arc::clone(&tasks));
        let result = execute_tool_async(
            &tool,
            serde_json::json!({ "task_id": task_id, "block": true, "timeout": 10 }),
        )
        .await;

        // When the tool result is the model's answer for the settled task
        // (the G25 rule TaskStop already follows), the terminal
        // notification must be suppressed so the driver does not
        // double-report the completion as a fresh turn.
        assert!(!result.is_error);
        assert!(
            tasks.get(&task_id).unwrap().notification_suppressed,
            "a blocking TaskOutput that observed the terminal status must suppress the notification"
        );
    }

    #[tokio::test]
    async fn should_suppress_terminal_notification_when_non_blocking_read_finds_task_terminal() {
        // Given a detached (background-registered) task that has ALREADY
        // settled (failed), a plain non-blocking TaskOutput read hands the
        // model the final status — the tool result is the model's answer,
        // so a terminal notification on top of it would double-report.
        let tasks = Arc::new(TaskManager::new());
        let task_id = tasks
            .register_background(
                TaskKind::Process {
                    command: None,
                    dispatcher: None,
                },
                "build".to_string(),
                "session-1".to_string(),
                None,
            )
            .expect("background registration");
        tasks.fail(&task_id, "exit status 1".to_string());

        let tool = TaskOutputTool::new(Arc::clone(&tasks));
        let result = execute_tool_async(&tool, serde_json::json!({ "task_id": task_id })).await;

        assert!(!result.is_error);
        assert!(
            tasks.get(&task_id).unwrap().notification_suppressed,
            "a non-blocking TaskOutput that observed a terminal status must suppress the notification"
        );
    }

    #[tokio::test]
    async fn should_not_suppress_terminal_notification_when_task_is_still_running() {
        // Given a detached (background-registered) task that is still
        // running, a non-blocking TaskOutput read shows partial output —
        // the terminal notification stays armed because the model has NOT
        // yet seen the task's settlement.
        let tasks = Arc::new(TaskManager::new());
        let task_id = tasks
            .register_background(
                TaskKind::Process {
                    command: None,
                    dispatcher: None,
                },
                "build".to_string(),
                "session-1".to_string(),
                None,
            )
            .expect("background registration");
        tasks.append_output(&task_id, "partial\n");

        let tool = TaskOutputTool::new(Arc::clone(&tasks));
        let result = execute_tool_async(&tool, serde_json::json!({ "task_id": task_id })).await;

        assert!(!result.is_error);
        assert!(
            !tasks.get(&task_id).unwrap().notification_suppressed,
            "a TaskOutput read of a still-running task must leave the notification armed"
        );
    }

    #[tokio::test]
    async fn should_not_suppress_terminal_notification_when_blocking_read_times_out() {
        // Given a detached (background-registered) task that stays running
        // past the blocking call's timeout, the tool result reports a
        // partial snapshot — the settlement has not been observed, so the
        // terminal notification must stay armed.
        let tasks = Arc::new(TaskManager::new());
        let task_id = tasks
            .register_background(
                TaskKind::Process {
                    command: None,
                    dispatcher: None,
                },
                "build".to_string(),
                "session-1".to_string(),
                None,
            )
            .expect("background registration");
        tasks.append_output(&task_id, "compiling...\n");

        let tool = TaskOutputTool::new(Arc::clone(&tasks));
        let result = execute_tool_async(
            &tool,
            serde_json::json!({ "task_id": task_id, "block": true, "timeout": 1 }),
        )
        .await;

        assert!(!result.is_error);
        let text = output_text(&result);
        assert!(text.contains("still running"), "{text}");
        assert!(
            !tasks.get(&task_id).unwrap().notification_suppressed,
            "a timed-out blocking read must leave the notification armed"
        );
    }

    #[tokio::test]
    async fn should_serve_the_persisted_full_log_when_reading_a_task_from_a_resumed_session() {
        // Given a task whose persisted log exceeds the 1 MiB inline window,
        // completed under a first manager over a persistence root
        let tmp = tempfile::tempdir().expect("tasks tempdir");
        let persistence = Arc::new(crate::task_persist::TaskPersistence::new(
            tmp.path().join("tasks"),
            std::sync::Arc::new(mirri_os::LocalSyncFs::new()),
        ));
        let first = Arc::new(TaskManager::with_persistence(Arc::clone(&persistence)));
        let task_id = first.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "build".to_string(),
            "session-1".to_string(),
        );
        first.append_output(&task_id, &"x".repeat(600_000));
        first.append_output(&task_id, &"x".repeat(600_000));
        first.append_output(&task_id, "TAIL_MARKER\n");
        first.complete(&task_id, 0);

        // When a fresh manager is built over the same root (the resume
        // case) and the task output is read through the tool
        let resumed = Arc::new(TaskManager::with_persistence(persistence));
        let tool = TaskOutputTool::new(Arc::clone(&resumed));
        let result = execute_tool(&tool, serde_json::json!({ "task_id": task_id }));

        // Then the persisted log is authoritative: honest truncation, the
        // log path surfaced with the truncation banner, the Read-tool
        // paging hint, the tail content, and the disk-recorded status
        assert!(!result.is_error);
        let text = output_text(&result);
        assert!(text.contains("status: completed"), "{text}");
        assert!(text.contains("output_truncated: true"), "{text}");
        assert!(text.contains("[Truncated. Full output:"), "{text}");
        assert!(
            text.contains(
                tmp.path()
                    .join("tasks")
                    .join(&task_id)
                    .join("output.log")
                    .to_str()
                    .unwrap()
            ),
            "{text}"
        );
        assert!(text.contains("TAIL_MARKER\n"), "{text}");
        assert!(text.contains("Read tool"), "{text}");
    }

    #[tokio::test]
    async fn should_point_at_the_read_tool_when_a_full_log_is_available() {
        // Given a task with a small persisted log (no truncation)
        let tmp = tempfile::tempdir().expect("tasks tempdir");
        let persistence = Arc::new(crate::task_persist::TaskPersistence::new(
            tmp.path().join("tasks"),
            std::sync::Arc::new(mirri_os::LocalSyncFs::new()),
        ));
        let tasks = Arc::new(TaskManager::with_persistence(Arc::clone(&persistence)));
        let task_id = tasks.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "build".to_string(),
            "session-1".to_string(),
        );
        tasks.append_output(&task_id, "compiling...\ndone\n");
        tasks.complete(&task_id, 0);

        let tool = TaskOutputTool::new(Arc::clone(&tasks));
        let result = execute_tool(&tool, serde_json::json!({ "task_id": task_id }));

        // Then the complete output is served inline and the model is
        // pointed at the Read tool for later re-reads — no truncation
        // banner for output that fits the window
        assert!(!result.is_error);
        let text = output_text(&result);
        assert!(text.contains("compiling...\ndone\n"), "{text}");
        assert!(text.contains("output_truncated: false"), "{text}");
        assert!(!text.contains("[Truncated."), "{text}");
        assert!(text.contains("Read tool"), "{text}");
    }

    #[tokio::test]
    async fn should_return_partial_output_when_block_timeout_expires() {
        let tasks = Arc::new(TaskManager::new());
        let task_id = tasks.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "build".to_string(),
            "session-1".to_string(),
        );
        tasks.append_output(&task_id, "partial output\n");

        let tool = TaskOutputTool::new(Arc::clone(&tasks));
        // Very short timeout — the task is never completed so we expect the
        // timeout to fire and partial output to be returned.
        let result = execute_tool_async(
            &tool,
            serde_json::json!({ "task_id": task_id, "block": true, "timeout": 1 }),
        )
        .await;

        assert!(!result.is_error);
        let text = output_text(&result);
        assert!(text.contains("partial output\n"), "{text}");
        assert!(text.contains("status: running"), "{text}");
        assert!(text.contains("still running"), "{text}");
        assert!(text.contains("[output]"), "{text}");
    }

    #[tokio::test]
    async fn should_return_error_immediately_when_the_turn_is_cancelled_while_blocking() {
        // Given a still-running task and a blocking TaskOutput whose turn
        // cancel token fires mid-wait
        let tasks = Arc::new(TaskManager::new());
        let task_id = tasks.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "build".to_string(),
            "session-1".to_string(),
        );
        tasks.append_output(&task_id, "compiling...\n");

        let cancel = crate::cancel::CancelToken::new();
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            cancel_clone.cancel();
        });

        let tool = TaskOutputTool::new(Arc::clone(&tasks));
        let execution = tool
            .resolve_execution(serde_json::json!({
                "task_id": task_id,
                "block": true,
                "timeout": 30
            }))
            .expect("resolve");
        let ToolExecution::Runnable { execute, .. } = execution else {
            panic!("expected runnable execution");
        };
        let ctx = crate::tool::ExecutableToolContext {
            wait_registry: None,
            turn_id: "test-turn".to_string(),
            tool_call_id: "test-call".to_string(),
            cancel,
            progress: crate::tool::ToolProgressHandle::default(),
        };

        // When the wait runs (30s timeout — the cancel at 150ms must win)
        let start = std::time::Instant::now();
        let result = execute(ctx).await;

        // Then the tool returns promptly with the cancellation error —
        // no zombie window between the cancel and the wait's natural end
        // (US5.3). The task itself keeps running.
        assert!(result.is_error, "{result:?}");
        assert!(
            output_text(&result).contains("turn was cancelled"),
            "{}",
            output_text(&result)
        );
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "cancel must end the blocked TaskOutput promptly, took {:?}",
            start.elapsed()
        );
        assert!(
            tasks
                .get(&task_id)
                .is_some_and(|t| t.status == TaskStatus::Running)
        );
    }
}
