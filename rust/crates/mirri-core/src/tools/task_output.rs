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
use crate::task::{MAX_TASK_OUTPUT_BYTES, TaskManager, TaskOutputCoordinates, TaskStatus};
use crate::tool::{ExecutionPlan, Tool, ToolExecution, ToolInputDisplay, ToolResult};
use crate::tools::tool_execution_error;

/// B-ADR-18 coordinate declarations, emitted in the paged header right
/// after the `window:` line so the model knows which byte space the
/// offsets address. One finalized wording per serving path.
/// The persisted-log path: offsets address the full on-disk log.
const LOG_ABSOLUTE_DECLARATION: &str =
    "coordinates: log-absolute (offsets address the full on-disk output log)";
/// The retained-buffer fallback: offsets address the in-memory tail
/// window, so after a head drop offset 0 is the retained head, not the
/// log's first byte.
const BUFFER_RELATIVE_DECLARATION: &str = "coordinates: buffer-relative (offsets address the retained in-memory buffer, which keeps only the tail window of the produced output)";

/// Format a [`TaskStatus`] as a lowercase label for plain-text output.
fn status_label(status: &TaskStatus) -> String {
    match status {
        TaskStatus::Running => "running".to_string(),
        TaskStatus::Completed { exit_code } => format!("completed (exit_code: {exit_code})"),
        TaskStatus::Failed(detail) => format!("failed: {detail}"),
        TaskStatus::Stopped => "stopped".to_string(),
    }
}

