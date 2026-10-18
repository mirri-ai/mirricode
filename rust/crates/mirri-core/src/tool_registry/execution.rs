//! Tool execution pipeline.
//!
//! The full pipeline: preflight → prepare → schedule → finalize.
//!
//! ## Preflight ([`preflight_tool_call`])
//! Synchronous: parse args, find tool, convert args (alias remap + numeric
//! coercion), validate against JSON Schema. Produces a [`ValidatedToolCall`]
//! or a [`RejectedToolCall`].
//!
//! ## Prepare ([`prepare_tool_call`])
//! Async: run `prepare_tool_execution` hook, `rewrite_tool_input` hook,
//! `resolve_execution` (tool builds its plan), then `authorize_tool_execution`
//! hook (permission gate). Produces a [`PreparedToolCall`], [`BlockedToolCall`],
//! or [`SyntheticToolCall`].
//!
//! ## Execute + Finalize ([`execute_tool_batch`])
//! Async: schedule ready calls with conflict-aware concurrency, execute them,
//! then run `finalize_tool_result` hook on each result. Blocked and synthetic
//! results pass through directly. Returns `(tool_call_id, ToolResult)` pairs.

use std::sync::Arc;

use serde_json::Value;

use crate::cancel::CancelToken;
use crate::error::{EngineError, EngineResult};
use crate::hooks::{
    AuthorizeToolContext, AuthorizeToolResult, FinalizeToolContext, LoopHooks, PrepareToolContext,
    PrepareToolResult,
};
use crate::tool::{
    ExecutableToolContext, ExecutionPlan, Tool, ToolFuture, ToolProgressHandle, ToolResult,
};
use crate::tool_registry::ToolManager;
use crate::tool_registry::arg_converter::convert_tool_args;
use crate::tool_registry::args_validator::ValidatorCache;
use crate::tool_registry::scheduler::{PendingToolTask, schedule_and_execute};

// ---------------------------------------------------------------------------
// Preflight types and function
// ---------------------------------------------------------------------------

/// Result of preflight validation.
pub enum PreflightResult {
    /// The tool call passed validation and is ready for preparation.
    Valid(ValidatedToolCall),
    /// The tool call was rejected (tool not found, invalid args).
    Rejected(RejectedToolCall),
}

/// A validated tool call ready for the prepare phase.
pub struct ValidatedToolCall {
    /// The resolved tool implementation.
    pub tool: Arc<dyn Tool>,
    /// Parsed, converted, and validated arguments.
    pub args: Value,
    /// The tool call ID from the LLM.
    pub tool_call_id: String,
}

/// A tool call that was rejected during preflight.
pub struct RejectedToolCall {
    /// The requested tool name (may not exist in the registry).
    pub tool_name: String,
    /// The tool call ID from the LLM.
    pub tool_call_id: String,
    /// The rejection reason (not found, invalid args, etc.).
    pub error: String,
}

/// Preflight validation: parse args, find tool, convert args, validate args.
///
/// This is synchronous and does not perform any I/O. It:
/// 1. Parses `raw_args` as JSON (lenient: parse failure → empty object)
/// 2. Looks up the tool in the registry (builtin → user → MCP)
/// 3. Converts args (alias remapping, numeric coercion)
/// 4. Validates args against the tool's JSON Schema
pub fn preflight_tool_call(
    tool_name: &str,
    raw_args: &str,
    tool_call_id: &str,
    tools: &ToolManager,
    validators: &mut ValidatorCache,
) -> PreflightResult {
    // Parse args (lenient: parse failure → {})
    let mut args: Value =
        serde_json::from_str(raw_args).unwrap_or_else(|_| Value::Object(Default::default()));

    // Find tool
    let tool = match tools.find_tool(tool_name) {
        Some(t) => t,
        None => {
            return PreflightResult::Rejected(RejectedToolCall {
                tool_name: tool_name.to_string(),
                tool_call_id: tool_call_id.to_string(),
                error: format!("Tool \"{}\" not found", tool_name),
            });
        }
    };

    // Convert args (alias remapping, numeric coercion)
    convert_tool_args(tool_name, tool.parameters(), &mut args);

    // Validate args
    match validators.get_or_create(tool_name, tool.parameters()) {
        Ok(validator) => {
            if let Err(err_msg) = validator.validate(&args) {
                return PreflightResult::Rejected(RejectedToolCall {
                    tool_name: tool_name.to_string(),
                    tool_call_id: tool_call_id.to_string(),
                    error: format!("Invalid args for tool {}: {}", tool_name, err_msg),
                });
            }
        }
        Err(e) => {
            // Schema compilation failed — skip validation but log warning
            tracing::warn!("Failed to compile schema for tool {}: {}", tool_name, e);
        }
    }

    PreflightResult::Valid(ValidatedToolCall {
        tool,
        args,
        tool_call_id: tool_call_id.to_string(),
    })
}

