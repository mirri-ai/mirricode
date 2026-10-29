//! Concurrent execution scheduler.
//!
//! The scheduler manages concurrent execution of tool calls within a single
//! step, based on resource-access conflicts (see [`ToolAccess`]).
//!
//! Rules (matching TypeScript `tool-scheduler.ts`):
//! - Non-conflicting tasks start immediately and run concurrently
//! - Conflicting tasks queue; when an active task finishes, queued tasks are
//!   re-evaluated for eligibility
//! - Results are handed back in provider order (input order), not execution
//!   order
//!
//! A task with [`AccessKind::All`] acts as a global mutex — it serializes all
//! tasks in the batch.

use std::collections::VecDeque;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;

use futures::FutureExt;
use futures::stream::{FuturesUnordered, StreamExt};

use crate::cancel::CancelToken;
use crate::tool::{AccessKind, ExecutableToolContext, ToolAccess, ToolFuture, ToolResult};

/// A boxed future that yields `(original_index, ToolResult)`.
type IndexedFuture = Pin<Box<dyn Future<Output = (usize, ToolResult)> + Send>>;

/// A pending tool execution task awaiting scheduling.
///
/// Created by the execution pipeline after a tool call passes preflight and
/// prepare. The scheduler decides when to start each task based on
/// [`ToolAccess`] conflict detection.
pub struct PendingToolTask {
    /// The tool call ID, used to map results back to the originating call.
    pub tool_call_id: String,
    /// The tool name (for finalize hook context).
    pub tool_name: String,
    /// The (possibly hook-rewritten) arguments the call runs with (for
    /// finalize hook context).
    pub args: serde_json::Value,
    /// Resource accesses this task will perform (for conflict detection).
    pub accesses: Vec<ToolAccess>,
    /// The execution closure (phase 2 of the two-phase Tool trait).
    pub execute: Box<dyn FnOnce(ExecutableToolContext) -> ToolFuture + Send>,
    /// Execution context (turn ID, tool call ID, cancel token).
    pub ctx: ExecutableToolContext,
    /// Whether to stop scheduling later tool calls in this batch after this
    /// one completes. Used by tools that mutate global state in a way that
    /// invalidates the rest of the batch (e.g. a goal change).
    pub stop_batch_after_this: bool,
}

impl std::fmt::Debug for PendingToolTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingToolTask")
            .field("tool_call_id", &self.tool_call_id)
            .field("tool_name", &self.tool_name)
            .field("accesses", &self.accesses)
            .field("stop_batch_after_this", &self.stop_batch_after_this)
            .finish()
    }
}

