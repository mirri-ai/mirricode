use std::future::Future;
use std::pin::Pin;

use crate::cancel::CancelToken;
use crate::error::EngineResult;
use crate::llm::{LlmChatParams, LlmChatResponse};
use crate::tool::{ToolInputDisplay, ToolResult};

/// A boxed async future for hook returns.
pub type HookFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Result of the `before_step` hook.
#[derive(Debug, Clone, Default)]
pub struct BeforeStepResult {
    /// If `true`, the step is blocked (the loop throws).
    pub block: bool,
    pub block_reason: Option<String>,
}

/// Result of the `after_step` hook.
#[derive(Debug, Clone, Default)]
pub struct AfterStepResult {
    /// If `true`, stop the turn after this step.
    pub stop_turn: bool,
}

/// Result of the `prepare_tool_execution` hook.
#[derive(Debug)]
pub enum PrepareToolResult {
    /// Tool call is allowed to proceed.
    Allowed,
    /// Tool call is blocked.
    Blocked { reason: String },
    /// Return a synthetic result instead of executing.
    Synthetic { result: ToolResult },
    /// Tool call is allowed to proceed with rewritten arguments (a hook —
    /// e.g. a PreToolUse hook's `updatedInput` — replaced the input).
    Rewritten { args: serde_json::Value },
}

/// Result of the `authorize_tool_execution` hook (permission gate).
#[derive(Debug)]
pub enum AuthorizeToolResult {
    /// Tool call is authorized.
    Approved,
    /// Tool call is denied.
    Blocked { reason: String },
    /// Return a synthetic result instead of executing.
    Synthetic { result: ToolResult },
}

/// Result of the `finalize_tool_result` hook.
#[derive(Debug, Clone, Default)]
pub struct FinalizeToolResult {
    /// If `true`, stop the turn after this tool result.
    pub stop_turn: bool,
}

/// Result of `should_continue_after_stop` hook.
#[derive(Debug, Clone, Default)]
pub struct ContinueAfterStopResult {
    pub continue_turn: bool,
}

/// Context for the `before_step` hook.
#[derive(Debug, Clone)]
pub struct BeforeStepContext {
    pub turn_id: String,
    pub step: u32,
    pub cancel: CancelToken,
}

/// Context for the `after_step` hook.
#[derive(Debug, Clone)]
pub struct AfterStepContext {
    pub turn_id: String,
    pub step: u32,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub stop_reason: String,
}

/// Context for the `prepare_tool_execution` hook.
pub struct PrepareToolContext<'a> {
    pub turn_id: String,
    pub step: u32,
    pub tool_call_id: String,
    pub tool_name: String,
    pub args: &'a serde_json::Value,
    pub cancel: CancelToken,
}

/// Context for the `authorize_tool_execution` hook.
pub struct AuthorizeToolContext<'a> {
    pub turn_id: String,
    pub step: u32,
    pub tool_call_id: String,
    pub tool_name: String,
    pub args: &'a serde_json::Value,
    pub approval_rule: &'a str,
    pub display: &'a ToolInputDisplay,
    pub cancel: CancelToken,
}

/// Context for the `finalize_tool_result` hook.
pub struct FinalizeToolContext<'a> {
    pub turn_id: String,
    pub step: u32,
    pub tool_call_id: String,
    pub tool_name: String,
    /// The (possibly hook-rewritten) arguments the tool call ran with —
    /// feeds the PostToolUse / PostToolUseFailure external-hook payloads
    /// (TS parity: `toolInput`).
    pub args: &'a serde_json::Value,
    pub result: &'a mut ToolResult,
    pub cancel: CancelToken,
}

/// Context for the `should_continue_after_stop` hook.
#[derive(Debug, Clone)]
pub struct ContinueAfterStopContext {
    pub turn_id: String,
    pub stop_reason: String,
    pub cancel: CancelToken,
}

/// The `LoopHooks` trait — the host injection surface.
///
/// The stateless loop calls these hooks at well-defined points. The stateful
/// host (`TurnFlow`) implements this trait to inject compaction, steering,
/// goals, hooks, permission, etc.
///
/// All methods have default no-op implementations so the loop can run without
/// a host (for testing).
pub trait LoopHooks: Send + Sync {
    fn before_step(&self, _ctx: BeforeStepContext) -> HookFuture<EngineResult<BeforeStepResult>> {
        Box::pin(async { Ok(BeforeStepResult::default()) })
    }

    fn after_step(&self, _ctx: AfterStepContext) -> HookFuture<EngineResult<AfterStepResult>> {
        Box::pin(async { Ok(AfterStepResult::default()) })
    }

    fn pre_llm_request(&self, _params: &LlmChatParams) {
        // fire-and-forget observer
    }

    fn post_llm_request(&self, _response: &LlmChatResponse) {
        // fire-and-forget observer
    }

    fn prepare_tool_execution(
        &self,
        _ctx: PrepareToolContext<'_>,
    ) -> HookFuture<EngineResult<PrepareToolResult>> {
        Box::pin(async { Ok(PrepareToolResult::Allowed) })
    }

    fn rewrite_tool_input(
        &self,
        _tool_name: &str,
        _args: &mut serde_json::Value,
    ) -> EngineResult<bool> {
        // Returns `true` if args were modified.
        Ok(false)
    }

    fn authorize_tool_execution(
        &self,
        _ctx: AuthorizeToolContext<'_>,
    ) -> HookFuture<EngineResult<AuthorizeToolResult>> {
        Box::pin(async { Ok(AuthorizeToolResult::Approved) })
    }

    fn finalize_tool_result(
        &self,
        _ctx: FinalizeToolContext<'_>,
    ) -> HookFuture<EngineResult<FinalizeToolResult>> {
        Box::pin(async { Ok(FinalizeToolResult::default()) })
    }

    fn should_continue_after_stop(
        &self,
        _ctx: ContinueAfterStopContext,
    ) -> HookFuture<EngineResult<ContinueAfterStopResult>> {
        Box::pin(async { Ok(ContinueAfterStopResult::default()) })
    }
}

/// A no-op implementation of [`LoopHooks`] for testing.
pub struct NoopHooks;

impl LoopHooks for NoopHooks {}