// ---------------------------------------------------------------------------
// Prepare types and function
// ---------------------------------------------------------------------------

/// Result of preparing a tool call (after hooks + resolve_execution + authorize).
pub enum PrepareResult {
    /// The tool call is ready to execute.
    Ready(PreparedToolCall),
    /// The tool call was blocked by a hook.
    Blocked(BlockedToolCall),
    /// The tool call was replaced with a synthetic result by a hook.
    Synthetic(SyntheticToolCall),
}

/// A tool call that has been prepared and is ready to execute.
pub struct PreparedToolCall {
    /// The tool call ID from the LLM.
    pub tool_call_id: String,
    /// The tool name (for finalize hook context).
    pub tool_name: String,
    /// The (possibly hook-rewritten) arguments (for finalize hook context).
    pub args: serde_json::Value,
    /// The execution plan (accesses, display, approval rule).
    pub plan: ExecutionPlan,
    /// The execution closure (phase 2 of the two-phase Tool trait).
    pub execute: Box<dyn FnOnce(ExecutableToolContext) -> ToolFuture + Send>,
}

/// A tool call that was blocked during preparation.
pub struct BlockedToolCall {
    /// The tool call ID from the LLM.
    pub tool_call_id: String,
    /// The tool name (for finalize hook context).
    pub tool_name: String,
    /// The call's arguments (for finalize hook context).
    pub args: serde_json::Value,
    /// The block reason from the hook.
    pub reason: String,
}

/// A tool call that was replaced with a synthetic result.
pub struct SyntheticToolCall {
    /// The tool call ID from the LLM.
    pub tool_call_id: String,
    /// The tool name (for finalize hook context).
    pub tool_name: String,
    /// The call's arguments (for finalize hook context).
    pub args: serde_json::Value,
    /// The synthetic result.
    pub result: ToolResult,
}