/// Execute a batch of tool calls with conflict-aware scheduling.
///
/// Tasks are started concurrently unless their accesses conflict. Conflicting
/// tasks are serialized. Results are returned in the same order as the input
/// (provider order).
///
/// # Conflict resolution
///
/// - If any task has [`AccessKind::All`], the entire batch is executed serially
///   (in provider order).
/// - Otherwise, tasks are started greedily: a task starts if it doesn't
///   conflict with any currently-active task. When a task finishes, queued
///   tasks are re-evaluated.
///
/// # Panics
///
/// If a spawned task panics, the panic is caught and converted into a
/// `ToolResult::error` for that task's slot. Other tasks continue normally.
///
/// # Returns
///
/// A `Vec<(tool_call_id, tool_name, ToolResult)>` in provider order. The
/// `tool_name` is carried through from [`PendingToolTask`] so the finalize
/// hook has full context.
pub async fn schedule_and_execute(
    tasks: Vec<PendingToolTask>,
    _cancel: &CancelToken,
) -> Vec<(String, String, serde_json::Value, ToolResult)> {
    let n = tasks.len();
    if n == 0 {
        return Vec::new();
    }

    // Extract tool_call_ids, tool_names, and args before consuming tasks
    // (for result mapping and the finalize hook's input).
    let task_meta: Vec<(String, String, serde_json::Value)> = tasks
        .iter()
        .map(|t| (t.tool_call_id.clone(), t.tool_name.clone(), t.args.clone()))
        .collect();

    // Record which tasks requested `stop_batch_after_this`, keyed by index.
    // When such a task completes, no further tasks are started.
    let stop_after_all: Vec<bool> = tasks.iter().map(|t| t.stop_batch_after_this).collect();

    if n == 1 {
        let task = tasks.into_iter().next().expect("n == 1");
        let (call_id, name, args) = task_meta.into_iter().next().expect("n == 1");
        let result = execute_task(task).await;
        return vec![(call_id, name, args, result)];
    }

    // Check if any task has AccessKind::All (global mutex → serialize all).
    let all_conflict = tasks
        .iter()
        .any(|t| t.accesses.iter().any(|a| a.kind == AccessKind::All));

    if all_conflict {
        // Execute serially in provider order. Honor `stop_batch_after_this`:
        // when a task with the flag completes, skip remaining tasks.
        let mut results = Vec::with_capacity(n);
        let mut batch_stopped = false;
        for (i, task) in tasks.into_iter().enumerate() {
            if batch_stopped {
                // Task was not started due to batch stop — record a synthetic
                // error result so the caller knows it was skipped.
                results.push(ToolResult::error(
                    "Tool execution skipped: a prior tool in this batch set stop_batch_after_this",
                ));
                continue;
            }
            let stop_after = stop_after_all[i];
            let result = execute_task(task).await;
            results.push(result);
            if stop_after {
                batch_stopped = true;
            }
        }
        return results
            .into_iter()
            .zip(task_meta)
            .map(|(result, (id, name, args))| (id, name, args, result))
            .collect();
    }

    // General case: greedy concurrent scheduling with conflict detection.
    let mut results: Vec<Option<ToolResult>> = vec![None; n];

    let mut pending: VecDeque<(usize, PendingToolTask)> = tasks.into_iter().enumerate().collect();
    let mut active: FuturesUnordered<IndexedFuture> = FuturesUnordered::new();
    let mut active_accesses: Vec<(usize, Vec<ToolAccess>)> = Vec::new();
    // Whether a completed task requested stopping the rest of the batch.
    // Once set, no new tasks are started; already-running tasks complete.
    let mut batch_stopped = false;

    loop {
        // Start as many pending tasks as possible (greedy), unless the batch
        // was stopped by a prior task's `stop_batch_after_this`.
        if !batch_stopped {
            let mut still_pending = VecDeque::new();
            while let Some((idx, task)) = pending.pop_front() {
                let conflicts = active_accesses
                    .iter()
                    .any(|(_, a)| ToolAccess::conflicts(&task.accesses, a));

                if conflicts {
                    still_pending.push_back((idx, task));
                } else {
                    let accesses = task.accesses.clone();
                    let execute = task.execute;
                    let ctx = task.ctx;
                    let tool_call_id = task.tool_call_id;

                    let fut: IndexedFuture = Box::pin(async move {
                        let result = execute_with_panic_safety(execute, ctx, &tool_call_id).await;
                        (idx, result)
                    });
                    active.push(fut);
                    active_accesses.push((idx, accesses));
                }
            }
            pending = still_pending;
        }

        if active.is_empty() {
            // No active tasks and no tasks can start — we're done.
            break;
        }

        // Wait for the next task to complete.
        match active.next().await {
            Some((idx, result)) => {
                results[idx] = Some(result);
                active_accesses.retain(|(i, _)| *i != idx);

                // If this task requested stopping the batch, prevent any
                // remaining pending tasks from starting. Already-active tasks
                // are allowed to finish.
                if stop_after_all[idx] {
                    batch_stopped = true;
                }
            }
            None => {
                // FuturesUnordered is empty — break to avoid infinite loop.
                break;
            }
        }
    }

    // Assemble results in provider order.
    results
        .into_iter()
        .zip(task_meta)
        .map(|(opt_result, (id, name, args))| {
            let result = opt_result.unwrap_or_else(|| {
                ToolResult::error("Tool execution was not started (scheduler anomaly)")
            });
            (id, name, args, result)
        })
        .collect()
}

/// Execute a single task by calling its execute closure with the context.
///
/// The tool's `execute` future is wrapped with [`catch_unwind`] so that a
/// panic inside the tool is converted into a [`ToolResult::error`] rather than
/// propagating through the scheduler and aborting the entire turn. This
/// fulfills the panic-safety contract documented on [`schedule_and_execute`].
///
/// [`catch_unwind`]: futures::FutureExt::catch_unwind
async fn execute_task(task: PendingToolTask) -> ToolResult {
    let PendingToolTask {
        execute,
        ctx,
        tool_call_id,
        ..
    } = task;
    execute_with_panic_safety(execute, ctx, &tool_call_id).await
}

