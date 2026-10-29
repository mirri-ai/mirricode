//! Bridge from the external command [`HookEngine`] to the [`LoopHooks`] trait.
//!
//! [`ExternalHooksBridge`] adapts the process-based hook engine so that each
//! `LoopHooks` callback triggers the corresponding [`HookEventType`]. This lets
//! users configure arbitrary shell commands that intercept the agent loop at
//! well-defined points without writing Rust code.
//!
//! # Mapping
//!
//! | LoopHooks method             | HookEventType fired                            |
//! |------------------------------|------------------------------------------------|
//! | `before_step`                | `PreLlmRequest`                                |
//! | `after_step`                 | `PostLlmRequest`                               |
//! | `finalize_tool_result`       | `PostToolUse` / `PostToolUseFailure` (one of)  |
//!
//! `prepare_tool_execution`, `authorize_tool_execution`,
//! `should_continue_after_stop`, and `rewrite_tool_input` deliberately fire
//! nothing: the `PreToolUse`, `PermissionRequest`, and `Stop` external hooks
//! are triggered by the primary
//! [`TurnFlowHooks`](crate::turn_flow::TurnFlowHooks) instead, because their
//! outcomes need context-memory access (the additionalContext injection,
//! the forced-continuation reason append) and the permission-ask observation
//! point — none of which the stateless bridge has. Tool-input rewriting is
//! likewise NOT an external hook event here: the `PreToolUse` `updatedInput`
//! output is the single external rewrite channel (TS parity), and
//! [`LoopHooks::rewrite_tool_input`](crate::hooks::LoopHooks::rewrite_tool_input)
//! remains a host-injection seam only.

use std::collections::HashMap;
use std::sync::Arc;

use crate::error::EngineResult;
use crate::hook_engine::HookEngine;
use crate::hook_engine::types::HookEventType;
use crate::hooks::{
    AfterStepContext, AfterStepResult, AuthorizeToolContext, AuthorizeToolResult,
    BeforeStepContext, BeforeStepResult, ContinueAfterStopContext, ContinueAfterStopResult,
    FinalizeToolContext, FinalizeToolResult, HookFuture, LoopHooks, PrepareToolContext,
    PrepareToolResult,
};
use crate::tool::{ToolOutput, ToolResult};

/// Bridges an external [`HookEngine`] to the [`LoopHooks`] trait.
///
/// Each `LoopHooks` callback triggers the corresponding [`HookEventType`] on
/// the engine. Hook outcomes are translated:
///
/// - **Block** → the matching `LoopHooks` result signals a block / denial.
/// - **Pass** → execution proceeds normally.
pub struct ExternalHooksBridge {
    engine: Arc<HookEngine>,
    /// Base environment variables merged into every hook invocation (e.g.
    /// `cwd`, `session_id`).
    base_env: HashMap<String, String>,
}

impl ExternalHooksBridge {
    /// Create a new bridge wrapping the given engine.
    ///
    /// `base_env` is merged into every hook command's environment, on top of
    /// the process environment and the hook's own `env` map.
    pub fn new(engine: Arc<HookEngine>, base_env: HashMap<String, String>) -> Self {
        Self { engine, base_env }
    }

    /// Serialize a [`ToolResult`] into a JSON value for hook input.
    fn tool_result_to_json(result: &ToolResult) -> serde_json::Value {
        let output_text = Self::tool_output_text(result);
        serde_json::json!({
            "is_error": result.is_error,
            "output": output_text,
            "stop_turn": result.stop_turn,
            "truncated": result.truncated,
        })
    }