/// Prepare a tool call: run hooks, resolve execution, authorize.
///
/// This is async and calls multiple hooks:
/// 1. `prepare_tool_execution` — dedup/block/synthetic
/// 2. `rewrite_tool_input` — modify args
/// 3. `resolve_execution` — tool builds its execution plan
/// 4. Cancellation check
/// 5. `authorize_tool_execution` — permission gate (block/synthetic)
pub async fn prepare_tool_call(
    valid: ValidatedToolCall,
    turn_id: &str,
    step: u32,
    cancel: &CancelToken,
    hooks: &dyn LoopHooks,
) -> EngineResult<PrepareResult> {
    let ValidatedToolCall {
        tool,
        args,
        tool_call_id,
    } = valid;
    let tool_name = tool.name().to_string();

    // 1. prepare_tool_execution hook
    let prepare_ctx = PrepareToolContext {
        turn_id: turn_id.to_string(),
        step,
        tool_call_id: tool_call_id.clone(),
        tool_name: tool_name.clone(),
        args: &args,
        cancel: cancel.clone(),
    };
    let mut effective_args = match hooks.prepare_tool_execution(prepare_ctx).await? {
        PrepareToolResult::Blocked { reason } => {
            return Ok(PrepareResult::Blocked(BlockedToolCall {
                tool_call_id,
                tool_name,
                args,
                reason,
            }));
        }
        PrepareToolResult::Synthetic { result } => {
            return Ok(PrepareResult::Synthetic(SyntheticToolCall {
                tool_call_id,
                tool_name,
                args,
                result,
            }));
        }
        PrepareToolResult::Allowed => args,
        // A hook rewrote the input (a PreToolUse hook's `updatedInput`) —
        // the rewritten args flow into the rewrite and execution phases.
        PrepareToolResult::Rewritten { args } => args,
    };

    // 2. rewrite_tool_input hook
    hooks.rewrite_tool_input(&tool_name, &mut effective_args)?;

    // 3. resolve_execution (tool builds its execution plan)
    let execution = tool
        .resolve_execution(effective_args.clone())
        .map_err(|e| {
            EngineError::ToolExecutionFailed(format!(
                "Tool {} failed to resolve execution: {}",
                tool_name, e
            ))
        })?;

    let (plan, execute) = match execution {
        crate::tool::ToolExecution::Runnable { plan, execute } => (plan, execute),
        crate::tool::ToolExecution::Error(err) => {
            return Ok(PrepareResult::Synthetic(SyntheticToolCall {
                tool_call_id,
                tool_name,
                args: effective_args,
                result: ToolResult::error(err.message),
            }));
        }
    };

    // 4. Check cancellation
    if cancel.is_cancelled() {
        return Ok(PrepareResult::Synthetic(SyntheticToolCall {
            tool_call_id,
            tool_name,
            args: effective_args,
            result: ToolResult::error("Tool execution was cancelled"),
        }));
    }

    // 5. authorize_tool_execution hook (permission gate)
    let auth_ctx = AuthorizeToolContext {
        turn_id: turn_id.to_string(),
        step,
        tool_call_id: tool_call_id.clone(),
        tool_name: tool_name.clone(),
        args: &effective_args,
        approval_rule: &plan.approval_rule,
        display: &plan.display,
        cancel: cancel.clone(),
    };
    match hooks.authorize_tool_execution(auth_ctx).await? {
        AuthorizeToolResult::Blocked { reason } => {
            return Ok(PrepareResult::Blocked(BlockedToolCall {
                tool_call_id,
                tool_name,
                args: effective_args,
                reason,
            }));
        }
        AuthorizeToolResult::Synthetic { result } => {
            return Ok(PrepareResult::Synthetic(SyntheticToolCall {
                tool_call_id,
                tool_name,
                args: effective_args,
                result,
            }));
        }
        AuthorizeToolResult::Approved => {}
    }

    Ok(PrepareResult::Ready(PreparedToolCall {
        tool_call_id,
        tool_name,
        args: effective_args,
        plan,
        execute,
    }))
}

// ---------------------------------------------------------------------------
// Execute + Finalize function
// ---------------------------------------------------------------------------

/// Execute a batch of tool calls: schedule, execute, finalize.
///
/// Takes the three output lists from the prepare phase:
/// - `ready` — calls that passed all hooks and are ready to execute
/// - `blocked` — calls blocked by a hook (produce error results)
/// - `synthetic` — calls replaced with a synthetic result by a hook
///
/// Ready calls are scheduled with conflict-aware concurrency via
/// [`schedule_and_execute`]. All results (blocked, synthetic, executed) are
/// then passed through the `finalize_tool_result` hook.
///
/// Returns `(tool_call_id, ToolResult)` pairs. The caller is responsible for
/// re-ordering by matching `tool_call_id` against the original provider order.
#[allow(clippy::too_many_arguments)]
pub async fn execute_tool_batch(
    ready: Vec<PreparedToolCall>,
    blocked: Vec<BlockedToolCall>,
    synthetic: Vec<SyntheticToolCall>,
    turn_id: &str,
    step: u32,
    cancel: &CancelToken,
    live_callback: Option<&crate::loop_engine::LiveEventCallback>,
    hooks: &dyn LoopHooks,
    wait_registry: Option<&std::sync::Arc<crate::turn_flow::WaitRegistry>>,
) -> Vec<(String, ToolResult)> {
    let mut results: Vec<(String, ToolResult)> = Vec::new();

    // Collect blocked results (error results)
    for b in blocked {
        results.push((
            b.tool_call_id.clone(),
            finalize_result(
                b.tool_call_id,
                b.tool_name,
                b.args,
                ToolResult::error(b.reason),
                turn_id,
                step,
                cancel,
                hooks,
            )
            .await,
        ));
    }

    // Collect synthetic results
    for s in synthetic {
        results.push((
            s.tool_call_id.clone(),
            finalize_result(
                s.tool_call_id,
                s.tool_name,
                s.args,
                s.result,
                turn_id,
                step,
                cancel,
                hooks,
            )
            .await,
        ));
    }

    // Schedule and execute ready calls
    let tasks: Vec<PendingToolTask> = ready
        .into_iter()
        .map(|p| {
            let tool_call_id = p.tool_call_id.clone();
            let tool_name = p.tool_name.clone();
            // Wire the tool's live-update handle: each update sent through
            // `ctx.progress` becomes a `LiveEvent::ToolProgress` on the turn's
            // live callback. No callback → default no-op handle.
            let progress = live_callback.map(|cb| {
                let cb = std::sync::Arc::clone(cb);
                let sink_id = p.tool_call_id.clone();
                ToolProgressHandle::with_sink(std::sync::Arc::new(move |update| {
                    cb(crate::loop_engine::LiveEvent::ToolProgress {
                        tool_call_id: sink_id.clone(),
                        update,
                    });
                }))
            });
            let ctx = ExecutableToolContext {
                turn_id: turn_id.to_string(),
                tool_call_id: p.tool_call_id.clone(),
                cancel: cancel.clone(),
                progress: progress.unwrap_or_default(),
                wait_registry: wait_registry.cloned(),
            };
            PendingToolTask {
                tool_call_id,
                tool_name,
                args: p.args,
                accesses: p.plan.accesses.clone(),
                execute: p.execute,
                ctx,
                stop_batch_after_this: p.plan.stop_batch_after_this,
            }
        })
        .collect();

    let exec_results = schedule_and_execute(tasks, cancel).await;

    // Finalize each executed result with the finalize_tool_result hook.
    // schedule_and_execute returns (tool_call_id, tool_name, args, ToolResult)
    // quads, so we have full context for the finalize hook.
    for (tool_call_id, tool_name, args, result) in exec_results {
        results.push((
            tool_call_id.clone(),
            finalize_result(
                tool_call_id,
                tool_name,
                args,
                result,
                turn_id,
                step,
                cancel,
                hooks,
            )
            .await,
        ));
    }

    results
}