/// Compact whole-second duration label (`1h2m3s`, `4m5s`, `6s`).
pub(crate) fn format_duration(duration: std::time::Duration) -> String {
    let secs = duration.as_secs();
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}h{m}m{s}s")
    } else if m > 0 {
        format!("{m}m{s}s")
    } else {
        format!("{s}s")
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
        "Read the accumulated output of a background task by id — including \
         while the task is still running (output lands incrementally, so a \
         poll mid-run returns the latest tail). Use TaskList \
         to discover task ids. The inline view is capped at 1 MiB; larger \
         outputs are served as the tail plus the full log path. Pass \
         offset_bytes / max_bytes to page an arbitrary window of the log."
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
                    },
                    "offset_bytes": {
                        "type": "integer",
                        "minimum": 0,
                        "default": 0,
                        "description": "Byte offset into the full output log to start a paged read at. Giving offset_bytes or max_bytes switches to paged mode; omit both for the tail snapshot."
                    },
                    "max_bytes": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 256000,
                        "default": 50000,
                        "description": "Maximum bytes to return in paged mode (1-256000, default 50000)."
                    }
                },
                "required": ["task_id"],
                "additionalProperties": false
            })
        })
    }

    /// ADR-015 (stage-2 D-M2): the read-only capability category —
    /// observation only, no side effects the ReadOnly posture must gate.
    fn readonly(&self) -> bool {
        true
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

        let offset_arg = input.get("offset_bytes");
        let max_arg = input.get("max_bytes");
        let paged = offset_arg.is_some() || max_arg.is_some();
        let offset_bytes = match offset_arg {
            None => 0,
            Some(v) => match v.as_u64() {
                Some(n) => n,
                None => {
                    return Ok(tool_execution_error(
                        "TaskOutput 'offset_bytes' must be an integer >= 0",
                    ));
                }
            },
        };
        let max_bytes = match max_arg {
            None => 50_000usize,
            Some(v) => match v.as_u64() {
                Some(n) if (1..=256_000).contains(&n) => n as usize,
                _ => {
                    return Ok(tool_execution_error(
                        "TaskOutput 'max_bytes' must be an integer between 1 and 256000",
                    ));
                }
            },
        };

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
                    let snapshot = if paged {
                        tasks.output_window(&task_id, offset_bytes, max_bytes)
                    } else {
                        tasks.output_snapshot(&task_id, MAX_TASK_OUTPUT_BYTES)
                    };
                    let Some(snapshot) = snapshot else {
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
                    if let Some(task) = &task {
                        let running_for = (task.ended_at.unwrap_or_else(chrono::Utc::now)
                            - task.created_at)
                            .to_std()
                            .unwrap_or_default();
                        let silent_for = task
                            .last_output_at
                            .map(|at| at.elapsed())
                            .unwrap_or(running_for);
                        lines.push(format!("running_for: {}", format_duration(running_for)));
                        if !task.status.is_terminal() {
                            lines.push(format!("silent_for: {}", format_duration(silent_for)));
                        }
                    }
                    lines.push(format!("output_bytes: {}", snapshot.output_size_bytes));
                    if let Some((start, end)) = snapshot.window {
                        lines.push(format!(
                            "window: {start}..{end} of {}",
                            snapshot.output_size_bytes
                        ));
                        // B-ADR-18: which coordinate system the window and
                        // the caller's offset_bytes address — the two
                        // serving paths must not be confusable.
                        lines.push(match snapshot.coordinates {
                            TaskOutputCoordinates::LogAbsolute => {
                                LOG_ABSOLUTE_DECLARATION.to_string()
                            }
                            TaskOutputCoordinates::BufferRelative => {
                                BUFFER_RELATIVE_DECLARATION.to_string()
                            }
                        });
                    }
                    lines.push(format!("output_truncated: {truncated}"));
                    if still_running {
                        lines.push("message: task still running; partial output shown".to_string());
                    }
                    lines.push(String::new()); // blank line

                    // The truncation banner and the "tail" wording belong
                    // to the default tail-snapshot read, where the preview
                    // really is the log's tail with its head dropped. A
                    // paged read (window == Some) serves the requested
                    // window — which may sit in the middle of the log — so
                    // describing it as the tail (or as "truncated") would
                    // contradict the window: line right above.
                    let paged_read = snapshot.window.is_some();
                    if truncated && !paged_read {
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
                        if truncated && paged_read {
                            lines.push(format!(
                                    "Only the requested window (see the window: line) is shown above. Use the Read tool with this output_path to page through the full log (parameters: path, line_offset, n_lines; read about 300 lines per page): {}",
                                    path.display()
                                ));
                        } else if truncated {
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
                    let past_end = paged
                        && snapshot.output_size_bytes > 0
                        && offset_bytes >= snapshot.output_size_bytes
                        && snapshot.preview.is_empty();
                    let preview = if past_end {
                        format!(
                            "offset_bytes {offset_bytes} is past the end of the log ({} bytes)",
                            snapshot.output_size_bytes
                        )
                    } else if snapshot.preview.is_empty() {
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
            approval_metadata: None,
            wait_registry: None,
            turn_id: "test-turn".to_string(),
            tool_call_id: "test-call".to_string(),
            cancel: crate::cancel::CancelToken::default(),
            progress: crate::tool::ToolProgressHandle::default(),
            turn_origin: Default::default(),
        };
        futures::executor::block_on(execute(ctx))
    }

    #[test]
    fn should_advertise_mid_run_reads_in_the_description() {
        let tasks = std::sync::Arc::new(crate::task::TaskManager::new());
        let tool = TaskOutputTool::new(tasks);
        let description = tool.description();

        // The model must learn from the description alone that reads are
        // valid while the task is still running (batch 2's BH4 landing).
        assert!(
            description.contains("including while the task is still running"),
            "the description must name the mid-run read affordance: {description}"
        );
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
            approval_metadata: None,
            wait_registry: None,
            turn_id: "test-turn".to_string(),
            tool_call_id: "test-call".to_string(),
            cancel: crate::cancel::CancelToken::default(),
            progress: crate::tool::ToolProgressHandle::default(),
            turn_origin: Default::default(),
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

    #[tokio::test(start_paused = true)]
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
            tokio::time::sleep(std::time::Duration::from_millis(200)).await; // hygiene-allow: D: paused clock 虚拟定时器
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

    #[tokio::test(start_paused = true)]
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
            tokio::time::sleep(std::time::Duration::from_millis(100)).await; // hygiene-allow: D: paused clock 虚拟定时器
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

    #[tokio::test(start_paused = true)]
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
            tokio::time::sleep(std::time::Duration::from_millis(150)).await; // hygiene-allow: D: paused clock 虚拟定时器
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
            approval_metadata: None,
            wait_registry: None,
            turn_id: "test-turn".to_string(),
            tool_call_id: "test-call".to_string(),
            cancel,
            progress: crate::tool::ToolProgressHandle::default(),
            turn_origin: Default::default(),
        };

        // When the wait runs (30s timeout — the cancel at 150ms must win)
        let start = tokio::time::Instant::now();
        let result = execute(ctx).await;

        // Then the tool returns promptly with the cancellation error —
        // no zombie window between the cancel and the wait's natural end
        // (US5.3, measured on the same virtual clock as the cancel timer).
        // The task itself keeps running.
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

    fn persisted_manager() -> (tempfile::TempDir, Arc<TaskManager>) {
        let tmp = tempfile::tempdir().expect("tasks tempdir");
        let persistence = Arc::new(crate::task_persist::TaskPersistence::new(
            tmp.path().join("tasks"),
            std::sync::Arc::new(mirri_os::LocalSyncFs::new()),
        ));
        (tmp, Arc::new(TaskManager::with_persistence(persistence)))
    }

    fn register_process(tasks: &TaskManager) -> String {
        tasks.register(
            TaskKind::Process {
                command: None,
                dispatcher: None,
            },
            "build".to_string(),
            "session-1".to_string(),
        )
    }

    #[tokio::test]
    async fn should_report_running_and_silent_durations_for_a_running_task() {
        let tasks = Arc::new(TaskManager::new());
        let id = register_process(&tasks);
        tasks.append_output(&id, "x\n");
        let tool = TaskOutputTool::new(Arc::clone(&tasks));
        let text = output_text(&execute_tool(&tool, serde_json::json!({ "task_id": id })));
        assert!(text.contains("running_for: "), "{text}");
        assert!(text.contains("silent_for: "), "{text}");
        assert!(text.contains("output_bytes: 2"), "{text}");
    }

    #[tokio::test]
    async fn should_page_an_arbitrary_window_when_offset_and_max_bytes_are_given() {
        let (_tmp, tasks) = persisted_manager();
        let id = register_process(&tasks);
        tasks.append_output(&id, "0123456789ABCDEF");
        let tool = TaskOutputTool::new(Arc::clone(&tasks));
        let text = output_text(&execute_tool(
            &tool,
            serde_json::json!({ "task_id": id, "offset_bytes": 4, "max_bytes": 6 }),
        ));
        assert!(text.ends_with("[output]\n456789"), "{text}");
    }

    #[tokio::test]
    async fn should_report_total_bytes_and_window_range_in_the_paged_header() {
        let (_tmp, tasks) = persisted_manager();
        let id = register_process(&tasks);
        tasks.append_output(&id, "0123456789");
        let tool = TaskOutputTool::new(Arc::clone(&tasks));
        let text = output_text(&execute_tool(
            &tool,
            serde_json::json!({ "task_id": id, "offset_bytes": 2, "max_bytes": 3 }),
        ));
        assert!(text.contains("output_bytes: 10"), "{text}");
        assert!(text.contains("window: 2..5 of 10"), "{text}");
    }

    #[tokio::test]
    async fn should_tell_the_caller_the_offset_is_past_the_end_when_it_exceeds_the_log() {
        let (_tmp, tasks) = persisted_manager();
        let id = register_process(&tasks);
        tasks.append_output(&id, "short");
        let tool = TaskOutputTool::new(Arc::clone(&tasks));
        let text = output_text(&execute_tool(
            &tool,
            serde_json::json!({ "task_id": id, "offset_bytes": 99 }),
        ));
        assert!(
            text.contains("offset_bytes 99 is past the end of the log (5 bytes)"),
            "{text}"
        );
    }

    #[tokio::test]
    async fn should_start_the_window_on_a_character_boundary_when_the_offset_lands_mid_character() {
        let (_tmp, tasks) = persisted_manager();
        let id = register_process(&tasks);
        // "a" + "é"(2 bytes: offsets 1..3) + "b"
        tasks.append_output(&id, "aéb");
        let tool = TaskOutputTool::new(Arc::clone(&tasks));
        let text = output_text(&execute_tool(
            &tool,
            serde_json::json!({ "task_id": id, "offset_bytes": 2, "max_bytes": 10 }),
        ));
        assert!(text.ends_with("[output]\néb"), "{text}");
        assert!(text.contains("window: 1..4 of 4"), "{text}");
    }

    #[tokio::test]
    async fn should_serve_the_same_window_from_the_in_memory_buffer_when_no_log_exists() {
        let (_tmp, persisted) = persisted_manager();
        let memory = Arc::new(TaskManager::new());
        let pid = register_process(&persisted);
        let mid = register_process(&memory);
        for tasks_id in [(&persisted, &pid), (&memory, &mid)] {
            tasks_id.0.append_output(tasks_id.1, "aéb0123456789");
        }
        let input =
            |id: &str| serde_json::json!({ "task_id": id, "offset_bytes": 2, "max_bytes": 5 });
        let p = output_text(&execute_tool(&TaskOutputTool::new(persisted), input(&pid)));
        let m = output_text(&execute_tool(&TaskOutputTool::new(memory), input(&mid)));
        let window = |t: &str| {
            t.lines()
                .find(|l| l.starts_with("window:"))
                .expect("window line")
                .to_string()
        };
        // B-ADR-18: the served window interval is the same on both paths,
        // but the coordinate systems differ — each header declares its own.
        assert_eq!(window(&p), window(&m), "{p}\n{m}");
        let coord = |t: &str| {
            t.lines()
                .find(|l| l.starts_with("coordinates:"))
                .expect("coordinates line")
                .to_string()
        };
        assert_eq!(
            coord(&p),
            "coordinates: log-absolute (offsets address the full on-disk output log)",
            "{p}"
        );
        assert_eq!(
            coord(&m),
            "coordinates: buffer-relative (offsets address the retained in-memory buffer, which keeps only the tail window of the produced output)",
            "{m}"
        );
        assert_ne!(coord(&p), coord(&m), "{p}\n{m}");
        let body = |t: &str| t[t.find("[output]").unwrap()..].to_string();
        assert_eq!(body(&p), body(&m), "{p}\n{m}");
        assert!(body(&m).ends_with("éb01"), "{m}");
    }

    #[tokio::test]
    async fn should_declare_log_absolute_coordinates_when_a_log_exists() {
        let (_tmp, tasks) = persisted_manager();
        let id = register_process(&tasks);
        tasks.append_output(&id, "0123456789");
        let tool = TaskOutputTool::new(Arc::clone(&tasks));
        let text = output_text(&execute_tool(
            &tool,
            serde_json::json!({ "task_id": id, "offset_bytes": 2, "max_bytes": 3 }),
        ));
        assert!(
            text.contains(
                "coordinates: log-absolute (offsets address the full on-disk output log)"
            ),
            "{text}"
        );
        assert!(!text.contains("buffer-relative"), "{text}");
    }

    #[tokio::test]
    async fn should_declare_buffer_relative_coordinates_when_only_the_memory_buffer_exists() {
        let tasks = Arc::new(TaskManager::new());
        let id = register_process(&tasks);
        tasks.append_output(&id, "0123456789");
        let tool = TaskOutputTool::new(Arc::clone(&tasks));
        let text = output_text(&execute_tool(
            &tool,
            serde_json::json!({ "task_id": id, "offset_bytes": 2, "max_bytes": 3 }),
        ));
        assert!(
            text.contains("coordinates: buffer-relative (offsets address the retained in-memory buffer, which keeps only the tail window of the produced output)"),
            "{text}"
        );
        assert!(!text.contains("log-absolute"), "{text}");
    }

    /// D4 blind spot: once the retained buffer exceeded the 1 MiB cap and
    /// dropped its head, a paged read must still declare buffer-relative
    /// coordinates — and the window interval must be self-consistent with
    /// the retained buffer, not the total produced.
    #[tokio::test]
    async fn should_declare_buffer_relative_coordinates_after_the_buffer_dropped_its_head() {
        let tasks = Arc::new(TaskManager::new());
        let id = register_process(&tasks);
        // 1.2 MB total: the second append pushes past the 1 MiB cap, so the
        // retained buffer keeps only 448_576 head bytes + the 600_000 tail.
        tasks.append_output(&id, &"H".repeat(600_000));
        tasks.append_output(&id, &"T".repeat(600_000));
        let tool = TaskOutputTool::new(Arc::clone(&tasks));
        let text = output_text(&execute_tool(
            &tool,
            serde_json::json!({ "task_id": id, "offset_bytes": 0, "max_bytes": 8 }),
        ));
        assert!(
            text.contains("coordinates: buffer-relative (offsets address the retained in-memory buffer, which keeps only the tail window of the produced output)"),
            "{text}"
        );
        assert!(!text.contains("coordinates: log-absolute"), "{text}");
        // The interval is anchored to the retained 1 MiB window, not the
        // 1_200_000 produced bytes.
        assert!(text.contains("output_bytes: 1048576"), "{text}");
        assert!(text.contains("window: 0..8 of 1048576"), "{text}");
        assert!(text.contains("output_truncated: true"), "{text}");
        // Buffer-relative offset 0 addresses the retained window's head —
        // the surviving 'H' prefix — not the log's byte 0.
        assert!(text.ends_with("[output]\nHHHHHHHH"), "{text}");
    }

    #[tokio::test]
    async fn should_tell_the_caller_the_offset_is_past_the_end_when_it_equals_the_log_size() {
        let (_tmp, tasks) = persisted_manager();
        let id = register_process(&tasks);
        tasks.append_output(&id, "short");
        let tool = TaskOutputTool::new(Arc::clone(&tasks));
        let text = output_text(&execute_tool(
            &tool,
            serde_json::json!({ "task_id": id, "offset_bytes": 5 }),
        ));
        assert!(
            text.contains("offset_bytes 5 is past the end of the log (5 bytes)"),
            "{text}"
        );
    }

    /// Given a task with a persisted log, when TaskOutput pages a middle
    /// window of it (the window is neither the head nor the tail of the
    /// log), then the hint must describe the served window — not claim
    /// "only the tail" — and must not print the head-drop truncation
    /// banner that belongs to the default tail snapshot.
    #[tokio::test]
    async fn should_describe_the_paged_window_instead_of_claiming_a_tail_when_paging_a_middle_window() {
        let (_tmp, tasks) = persisted_manager();
        let id = register_process(&tasks);
        tasks.append_output(&id, &"0123456789".repeat(100));
        let tool = TaskOutputTool::new(Arc::clone(&tasks));
        let text = output_text(&execute_tool(
            &tool,
            serde_json::json!({ "task_id": id, "offset_bytes": 100, "max_bytes": 50 }),
        ));
        assert!(text.contains("window: 100..150 of 1000"), "{text}");
        assert!(
            !text.contains("Only the tail is shown above"),
            "a middle window must not be described as the tail: {text}"
        );
        assert!(
            !text.contains("[Truncated. Full output:"),
            "the head-drop banner belongs to the tail snapshot, not a paged window: {text}"
        );
        assert!(
            text.contains("requested window"),
            "the hint must describe the requested window: {text}"
        );
        assert!(text.contains("Read tool"), "{text}");
    }

    /// Given a task whose persisted log exceeds the inline preview window,
    /// when TaskOutput takes the default tail snapshot, then the hint
    /// keeps the tail wording and the head-drop truncation banner — the
    /// preview really is the tail there.
    #[tokio::test]
    async fn should_keep_the_tail_wording_when_the_default_snapshot_dropped_the_head() {
        let (_tmp, tasks) = persisted_manager();
        let id = register_process(&tasks);
        tasks.append_output(&id, &"x".repeat(1_200_000));
        tasks.append_output(&id, "TAIL_MARKER\n");
        tasks.complete(&id, 0);

        let tool = TaskOutputTool::new(Arc::clone(&tasks));
        let text = output_text(&execute_tool(&tool, serde_json::json!({ "task_id": id })));

        assert!(text.contains("output_truncated: true"), "{text}");
        assert!(text.contains("Only the tail is shown above"), "{text}");
        assert!(text.contains("[Truncated. Full output:"), "{text}");
        assert!(text.contains("TAIL_MARKER\n"), "{text}");
    }

    #[tokio::test]
    async fn should_not_report_silence_when_the_task_is_terminal() {
        let tasks = Arc::new(TaskManager::new());
        let id = register_process(&tasks);
        tasks.complete(&id, 0);
        let tool = TaskOutputTool::new(Arc::clone(&tasks));
        let text = output_text(&execute_tool(&tool, serde_json::json!({ "task_id": id })));
        assert!(text.contains("running_for: "), "{text}");
        assert!(!text.contains("silent_for"), "{text}");
    }
}
