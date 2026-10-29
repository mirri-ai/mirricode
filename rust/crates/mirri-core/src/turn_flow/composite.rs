//! `CompositeLoopHooks` — chains two [`LoopHooks`] implementations in sequence.
//!
//! `TurnFlow` uses this to run its internal [`TurnFlowHooks`] (compaction,
//! steering, injection, dedup, permissions) first and the user-configured
//! [`ExternalHooksBridge`](crate::hook_engine::ExternalHooksBridge) second, so
//! external command hooks fire at the same lifecycle points as the internal
//! host logic.
//!
//! # Merge semantics
//!
//! The composite is **conservative**: a callback that can gate an operation
//! combines both results so that either side blocking blocks the operation,
//! and `should_continue_after_stop` requires **both** sides to agree on
//! continuing (AND). Because an
//! [`ExternalHooksBridge`](crate::hook_engine::ExternalHooksBridge) with no
//! hooks registered returns non-blocking defaults (`block: false`,
//! `continue_turn: true`), the internal decision dominates whenever no
//! external hooks are registered for that event.
//!
//! # Implementation note
//!
//! [`HookFuture`] boxes are `'static`, so the composite cannot hold `&self`
//! across its chained awaits. Instead, each method invokes the stage methods
//! **synchronously** — the stage implementations return `'static` futures that
//! own all their data — and then chains those futures inside one fresh boxed
//! future.

use crate::error::EngineResult;
use crate::hooks::{
    AfterStepContext, AfterStepResult, AuthorizeToolContext, AuthorizeToolResult,
    BeforeStepContext, BeforeStepResult, ContinueAfterStopContext, ContinueAfterStopResult,
    FinalizeToolContext, FinalizeToolResult, HookFuture, LoopHooks, PrepareToolContext,
    PrepareToolResult,
};
use crate::llm::{LlmChatParams, LlmChatResponse};

/// Chains a primary and an optional secondary [`LoopHooks`] implementation.
///
/// Every callback runs the primary first. The secondary runs only when the
/// primary's result does not already settle the outcome — e.g. a
/// `prepare_tool_execution` that returns `Blocked` or `Synthetic`
/// short-circuits, so external hooks do not run for a tool call that was
/// already suppressed by the internal host logic.
pub struct CompositeLoopHooks<Primary, Secondary>
where
    Primary: LoopHooks + Send + Sync,
    Secondary: LoopHooks + Send + Sync,
{
    primary: Primary,
    secondary: Option<Secondary>,
}

impl<Primary, Secondary> CompositeLoopHooks<Primary, Secondary>
where
    Primary: LoopHooks + Send + Sync,
    Secondary: LoopHooks + Send + Sync,
{
    /// Create a composite running `primary` before `secondary`.
    ///
    /// Pass `None` for `secondary` to get plain primary behavior (the same
    /// code path as when no external hooks are wired).
    pub fn new(primary: Primary, secondary: Option<Secondary>) -> Self {
        Self { primary, secondary }
    }
}