/// Run the `finalize_tool_result` hook on a single result.
///
/// If the hook returns an error, the result is replaced with an error result.
/// If the hook returns `stop_turn: true`, the result's `stop_turn` flag is set.
#[allow(clippy::too_many_arguments)]
async fn finalize_result(
    tool_call_id: String,
    tool_name: String,
    args: serde_json::Value,
    mut result: ToolResult,
    turn_id: &str,
    step: u32,
    cancel: &CancelToken,
    hooks: &dyn LoopHooks,
) -> ToolResult {
    let finalize_ctx = FinalizeToolContext {
        turn_id: turn_id.to_string(),
        step,
        tool_call_id: tool_call_id.clone(),
        tool_name,
        args: &args,
        result: &mut result,
        cancel: cancel.clone(),
    };

    match hooks.finalize_tool_result(finalize_ctx).await {
        Ok(finalize_result) => {
            if finalize_result.stop_turn {
                result.stop_turn = true;
            }
            result
        }
        Err(e) => {
            tracing::warn!("Finalize hook failed for {}: {}", tool_call_id, e);
            ToolResult::error("Tool result finalization failed")
        }
    }
}

/// One tool call whose final arguments are decided, passed to the
/// [`run_tool_batch`] dispatch callback (TS parity: `dispatchToolCall`).
///
/// Emitted in provider order, once per call, after preflight and the prepare
/// hooks (dedup, PreToolUse rewrite, authorization) have settled the call —
/// `args` is what the call actually runs with (the possibly hook-rewritten
/// input), so the recorded `tool.call` event and the live `ToolCallStarted`
/// notification reflect the executed reality rather than the model's original
/// output.
pub struct DispatchedToolCall<'a> {
    /// The tool call id from the LLM response.
    pub tool_call_id: &'a str,
    /// The resolved tool name.
    pub tool_name: &'a str,
    /// The final (possibly hook-rewritten) arguments.
    pub args: &'a serde_json::Value,
}

