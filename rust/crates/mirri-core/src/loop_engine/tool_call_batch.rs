//! Tool call batch helpers — bridges [`LlmToolCall`] to the execution pipeline.
//!
//! The loop step collects [`LlmToolCall`]s from the LLM response and either:
//! - Executes them via [`run_tool_batch`] (when stop reason is `ToolUse`), or
//! - Records them as unexecuted synthetic errors (when the model stopped early).
//!
//! This module also provides [`derive_step_stop_reason`] which maps the
//! normalized [`StopReason`] to the step-level [`StepStopReason`].

use crate::cancel::CancelToken;
use crate::context_memory::{ContextMemory, LoopEvent};
use crate::hooks::LoopHooks;
use crate::llm::{LlmToolCall, StopReason};
use crate::loop_engine::StepStopReason;
use crate::loop_engine::events::LiveEventCallback;
use crate::tool::{ToolOutput, ToolResult};
use crate::tool_registry::{DispatchedToolCall, ToolManager, ValidatorCache, run_tool_batch};

/// Derive the step-level stop reason from the normalized [`StopReason`].
///
/// The [`StopReason`] from [`LlmChatResponse`](crate::llm::LlmChatResponse) is
/// already normalized (the adapter or `chat_with_retry` resolved `completed`/
/// `tool_calls` based on whether tool calls were present). This function maps
/// the isomorphic enum variants 1:1.
pub fn derive_step_stop_reason(stop_reason: &StopReason, _has_tool_calls: bool) -> StepStopReason {
    match stop_reason {
        StopReason::EndTurn => StepStopReason::EndTurn,
        StopReason::MaxTokens => StepStopReason::MaxTokens,
        StopReason::ToolUse => StepStopReason::ToolUse,
        StopReason::Filtered => StepStopReason::Filtered,
        StopReason::Paused => StepStopReason::Paused,
        StopReason::Unknown => StepStopReason::Unknown,
    }
}