impl<Primary, Secondary> LoopHooks for CompositeLoopHooks<Primary, Secondary>
where
    Primary: LoopHooks + Send + Sync,
    Secondary: LoopHooks + Send + Sync,
{
    fn before_step(&self, ctx: BeforeStepContext) -> HookFuture<EngineResult<BeforeStepResult>> {
        // Build both stage futures synchronously: they are `'static` (each
        // stage clones the context it needs), so the chaining future below
        // can own them.
        let primary_future = self.primary.before_step(ctx.clone());
        let secondary_future = self
            .secondary
            .as_ref()
            .map(|secondary| secondary.before_step(ctx));

        Box::pin(async move {
            let primary_result = primary_future.await?;
            if primary_result.block {
                return Ok(primary_result);
            }
            match secondary_future {
                Some(secondary_future) => {
                    let secondary_result = secondary_future.await?;
                    Ok(BeforeStepResult {
                        // Either side blocking blocks the step; the primary's
                        // reason wins when both provide one.
                        block: secondary_result.block,
                        block_reason: secondary_result
                            .block_reason
                            .or(primary_result.block_reason),
                    })
                }
                None => Ok(primary_result),
            }
        })
    }

    fn after_step(&self, ctx: AfterStepContext) -> HookFuture<EngineResult<AfterStepResult>> {
        let primary_future = self.primary.after_step(ctx.clone());
        let secondary_future = self
            .secondary
            .as_ref()
            .map(|secondary| secondary.after_step(ctx));

        Box::pin(async move {
            let primary_result = primary_future.await?;
            match secondary_future {
                Some(secondary_future) => {
                    let secondary_result = secondary_future.await?;
                    Ok(AfterStepResult {
                        // Either side requesting a stop stops the turn.
                        stop_turn: primary_result.stop_turn || secondary_result.stop_turn,
                    })
                }
                None => Ok(primary_result),
            }
        })
    }

    fn pre_llm_request(&self, params: &LlmChatParams) {
        self.primary.pre_llm_request(params);
        if let Some(secondary) = &self.secondary {
            secondary.pre_llm_request(params);
        }
    }

    fn post_llm_request(&self, response: &LlmChatResponse) {
        self.primary.post_llm_request(response);
        if let Some(secondary) = &self.secondary {
            secondary.post_llm_request(response);
        }
    }

    fn prepare_tool_execution(
        &self,
        ctx: PrepareToolContext<'_>,
    ) -> HookFuture<EngineResult<PrepareToolResult>> {
        // The secondary needs its own context: the owned fields are cloned
        // and the borrowed `args` reference is copied. Both stage futures are
        // built synchronously — they own their data.
        let secondary_ctx = PrepareToolContext {
            turn_id: ctx.turn_id.clone(),
            step: ctx.step,
            tool_call_id: ctx.tool_call_id.clone(),
            tool_name: ctx.tool_name.clone(),
            args: ctx.args,
            cancel: ctx.cancel.clone(),
        };
        let primary_future = self.primary.prepare_tool_execution(ctx);
        let secondary_future = self
            .secondary
            .as_ref()
            .map(|secondary| secondary.prepare_tool_execution(secondary_ctx));

        Box::pin(async move {
            let primary_result = primary_future.await?;
            match primary_result {
                // The internal host allowed the call; let the external hooks
                // weigh in.
                PrepareToolResult::Allowed => match secondary_future {
                    Some(secondary_future) => secondary_future.await,
                    None => Ok(PrepareToolResult::Allowed),
                },
                // Blocked or synthetic — the call is already settled.
                other => Ok(other),
            }
        })
    }

    fn rewrite_tool_input(
        &self,
        tool_name: &str,
        args: &mut serde_json::Value,
    ) -> EngineResult<bool> {
        // Run both stages over the same mutable args; the secondary sees the
        // primary's rewrites.
        let primary_changed = self.primary.rewrite_tool_input(tool_name, args)?;
        let secondary_changed = match &self.secondary {
            Some(secondary) => secondary.rewrite_tool_input(tool_name, args)?,
            None => false,
        };
        Ok(primary_changed || secondary_changed)
    }

    fn authorize_tool_execution(
        &self,
        ctx: AuthorizeToolContext<'_>,
    ) -> HookFuture<EngineResult<AuthorizeToolResult>> {
        // Build the secondary's context upfront (owned fields cloned, the
        // borrowed refs copied), then build both stage futures synchronously.
        let secondary_ctx = AuthorizeToolContext {
            turn_id: ctx.turn_id.clone(),
            step: ctx.step,
            tool_call_id: ctx.tool_call_id.clone(),
            tool_name: ctx.tool_name.clone(),
            args: ctx.args,
            approval_rule: ctx.approval_rule,
            display: ctx.display,
            cancel: ctx.cancel.clone(),
        };
        let primary_future = self.primary.authorize_tool_execution(ctx);
        let secondary_future = self
            .secondary
            .as_ref()
            .map(|secondary| secondary.authorize_tool_execution(secondary_ctx));

        Box::pin(async move {
            let primary_result = primary_future.await?;
            match primary_result {
                // Internal permission logic approved; consult the external
                // PermissionRequest hooks. When both approve, the primary's
                // metadata (the permission-gate exemption payload, ADR-010)
                // rides into execution — the secondary's plain Approved
                // carries no metadata of its own. Any other secondary
                // verdict stands (external-hook veto wins, unchanged).
                AuthorizeToolResult::Approved {
                    metadata: primary_metadata,
                } => match secondary_future {
                    Some(secondary_future) => match secondary_future.await? {
                        AuthorizeToolResult::Approved { .. } => Ok(AuthorizeToolResult::Approved {
                            metadata: primary_metadata,
                        }),
                        other => Ok(other),
                    },
                    None => Ok(AuthorizeToolResult::Approved {
                        metadata: primary_metadata,
                    }),
                },
                // Denied or synthetic — the call is already settled.
                other => Ok(other),
            }
        })
    }

    fn finalize_tool_result(
        &self,
        ctx: FinalizeToolContext<'_>,
    ) -> HookFuture<EngineResult<FinalizeToolResult>> {
        // `ctx.result` is a `&mut` — the primary stage uses a reborrow, and
        // once its `'static` future is built (owning all its data) the `&mut`
        // can move into the secondary context. Both stage futures are built
        // synchronously.
        let result = ctx.result;
        let primary_ctx = FinalizeToolContext {
            turn_id: ctx.turn_id.clone(),
            step: ctx.step,
            tool_call_id: ctx.tool_call_id.clone(),
            tool_name: ctx.tool_name.clone(),
            args: ctx.args,
            result: &mut *result,
            cancel: ctx.cancel.clone(),
        };
        let primary_future = self.primary.finalize_tool_result(primary_ctx);

        let secondary_ctx = FinalizeToolContext {
            turn_id: ctx.turn_id,
            step: ctx.step,
            tool_call_id: ctx.tool_call_id,
            tool_name: ctx.tool_name,
            args: ctx.args,
            result,
            cancel: ctx.cancel,
        };
        let secondary_future = self
            .secondary
            .as_ref()
            .map(|secondary| secondary.finalize_tool_result(secondary_ctx));

        Box::pin(async move {
            let primary_result = primary_future.await?;
            match secondary_future {
                Some(secondary_future) => {
                    let secondary_result = secondary_future.await?;
                    Ok(FinalizeToolResult {
                        // Either side requesting a stop stops the turn.
                        stop_turn: primary_result.stop_turn || secondary_result.stop_turn,
                    })
                }
                None => Ok(primary_result),
            }
        })
    }

    fn should_continue_after_stop(
        &self,
        ctx: ContinueAfterStopContext,
    ) -> HookFuture<EngineResult<ContinueAfterStopResult>> {
        let primary_future = self.primary.should_continue_after_stop(ctx.clone());
        let secondary_future = self
            .secondary
            .as_ref()
            .map(|secondary| secondary.should_continue_after_stop(ctx));

        Box::pin(async move {
            let primary_result = primary_future.await?;
            match secondary_future {
                Some(secondary_future) => {
                    let secondary_result = secondary_future.await?;
                    Ok(ContinueAfterStopResult {
                        // Both sides must agree to continue; a secondary with
                        // no Stop hooks returns `true` and preserves the
                        // primary's decision.
                        continue_turn: primary_result.continue_turn
                            && secondary_result.continue_turn,
                    })
                }
                None => Ok(primary_result),
            }
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::EngineError;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    /// A configurable test hook that records the calls it receives.
    struct RecordingHooks {
        name: &'static str,
        calls: Arc<StdMutex<Vec<&'static str>>>,
        block_before_step: bool,
        continue_after_stop: bool,
    }

    impl RecordingHooks {
        fn new(
            name: &'static str,
            calls: Arc<StdMutex<Vec<&'static str>>>,
            block_before_step: bool,
            continue_after_stop: bool,
        ) -> Self {
            Self {
                name,
                calls,
                block_before_step,
                continue_after_stop,
            }
        }

        fn record(&self) {
            self.calls
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(self.name);
        }
    }

    impl LoopHooks for RecordingHooks {
        fn before_step(
            &self,
            _ctx: BeforeStepContext,
        ) -> HookFuture<EngineResult<BeforeStepResult>> {
            // Record on poll (inside the future body) so tests can verify
            // short-circuiting: a future that is never awaited never polls.
            let calls = Arc::clone(&self.calls);
            let name = self.name;
            let block = self.block_before_step;
            Box::pin(async move {
                calls.lock().unwrap_or_else(|e| e.into_inner()).push(name);
                Ok(BeforeStepResult {
                    block,
                    block_reason: block.then(|| "blocked by test hook".to_string()),
                })
            })
        }

        fn should_continue_after_stop(
            &self,
            _ctx: ContinueAfterStopContext,
        ) -> HookFuture<EngineResult<ContinueAfterStopResult>> {
            let calls = Arc::clone(&self.calls);
            let name = self.name;
            let continue_turn = self.continue_after_stop;
            Box::pin(async move {
                calls.lock().unwrap_or_else(|e| e.into_inner()).push(name);
                Ok(ContinueAfterStopResult { continue_turn })
            })
        }

        fn rewrite_tool_input(
            &self,
            _tool_name: &str,
            args: &mut serde_json::Value,
        ) -> EngineResult<bool> {
            self.record();
            if self.name == "primary" {
                args["from_primary"] = serde_json::json!(true);
                Ok(true)
            } else {
                args["from_secondary"] = serde_json::json!(true);
                Ok(true)
            }
        }
    }

    fn make_context() -> BeforeStepContext {
        BeforeStepContext {
            turn_id: "turn-1".to_string(),
            step: 0,
            cancel: crate::cancel::CancelToken::new(),
        }
    }

    #[tokio::test]
    async fn should_run_primary_then_secondary_when_before_step_called() {
        // Given a composite with a primary and a secondary stage
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let primary = RecordingHooks::new("primary", Arc::clone(&calls), false, true);
        let secondary = RecordingHooks::new("secondary", Arc::clone(&calls), false, true);
        let composite = CompositeLoopHooks::new(primary, Some(secondary));

        // When before_step runs
        let result = composite.before_step(make_context()).await.unwrap();

        // Then both stages ran in order, and nothing blocked
        let recorded = calls.lock().unwrap();
        assert_eq!(*recorded, vec!["primary", "secondary"]);
        assert!(!result.block);
    }

    #[tokio::test]
    async fn should_block_when_secondary_blocks_when_before_step_called() {
        // Given a composite where the secondary blocks but the primary allows
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let primary = RecordingHooks::new("primary", Arc::clone(&calls), false, true);
        let secondary = RecordingHooks::new("secondary", Arc::clone(&calls), true, true);
        let composite = CompositeLoopHooks::new(primary, Some(secondary));

        // When before_step runs
        let result = composite.before_step(make_context()).await.unwrap();

        // Then the step is blocked with the secondary's reason
        assert!(result.block);
        assert_eq!(result.block_reason.as_deref(), Some("blocked by test hook"));
    }

    #[tokio::test]
    async fn should_short_circuit_secondary_when_primary_blocks_when_before_step_called() {
        // Given a composite where the primary blocks
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let primary = RecordingHooks::new("primary", Arc::clone(&calls), true, true);
        let secondary = RecordingHooks::new("secondary", Arc::clone(&calls), false, true);
        let composite = CompositeLoopHooks::new(primary, Some(secondary));

        // When before_step runs
        let result = composite.before_step(make_context()).await.unwrap();

        // Then only the primary ran and the step is blocked
        let recorded = calls.lock().unwrap();
        assert_eq!(*recorded, vec!["primary"]);
        assert!(result.block);
    }

    #[tokio::test]
    async fn should_require_both_stages_to_continue_when_should_continue_after_stop_called() {
        // Given a composite where the primary wants to continue but the
        // secondary does not
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let primary = RecordingHooks::new("primary", Arc::clone(&calls), false, true);
        let secondary = RecordingHooks::new("secondary", Arc::clone(&calls), false, false);
        let composite = CompositeLoopHooks::new(primary, Some(secondary));

        // When should_continue_after_stop runs
        let result = composite
            .should_continue_after_stop(ContinueAfterStopContext {
                turn_id: "turn-1".to_string(),
                stop_reason: "end_turn".to_string(),
                cancel: crate::cancel::CancelToken::new(),
            })
            .await
            .unwrap();

        // Then the turn stops (both sides must agree to continue)
        assert!(!result.continue_turn);
    }

    #[tokio::test]
    async fn should_pass_through_primary_when_no_secondary_when_before_step_called() {
        // Given a composite with no secondary stage
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let primary = RecordingHooks::new("primary", Arc::clone(&calls), false, true);
        let composite: CompositeLoopHooks<RecordingHooks, RecordingHooks> =
            CompositeLoopHooks::new(primary, None);

        // When before_step runs
        let result = composite.before_step(make_context()).await.unwrap();

        // Then only the primary ran and nothing blocked
        let recorded = calls.lock().unwrap();
        assert_eq!(*recorded, vec!["primary"]);
        assert!(!result.block);
    }

    #[tokio::test]
    async fn should_apply_both_stage_rewrites_when_rewrite_tool_input_called() {
        // Given a composite with two rewrite stages
        let calls = Arc::new(StdMutex::new(Vec::new()));
        let primary = RecordingHooks::new("primary", Arc::clone(&calls), false, true);
        let secondary = RecordingHooks::new("secondary", Arc::clone(&calls), false, true);
        let composite = CompositeLoopHooks::new(primary, Some(secondary));

        // When rewrite_tool_input runs
        let mut args = serde_json::json!({});
        let modified = composite
            .rewrite_tool_input("Read", &mut args)
            .expect("rewrite should not error");

        // Then the secondary saw the primary's rewrite and both markers exist
        assert!(modified);
        assert_eq!(args["from_primary"], serde_json::json!(true));
        assert_eq!(args["from_secondary"], serde_json::json!(true));
    }

    #[test]
    fn should_compile_with_error_types_when_stages_return_engine_errors() {
        // Given the composite type is usable with real error types
        // When the type is named
        type Composite = CompositeLoopHooks<RecordingHooks, RecordingHooks>;
        let _composite: Composite = CompositeLoopHooks::new(
            RecordingHooks::new("a", Arc::new(StdMutex::new(Vec::new())), false, true),
            None,
        );

        // Then it is constructed without error (EngineError must be Send+Sync
        // for the HookFuture bound)
        let _err = EngineError::Internal("probe".to_string());
        assert!(format!("{_err}").contains("probe"));
    }
}