/// Invoke the optional dispatch callback with one settled tool call.
fn notify_dispatched(
    dispatch: &mut Option<&mut (dyn FnMut(&DispatchedToolCall<'_>) + Send)>,
    record: &DispatchedToolCall<'_>,
) {
    if let Some(cb) = dispatch.as_deref_mut() {
        cb(record);
    }
}

/// Run the full pipeline for a batch of tool calls.
///
/// This is a convenience function that:
/// 1. Runs preflight for each (tool_name, raw_args, tool_call_id) triple
/// 2. Runs prepare for each validated call
/// 3. Runs execute_tool_batch for all prepared/blocked/synthetic calls
///
/// `dispatch` receives one notification per tool call, in provider order,
/// as soon as the call's final arguments are decided — after preflight and
/// the prepare hooks (dedup, PreToolUse rewrite, authorization) have
/// adjudicated it (TS parity: `dispatchToolCall`, which publishes
/// `tool.call.started` and records the tool call into context memory with
/// the effective args — the possibly hook-rewritten arguments the call will
/// actually run with). The three outcome lists are notified in their
/// resolved form: ready calls carry their (possibly rewritten) args,
/// blocked/synthetic calls the args they settled on.
///
/// Returns `(tool_call_id, ToolResult)` pairs.
#[allow(clippy::too_many_arguments)]
pub async fn run_tool_batch(
    tool_calls: &[(String, String, String)],
    tools: &ToolManager,
    validators: &mut ValidatorCache,
    turn_id: &str,
    step: u32,
    cancel: &CancelToken,
    live_callback: Option<&crate::loop_engine::LiveEventCallback>,
    hooks: &dyn LoopHooks,
    mut dispatch: Option<&mut (dyn FnMut(&DispatchedToolCall<'_>) + Send)>,
    wait_registry: Option<&std::sync::Arc<crate::turn_flow::WaitRegistry>>,
) -> Vec<(String, ToolResult)> {
    // Preflight all tool calls
    let mut ready: Vec<PreparedToolCall> = Vec::new();
    let mut blocked: Vec<BlockedToolCall> = Vec::new();
    let mut synthetic: Vec<SyntheticToolCall> = Vec::new();

    for (tool_name, raw_args, tool_call_id) in tool_calls {
        match preflight_tool_call(tool_name, raw_args, tool_call_id, tools, validators) {
            PreflightResult::Rejected(r) => {
                // Rejected calls produce error results directly. The
                // best-effort args feed the finalize hook's input.
                let s = SyntheticToolCall {
                    tool_call_id: r.tool_call_id,
                    tool_name: r.tool_name,
                    args: serde_json::from_str(raw_args).unwrap_or_else(|_| serde_json::json!({})),
                    result: ToolResult::error(r.error),
                };
                notify_dispatched(
                    &mut dispatch,
                    &DispatchedToolCall {
                        tool_call_id: &s.tool_call_id,
                        tool_name: &s.tool_name,
                        args: &s.args,
                    },
                );
                synthetic.push(s);
            }
            PreflightResult::Valid(v) => {
                // Keep a copy for the prepare-error path below — the
                // prepare hook consumes the validated call.
                let call_args = v.args.clone();
                match prepare_tool_call(v, turn_id, step, cancel, hooks).await {
                    Ok(PrepareResult::Ready(p)) => {
                        notify_dispatched(
                            &mut dispatch,
                            &DispatchedToolCall {
                                tool_call_id: &p.tool_call_id,
                                tool_name: &p.tool_name,
                                args: &p.args,
                            },
                        );
                        ready.push(p);
                    }
                    Ok(PrepareResult::Blocked(b)) => {
                        notify_dispatched(
                            &mut dispatch,
                            &DispatchedToolCall {
                                tool_call_id: &b.tool_call_id,
                                tool_name: &b.tool_name,
                                args: &b.args,
                            },
                        );
                        blocked.push(b);
                    }
                    Ok(PrepareResult::Synthetic(s)) => {
                        notify_dispatched(
                            &mut dispatch,
                            &DispatchedToolCall {
                                tool_call_id: &s.tool_call_id,
                                tool_name: &s.tool_name,
                                args: &s.args,
                            },
                        );
                        synthetic.push(s);
                    }
                    Err(e) => {
                        // Hook error → synthetic error result
                        let s = SyntheticToolCall {
                            tool_call_id: tool_call_id.clone(),
                            tool_name: tool_name.clone(),
                            args: call_args,
                            result: ToolResult::error(e.to_string()),
                        };
                        notify_dispatched(
                            &mut dispatch,
                            &DispatchedToolCall {
                                tool_call_id: &s.tool_call_id,
                                tool_name: &s.tool_name,
                                args: &s.args,
                            },
                        );
                        synthetic.push(s);
                    }
                }
            }
        }
    }

    // Execute + finalize
    execute_tool_batch(
        ready,
        blocked,
        synthetic,
        turn_id,
        step,
        cancel,
        live_callback,
        hooks,
        wait_registry,
    )
    .await
}