/// Execute a batch of tool calls from the LLM response.
///
/// Converts [`LlmToolCall`]s to `(tool_name, raw_args, tool_call_id)` triples
/// and delegates to [`run_tool_batch`], forwarding `dispatch`: the caller's
/// callback receives each settled tool call with its final (possibly
/// hook-rewritten) arguments, in provider order — the point where the
/// `LoopEvent::ToolCall` record and the live `ToolCallStarted` event belong
/// (TS parity: `dispatchToolCall`).
///
/// Returns `(tool_call_id, ToolResult)` pairs in provider order (the order
/// the tool calls appeared in the LLM response). This is important because
/// [`run_tool_batch`] internally groups results by category (blocked,
/// synthetic, ready) which may differ from the original provider order.
#[allow(clippy::too_many_arguments)]
pub async fn execute_tool_calls(
    tool_calls: &[LlmToolCall],
    tools: &ToolManager,
    validators: &mut ValidatorCache,
    turn_id: &str,
    step: u32,
    cancel: &CancelToken,
    live_callback: Option<&LiveEventCallback>,
    hooks: &dyn LoopHooks,
    dispatch: &mut (dyn FnMut(&DispatchedToolCall<'_>) + Send),
    wait_registry: Option<&std::sync::Arc<crate::turn_flow::WaitRegistry>>,
) -> Vec<(String, ToolResult)> {
    let triples: Vec<(String, String, String)> = tool_calls
        .iter()
        .map(|tc| (tc.name.clone(), tc.args.to_string(), tc.id.clone()))
        .collect();

    let mut results = run_tool_batch(
        &triples,
        tools,
        validators,
        turn_id,
        step,
        cancel,
        live_callback,
        hooks,
        Some(dispatch),
        wait_registry,
    )
    .await;

    // Re-sort results to match the original provider order (the order of
    // `tool_calls`). `run_tool_batch` returns results grouped by category
    // (blocked → synthetic → ready), which may differ from provider order.
    // We rebuild the list by matching each original tool_call_id against the
    // results, using swap_remove to avoid O(n²) and to consume results without
    // cloning.
    let mut sorted = Vec::with_capacity(tool_calls.len());
    for tc in tool_calls {
        if let Some(pos) = results.iter().position(|(id, _)| *id == tc.id) {
            sorted.push(results.swap_remove(pos));
        }
    }

    // If any results remain (e.g., orphaned IDs not in the original list),
    // append them at the end so no data is silently dropped.
    sorted.append(&mut results);

    sorted
}

/// Record tool calls that were not executed because the model stopped before
/// tool execution could begin (Paused, Unknown, MaxTokens with tool calls).
///
/// For each tool call, records a `LoopEvent::ToolCall` followed by a
/// `LoopEvent::ToolResult` with a synthetic error message. The recorded
/// events are returned so callers can persist them (e.g. to the session's
/// wire log) in the same order they were folded into memory.
pub fn record_unexecuted_tool_calls(
    tool_calls: &[LlmToolCall],
    step_id: &str,
    memory: &mut ContextMemory,
) -> Vec<LoopEvent> {
    let mut recorded = Vec::new();
    for tc in tool_calls {
        let tool_call = LoopEvent::ToolCall {
            step_id: step_id.to_string(),
            tool_call_id: tc.id.clone(),
            tool_name: tc.name.clone(),
            args: tc.args.clone(),
        };
        memory.append_loop_event(tool_call.clone());
        recorded.push(tool_call);

        let tool_result = LoopEvent::ToolResult {
            tool_call_id: tc.id.clone(),
            output: ToolOutput::text(
                "This tool call was not executed: the model response ended before tool execution could start.",
            ),
            is_error: true,
            note: None,
            entry_seq: None,
        };
        memory.append_loop_event(tool_result.clone());
        recorded.push(tool_result);
    }
    recorded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_derive_end_turn_when_stop_reason_is_end_turn() {
        assert_eq!(
            derive_step_stop_reason(&StopReason::EndTurn, false),
            StepStopReason::EndTurn
        );
    }

    #[test]
    fn should_derive_tool_use_when_stop_reason_is_tool_use() {
        assert_eq!(
            derive_step_stop_reason(&StopReason::ToolUse, true),
            StepStopReason::ToolUse
        );
    }

    #[test]
    fn should_derive_max_tokens_when_stop_reason_is_max_tokens() {
        assert_eq!(
            derive_step_stop_reason(&StopReason::MaxTokens, false),
            StepStopReason::MaxTokens
        );
    }

    #[test]
    fn should_derive_filtered_when_stop_reason_is_filtered() {
        assert_eq!(
            derive_step_stop_reason(&StopReason::Filtered, false),
            StepStopReason::Filtered
        );
    }

    #[test]
    fn should_derive_paused_when_stop_reason_is_paused() {
        assert_eq!(
            derive_step_stop_reason(&StopReason::Paused, false),
            StepStopReason::Paused
        );
    }

    #[test]
    fn should_derive_unknown_when_stop_reason_is_unknown() {
        assert_eq!(
            derive_step_stop_reason(&StopReason::Unknown, false),
            StepStopReason::Unknown
        );
    }

    /// Verify that `execute_tool_calls` returns results in provider order,
    /// not in the internal (blocked → synthetic → ready) order that
    /// `run_tool_batch` produces.
    ///
    /// Strategy: register a tool that returns its `tool_call_id` as output.
    /// Provide two tool calls with different IDs. The first call is to a
    /// non-existent tool (producing a synthetic error result) and the second
    /// is to the real tool. Without re-sorting, the blocked/synthetic result
    /// would appear first; with re-sorting, results match the original order.
    #[tokio::test]
    async fn should_return_results_in_provider_order_when_executing_tool_calls() {
        use crate::error::EngineResult;
        use crate::hooks::{
            AfterStepContext, AfterStepResult, BeforeStepContext, BeforeStepResult,
            ContinueAfterStopContext, ContinueAfterStopResult, FinalizeToolContext,
            FinalizeToolResult, HookFuture, LoopHooks, PrepareToolContext, PrepareToolResult,
        };
        use crate::tool::{ExecutionPlan, Tool, ToolExecution, ToolInputDisplay};
        use crate::tool_registry::ToolManager;
        use serde_json::{Value, json};
        use std::sync::Arc;

        // A minimal no-op hooks implementation.
        struct NoopHooks;
        impl LoopHooks for NoopHooks {
            fn before_step(
                &self,
                _ctx: BeforeStepContext,
            ) -> HookFuture<crate::error::EngineResult<BeforeStepResult>> {
                Box::pin(async { Ok(BeforeStepResult::default()) })
            }
            fn after_step(
                &self,
                _ctx: AfterStepContext,
            ) -> HookFuture<crate::error::EngineResult<AfterStepResult>> {
                Box::pin(async { Ok(AfterStepResult::default()) })
            }
            fn prepare_tool_execution(
                &self,
                _ctx: PrepareToolContext<'_>,
            ) -> HookFuture<crate::error::EngineResult<PrepareToolResult>> {
                Box::pin(async { Ok(PrepareToolResult::Allowed) })
            }
            fn finalize_tool_result(
                &self,
                _ctx: FinalizeToolContext<'_>,
            ) -> HookFuture<crate::error::EngineResult<FinalizeToolResult>> {
                Box::pin(async { Ok(FinalizeToolResult::default()) })
            }
            fn should_continue_after_stop(
                &self,
                _ctx: ContinueAfterStopContext,
            ) -> HookFuture<crate::error::EngineResult<ContinueAfterStopResult>> {
                Box::pin(async {
                    Ok(ContinueAfterStopResult {
                        continue_turn: false,
                    })
                })
            }
        }

        // A minimal tool that echoes its "message" argument.
        struct EchoTool;
        impl Tool for EchoTool {
            fn name(&self) -> &str {
                "Echo"
            }
            fn description(&self) -> &str {
                "Echoes back the input"
            }
            fn parameters(&self) -> &Value {
                static SCHEMA: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
                SCHEMA.get_or_init(|| {
                    json!({
                        "type": "object",
                        "properties": {
                            "message": { "type": "string" }
                        },
                        "required": ["message"]
                    })
                })
            }
            fn resolve_execution(&self, input: Value) -> EngineResult<ToolExecution> {
                let msg = input
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("no message")
                    .to_string();
                Ok(ToolExecution::Runnable {
                    plan: ExecutionPlan {
                        accesses: Vec::new(),
                        display: ToolInputDisplay::Generic {
                            summary: format!("Echo: {}", msg),
                        },
                        approval_rule: "Echo".to_string(),
                        stop_batch_after_this: false,
                    },
                    execute: Box::new(move |_ctx| {
                        Box::pin(async move { ToolResult::success(msg) })
                    }),
                })
            }
        }

        let mut tools = ToolManager::new();
        tools.register_builtin(Arc::new(EchoTool));
        let mut validators = crate::tool_registry::ValidatorCache::new();
        let cancel = crate::cancel::CancelToken::new();
        let hooks = NoopHooks;

        // Tool calls in provider order: call-2 (non-existent → synthetic error),
        // call-1 (real tool → success). Without re-sorting, the synthetic
        // result from call-2 would appear first and the ready result from
        // call-1 would appear second — which happens to be correct here.
        // To actually test the re-sort, we put the real tool FIRST so its
        // "ready" result lands in the ready bucket (appended last by
        // run_tool_batch), and the synthetic error second. Without re-sorting
        // the synthetic would come first; with re-sorting the real tool
        // result comes first.
        let tool_calls = vec![
            LlmToolCall {
                id: "call-1".to_string(),
                name: "Echo".to_string(),
                args: json!({"message": "hello"}),
            },
            LlmToolCall {
                id: "call-2".to_string(),
                name: "NonExistent".to_string(),
                args: json!({}),
            },
        ];

        let results = execute_tool_calls(
            &tool_calls,
            &tools,
            &mut validators,
            "test-turn",
            1,
            &cancel,
            None,
            &hooks,
            &mut |_| {},
            None,
        )
        .await;

        // Results should be in provider order: call-1 first, call-2 second.
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].0, "call-1");
        assert_eq!(results[1].0, "call-2");

        // call-1 (Echo) should succeed.
        assert!(!results[0].1.is_error, "call-1 should succeed, got error");
        // call-2 (non-existent tool) should be an error.
        assert!(results[1].1.is_error, "call-2 should be an error result");
    }
}