    /// The result's output as text (TS `toolOutputText`: text parts joined).
    fn tool_output_text(result: &ToolResult) -> String {
        match &result.output {
            ToolOutput::Text(s) => s.clone(),
            ToolOutput::Parts(parts) => parts
                .iter()
                .filter_map(|p| match p {
                    crate::message::ContentPart::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect(),
        }
    }
}

impl LoopHooks for ExternalHooksBridge {
    fn before_step(&self, ctx: BeforeStepContext) -> HookFuture<EngineResult<BeforeStepResult>> {
        let engine = Arc::clone(&self.engine);
        let env = self.base_env.clone();

        Box::pin(async move {
            let input = serde_json::json!({
                "step": ctx.step,
                "turn": ctx.turn_id,
                "context": {},
            });

            let result = engine
                .trigger(HookEventType::PreLlmRequest, input, &env)
                .await;

            if result.blocked {
                Ok(BeforeStepResult {
                    block: true,
                    block_reason: result.block_reason,
                })
            } else {
                Ok(BeforeStepResult::default())
            }
        })
    }

    fn after_step(&self, ctx: AfterStepContext) -> HookFuture<EngineResult<AfterStepResult>> {
        let engine = Arc::clone(&self.engine);
        let env = self.base_env.clone();

        Box::pin(async move {
            let input = serde_json::json!({
                "step": ctx.step,
                "turn": ctx.turn_id,
                "response": {
                    "input_tokens": ctx.input_tokens,
                    "output_tokens": ctx.output_tokens,
                    "cache_read_tokens": ctx.cache_read_tokens,
                    "cache_creation_tokens": ctx.cache_creation_tokens,
                    "stop_reason": ctx.stop_reason,
                },
            });

            engine
                .trigger(HookEventType::PostLlmRequest, input, &env)
                .await;

            Ok(AfterStepResult::default())
        })
    }

    fn prepare_tool_execution(
        &self,
        _ctx: PrepareToolContext<'_>,
    ) -> HookFuture<EngineResult<PrepareToolResult>> {
        // The `PreToolUse` external hook is fired by the primary
        // [`TurnFlowHooks`](crate::turn_flow::TurnFlowHooks): its outcomes
        // need context-memory access (`additionalContext` injection, TS
        // parity `runPreToolUse`) and the rewritten-args channel, neither of
        // which the stateless bridge has. The bridge has no opinion here —
        // `Allowed` preserves the primary's decision.
        Box::pin(async { Ok(PrepareToolResult::Allowed) })
    }

    fn rewrite_tool_input(
        &self,
        _tool_name: &str,
        _args: &mut serde_json::Value,
    ) -> EngineResult<bool> {
        // No external event: tool-input rewriting goes through the
        // `PreToolUse` hook's `updatedInput` output (fired by the primary
        // `TurnFlowHooks`), so the bridge keeps the trait's no-op default.
        // The `LoopHooks::rewrite_tool_input` method itself stays available
        // as a host-injection seam.
        Ok(false)
    }

    fn authorize_tool_execution(
        &self,
        _ctx: AuthorizeToolContext<'_>,
    ) -> HookFuture<EngineResult<AuthorizeToolResult>> {
        // The `PermissionRequest` external hook is fired by the primary
        // [`TurnFlowHooks`](crate::turn_flow::TurnFlowHooks) at the actual
        // permission ask — an observation of the request, never a gate (TS
        // parity: v2's `permission.approval.requested` subscriber). The
        // bridge has no opinion here — `Approved` preserves the primary's
        // decision under the composite's combination.
        Box::pin(async { Ok(AuthorizeToolResult::Approved { metadata: None }) })
    }

    fn finalize_tool_result(
        &self,
        ctx: FinalizeToolContext<'_>,
    ) -> HookFuture<EngineResult<FinalizeToolResult>> {
        let engine = Arc::clone(&self.engine);
        let env = self.base_env.clone();
        let tool_name = ctx.tool_name.clone();
        let tool_call_id = ctx.tool_call_id.clone();
        let tool_input = ctx.args.clone();
        let step = ctx.step;
        let turn_id = ctx.turn_id.clone();
        let is_error = ctx.result.is_error;
        let result_json = Self::tool_result_to_json(ctx.result);
        let output_text = Self::tool_output_text(ctx.result);

        Box::pin(async move {
            // TS parity (`notifyPostToolUse`): exactly one of
            // `PostToolUse` / `PostToolUseFailure` fires — an error result
            // fires only the failure event with an error payload, a success
            // fires only the success event with the output truncated to
            // 2000 characters.
            let event = if is_error {
                HookEventType::PostToolUseFailure
            } else {
                HookEventType::PostToolUse
            };
            let mut input = serde_json::json!({
                "tool_name": tool_name,
                "tool_call_id": tool_call_id,
                "tool_input": tool_input,
                "tool_result": result_json,
                "step": step,
                "turn": turn_id,
            });
            if is_error {
                // TS `toMirriErrorPayload` on a string tool output: an
                // internal-code error payload whose message is the text.
                input["error"] = serde_json::json!({
                    "code": "internal",
                    "message": output_text,
                });
            } else {
                input["tool_output"] =
                    serde_json::Value::String(output_text.chars().take(2000).collect());
            }

            engine.trigger(event, input, &env).await;

            Ok(FinalizeToolResult::default())
        })
    }

    fn should_continue_after_stop(
        &self,
        _ctx: ContinueAfterStopContext,
    ) -> HookFuture<EngineResult<ContinueAfterStopResult>> {
        // The `Stop` external hook is fired by the primary
        // [`TurnFlowHooks`](crate::turn_flow::TurnFlowHooks): a blocking
        // Stop hook forces a continuation that appends the block reason to
        // context memory (TS parity: `AgentExternalHooksService.runStop`),
        // and the stateless bridge has no memory access. The bridge has no
        // opinion of its own here — `continue_turn: true` preserves the
        // primary's decision under the composite's AND combination.
        Box::pin(async {
            Ok(ContinueAfterStopResult {
                continue_turn: true,
            })
        })
    }
}