/// Wrap a tool's execute future with panic-safety.
///
/// If the tool future panics, [`catch_unwind`] captures the panic payload and
/// we convert it into a [`ToolResult::error`] so the caller (scheduler) is
/// unaffected. The panic message is extracted from the `Box<dyn Any + Send>`
/// payload if possible; otherwise a generic message is used.
///
/// [`catch_unwind`]: futures::FutureExt::catch_unwind
async fn execute_with_panic_safety(
    execute: Box<dyn FnOnce(ExecutableToolContext) -> ToolFuture + Send>,
    ctx: ExecutableToolContext,
    tool_call_id: &str,
) -> ToolResult {
    let future = execute(ctx);
    match AssertUnwindSafe(future).catch_unwind().await {
        Ok(result) => result,
        Err(panic_payload) => {
            let message = panic_payload
                .downcast_ref::<&'static str>()
                .copied()
                .or_else(|| panic_payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("(no panic message)");
            let message = match crate::panic_capture::take_last_panic_location() {
                Some(location) => format!("{message} (panic at {location})"),
                None => message.to_string(),
            };
            ToolResult::error(format!("Tool '{}' panicked: {}", tool_call_id, message))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::{ExecutionPlan, ToolExecution, ToolInputDisplay};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A helper to create a pending task with given accesses and a result string.
    fn make_task(
        tool_call_id: &str,
        accesses: Vec<ToolAccess>,
        result_text: &str,
    ) -> PendingToolTask {
        let result_text = result_text.to_string();
        let ctx = ExecutableToolContext {
            approval_metadata: None,
            wait_registry: None,
            turn_id: "test-turn".to_string(),
            tool_call_id: tool_call_id.to_string(),
            cancel: CancelToken::new(),
            progress: crate::tool::ToolProgressHandle::default(),
        };
        PendingToolTask {
            tool_call_id: tool_call_id.to_string(),
            tool_name: "TestTool".to_string(),
            args: serde_json::json!({}),
            accesses,
            execute: Box::new(move |_ctx| {
                Box::pin(async move { ToolResult::success(result_text) })
            }),
            ctx,
            stop_batch_after_this: false,
        }
    }

    /// A helper to create a task that sleeps briefly, to test concurrency.
    fn make_delayed_task(
        tool_call_id: &str,
        accesses: Vec<ToolAccess>,
        delay_ms: u64,
        counter: Arc<AtomicUsize>,
        max_concurrent: Arc<AtomicUsize>,
    ) -> PendingToolTask {
        let ctx = ExecutableToolContext {
            approval_metadata: None,
            wait_registry: None,
            turn_id: "test-turn".to_string(),
            tool_call_id: tool_call_id.to_string(),
            cancel: CancelToken::new(),
            progress: crate::tool::ToolProgressHandle::default(),
        };
        let result_text = tool_call_id.to_string();
        PendingToolTask {
            tool_call_id: tool_call_id.to_string(),
            tool_name: "DelayedTool".to_string(),
            args: serde_json::json!({}),
            accesses,
            execute: Box::new(move |_ctx| {
                Box::pin(async move {
                    let current = counter.fetch_add(1, Ordering::SeqCst) + 1;
                    max_concurrent.fetch_max(current, Ordering::SeqCst);

                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;

                    counter.fetch_sub(1, Ordering::SeqCst);
                    ToolResult::success(result_text)
                })
            }),
            ctx,
            stop_batch_after_this: false,
        }
    }

    fn read_access(path: &str) -> ToolAccess {
        ToolAccess {
            kind: AccessKind::File,
            path: Some(path.to_string()),
            operation: crate::tool::AccessOperation::Read,
        }
    }

    fn write_access(path: &str) -> ToolAccess {
        ToolAccess {
            kind: AccessKind::File,
            path: Some(path.to_string()),
            operation: crate::tool::AccessOperation::Write,
        }
    }

    fn all_access() -> ToolAccess {
        ToolAccess {
            kind: AccessKind::All,
            path: None,
            operation: crate::tool::AccessOperation::ReadWrite,
        }
    }

    #[tokio::test]
    async fn should_return_no_results_when_batch_is_empty() {
        let cancel = CancelToken::new();
        let results = schedule_and_execute(Vec::new(), &cancel).await;
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn should_return_task_result_when_batch_has_single_task() {
        let cancel = CancelToken::new();
        let tasks = vec![make_task("call-1", vec![], "result-1")];
        let results = schedule_and_execute(tasks, &cancel).await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "call-1");
        match &results[0].3.output {
            crate::tool::ToolOutput::Text(s) => assert_eq!(s, "result-1"),
            _ => panic!("expected text output"),
        }
    }

    #[tokio::test]
    async fn should_execute_tasks_concurrently_when_accesses_do_not_conflict() {
        let cancel = CancelToken::new();
        let counter = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));

        let tasks = vec![
            make_delayed_task(
                "read-1",
                vec![read_access("/a/b")],
                50,
                Arc::clone(&counter),
                Arc::clone(&max_concurrent),
            ),
            make_delayed_task(
                "read-2",
                vec![read_access("/a/b")],
                50,
                Arc::clone(&counter),
                Arc::clone(&max_concurrent),
            ),
            make_delayed_task(
                "read-3",
                vec![read_access("/a/b")],
                50,
                Arc::clone(&counter),
                Arc::clone(&max_concurrent),
            ),
        ];

        let results = schedule_and_execute(tasks, &cancel).await;

        // Results in provider order
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].0, "read-1");
        assert_eq!(results[1].0, "read-2");
        assert_eq!(results[2].0, "read-3");

        // All three ran concurrently (max_concurrent >= 2)
        let max = max_concurrent.load(Ordering::SeqCst);
        assert!(
            max >= 2,
            "expected at least 2 concurrent tasks, got {}",
            max
        );
    }

    #[tokio::test]
    async fn should_serialize_tasks_when_writes_conflict_on_same_path() {
        let cancel = CancelToken::new();
        let counter = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));

        let tasks = vec![
            make_delayed_task(
                "write-1",
                vec![write_access("/same/path")],
                50,
                Arc::clone(&counter),
                Arc::clone(&max_concurrent),
            ),
            make_delayed_task(
                "write-2",
                vec![write_access("/same/path")],
                50,
                Arc::clone(&counter),
                Arc::clone(&max_concurrent),
            ),
        ];

        let results = schedule_and_execute(tasks, &cancel).await;

        // Results in provider order
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, "write-1");
        assert_eq!(results[1].0, "write-2");

        // Tasks were serialized (max_concurrent == 1)
        let max = max_concurrent.load(Ordering::SeqCst);
        assert_eq!(
            max, 1,
            "expected serialized execution (max_concurrent=1), got {}",
            max
        );
    }

    #[tokio::test]
    async fn should_run_concurrently_when_writes_target_disjoint_paths() {
        let cancel = CancelToken::new();
        let counter = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));

        let tasks = vec![
            make_delayed_task(
                "write-a",
                vec![write_access("/path/a")],
                50,
                Arc::clone(&counter),
                Arc::clone(&max_concurrent),
            ),
            make_delayed_task(
                "write-b",
                vec![write_access("/path/b")],
                50,
                Arc::clone(&counter),
                Arc::clone(&max_concurrent),
            ),
        ];

        let results = schedule_and_execute(tasks, &cancel).await;

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, "write-a");
        assert_eq!(results[1].0, "write-b");

        let max = max_concurrent.load(Ordering::SeqCst);
        assert!(
            max >= 2,
            "expected concurrent execution, got max_concurrent={}",
            max
        );
    }

    #[tokio::test]
    async fn should_serialize_everything_when_task_has_all_access() {
        let cancel = CancelToken::new();
        let counter = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));

        let tasks = vec![
            make_delayed_task(
                "all-1",
                vec![all_access()],
                50,
                Arc::clone(&counter),
                Arc::clone(&max_concurrent),
            ),
            make_delayed_task(
                "read-1",
                vec![read_access("/some/path")],
                50,
                Arc::clone(&counter),
                Arc::clone(&max_concurrent),
            ),
            make_delayed_task(
                "read-2",
                vec![read_access("/other/path")],
                50,
                Arc::clone(&counter),
                Arc::clone(&max_concurrent),
            ),
        ];

        let results = schedule_and_execute(tasks, &cancel).await;

        assert_eq!(results.len(), 3);
        // Results in provider order
        assert_eq!(results[0].0, "all-1");
        assert_eq!(results[1].0, "read-1");
        assert_eq!(results[2].0, "read-2");

        // Everything serialized
        let max = max_concurrent.load(Ordering::SeqCst);
        assert_eq!(
            max, 1,
            "expected full serialization, got max_concurrent={}",
            max
        );
    }

    #[tokio::test]
    async fn should_partially_serialize_when_some_tasks_conflict() {
        let cancel = CancelToken::new();
        let counter = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));

        // Task 0: write to /a — conflicts with task 1 (write to /a)
        // Task 1: write to /a — conflicts with task 0
        // Task 2: read from /b — no conflict with either
        let tasks = vec![
            make_delayed_task(
                "write-a-1",
                vec![write_access("/a")],
                50,
                Arc::clone(&counter),
                Arc::clone(&max_concurrent),
            ),
            make_delayed_task(
                "write-a-2",
                vec![write_access("/a")],
                50,
                Arc::clone(&counter),
                Arc::clone(&max_concurrent),
            ),
            make_delayed_task(
                "read-b",
                vec![read_access("/b")],
                50,
                Arc::clone(&counter),
                Arc::clone(&max_concurrent),
            ),
        ];

        let results = schedule_and_execute(tasks, &cancel).await;

        assert_eq!(results.len(), 3);
        assert_eq!(results[0].0, "write-a-1");
        assert_eq!(results[1].0, "write-a-2");
        assert_eq!(results[2].0, "read-b");

        // At least 2 concurrent at some point (task 0+2 or task 1+2)
        let max = max_concurrent.load(Ordering::SeqCst);
        assert!(
            max >= 2,
            "expected partial concurrency, got max_concurrent={}",
            max
        );
        // But not all 3 at once (two writes conflict)
        assert!(
            max < 3,
            "expected at most 2 concurrent, got max_concurrent={}",
            max
        );
    }

    /// A helper to create a task that panics during execution.
    fn make_panicking_task(tool_call_id: &str) -> PendingToolTask {
        let ctx = ExecutableToolContext {
            approval_metadata: None,
            wait_registry: None,
            turn_id: "test-turn".to_string(),
            tool_call_id: tool_call_id.to_string(),
            cancel: CancelToken::new(),
            progress: crate::tool::ToolProgressHandle::default(),
        };
        PendingToolTask {
            tool_call_id: tool_call_id.to_string(),
            tool_name: "PanickingTool".to_string(),
            args: serde_json::json!({}),
            accesses: vec![],
            execute: Box::new(move |_ctx| {
                Box::pin(async move {
                    panic!("intentional test panic from PanickingTool");
                })
            }),
            ctx,
            stop_batch_after_this: false,
        }
    }

    #[tokio::test]
    async fn should_return_error_result_when_single_task_panics() {
        let cancel = CancelToken::new();
        let tasks = vec![make_panicking_task("panic-1")];
        let results = schedule_and_execute(tasks, &cancel).await;

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0, "panic-1");
        assert!(
            results[0].3.is_error,
            "panicking task should produce an error result, not propagate the panic"
        );
        if let crate::tool::ToolOutput::Text(s) = &results[0].3.output {
            assert!(
                s.contains("panicked"),
                "error message should mention the panic, got: {}",
                s
            );
        } else {
            panic!("expected text output for panic error");
        }
    }

    #[tokio::test]
    async fn should_not_affect_other_tasks_when_one_task_panics() {
        let cancel = CancelToken::new();
        let tasks = vec![
            make_task("call-1", vec![], "result-1"),
            make_panicking_task("panic-1"),
            make_task("call-2", vec![], "result-2"),
        ];
        let results = schedule_and_execute(tasks, &cancel).await;

        assert_eq!(results.len(), 3);
        // All results present in provider order.
        assert_eq!(results[0].0, "call-1");
        assert_eq!(results[1].0, "panic-1");
        assert_eq!(results[2].0, "call-2");

        // Non-panicking tasks succeed normally.
        match &results[0].3.output {
            crate::tool::ToolOutput::Text(s) => assert_eq!(s, "result-1"),
            _ => panic!("expected text output for call-1"),
        }
        match &results[2].3.output {
            crate::tool::ToolOutput::Text(s) => assert_eq!(s, "result-2"),
            _ => panic!("expected text output for call-2"),
        }

        // Panicking task produces an error result.
        assert!(
            results[1].3.is_error,
            "panicking task should produce an error result"
        );
    }

    #[tokio::test]
    async fn should_catch_panic_when_task_runs_in_all_access_batch() {
        let cancel = CancelToken::new();
        let tasks = vec![
            make_panicking_task("panic-all"),
            make_task("call-1", vec![all_access()], "result-1"),
        ];
        let results = schedule_and_execute(tasks, &cancel).await;

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, "panic-all");
        assert_eq!(results[1].0, "call-1");
        assert!(results[0].3.is_error, "panicking task should error");
        assert!(
            !results[1].3.is_error,
            "non-panicking task should succeed despite sibling panic"
        );
    }

    // Suppress unused import warnings for test-only types.
    #[allow(dead_code)]
    fn _suppress_unused() {
        let _ = ExecutionPlan {
            accesses: vec![],
            display: ToolInputDisplay::Generic {
                summary: String::new(),
            },
            approval_rule: String::new(),
            stop_batch_after_this: false,
        };
        let _ = ToolExecution::Error(crate::tool::ToolErrorResult {
            message: String::new(),
        });
    }
}
