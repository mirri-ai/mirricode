//! `run_turn` — the outer agent loop.
//!
//! Iterates steps until a terminal stop reason, max steps, or cancellation.
//! The LLM provider is resolved through the [`LlmResolver`] at the top of
//! every step, so a mid-turn model switch applies on the next LLM call.
//! Each step is executed via [`execute_loop_step`]. Between steps, the loop:
//! - Accumulates token usage.
//! - Propagates sticky media-fallback flags.
//! - Calls `should_continue_after_stop` after terminal stops.
//! - Emits `TurnInterrupted` on abort.

use std::sync::{Arc, Mutex};

use crate::cancel::CancelToken;
use crate::context_memory::ContextMemory;
use crate::error::{EngineError, EngineResult};
use crate::hooks::{ContinueAfterStopContext, LoopHooks};
use crate::llm::{Llm, LlmResolver, LlmUsage};
use crate::loop_engine::events::{LiveEvent, LiveEventCallback};
use crate::loop_engine::turn_step::{TurnStepDeps, execute_loop_step, record_loop_event_wire};
use crate::loop_engine::{
    MessageBuilders, RecordStepUsageFn, RunTurnConfig, StepRetryDecision, StepStopReason,
    TurnResult, TurnStopReason, WireOpRecorder,
};
use crate::system_reminder_wire::record_system_reminders;
use crate::tool_registry::{ToolManager, ValidatorCache};

/// Driver id under which the loop engine's own step execution is tracked by
/// the step-level retry service: LLM/provider failures dominate retryable
/// step errors, and the per-driver budget prevents one flaky provider from
/// consuming the whole turn's retry budget.
const STEP_DRIVER_ID: &str = "llm";

/// Resolve the LLM a step should talk to, mapping a resolver failure into
/// the engine's `Internal` error shape.
pub(crate) fn resolve_step_llm(llm_resolver: &dyn LlmResolver) -> EngineResult<Arc<dyn Llm>> {
    llm_resolver.resolve().map_err(|reason| {
        EngineError::Internal(format!("LLM provider resolution failed: {reason}"))
    })
}

/// Run a single agent turn.
///
/// The LLM provider is resolved through `llm_resolver` at the top of every
/// loop step, so a mid-turn model switch (e.g. `agent.setModel` while the
/// turn is streaming) takes effect on the next LLM call instead of the next
/// turn. The resolved `Arc` is held for the step's duration (including its
/// step-level retries) and handed to `execute_loop_step` as `&dyn Llm`.
///
/// The loop:
/// 1. Check cancellation → return `Aborted` if cancelled.
/// 2. Check max steps → return `MaxStepsExceeded` error if exceeded.
/// 3. Resolve the step's LLM via the resolver.
/// 4. Execute a step via `execute_loop_step`.
/// 5. Accumulate usage and propagate sticky media flags.
/// 6. If `ToolUse` → continue to the next step.
/// 7. If terminal stop → call `should_continue_after_stop` hook.
/// 8. If hook says continue → loop; else → return `TurnResult`.
///
/// # Known limitation
///
/// The capability-gap injection (see `TurnFlow::run_step_loop`) reads the
/// model's capabilities once at turn start and stays turn-level sticky: a
/// mid-turn model switch does not re-compute the gap injection until the
/// next turn. This is accepted behavior, not a defect — the step's LLM
/// calls themselves always use the freshly resolved provider.
#[allow(clippy::too_many_arguments)]
pub async fn run_turn(
    config: RunTurnConfig,
    llm_resolver: &dyn LlmResolver,
    hooks: &dyn LoopHooks,
    tools: &ToolManager,
    validators: &mut ValidatorCache,
    memory: Arc<Mutex<ContextMemory>>,
    cancel: &CancelToken,
    message_builders: &MessageBuilders,
    live_callback: Option<&LiveEventCallback>,
    record_step_usage: Option<&RecordStepUsageFn>,
    wire_recorder: Option<&dyn WireOpRecorder>,
) -> EngineResult<TurnResult> {
    let mut total_usage = LlmUsage::default();
    let mut steps: u32 = 0;
    let mut media_degraded_active = false;
    let mut media_stripped_active = false;

    loop {
        // --- (A) Abort check ---
        if cancel.is_cancelled() {
            close_interrupted_turn(&memory, wire_recorder).await;
            emit_turn_interrupted(live_callback, "aborted", steps, None, None);
            return Ok(TurnResult {
                stop_reason: TurnStopReason::Aborted,
                steps,
                usage: total_usage,
            });
        }

        // --- (A2) Turn-abort guard check (session-consistency
        // invariant 2, spec §2.2 ③): before the NEXT step's LLM request
        // runs, verify the turn is still the journal's tail writer. A
        // failed guard aborts through the same cancellation path a user
        // abort takes (方式 B append-only closure → `TurnStopReason::
        // Aborted`), so the already-recorded partial work stays durable.
        // `None` (every existing caller) is a no-op.
        if let Some(guard) = config.turn_abort_guard.as_deref()
            && !guard()
        {
            tracing::warn!(
                turn_id = %config.turn_id,
                step = steps,
                "turn-abort guard tripped between steps; aborting before the next LLM request"
            );
            close_interrupted_turn(&memory, wire_recorder).await;
            emit_turn_interrupted(
                live_callback,
                "aborted",
                steps,
                None,
                Some(crate::loop_engine::shared::TURN_ABORTED_BY_GUARD_MESSAGE.to_string()),
            );
            return Ok(TurnResult {
                stop_reason: TurnStopReason::Aborted,
                steps,
                usage: total_usage,
            });
        }

        // --- (B) Max steps check ---
        if let Some(max_steps) = config.max_steps
            && max_steps > 0
            && steps >= max_steps
        {
            // The payload is the count only — the Display impl on
            // `EngineError::MaxStepsExceeded` adds the "Max steps
            // exceeded:" prefix.
            return Err(EngineError::MaxStepsExceeded(format!("{max_steps} steps")));
        }

        // --- (C) Increment step ---
        steps += 1;

        // --- (D) Resolve the LLM for this step ---
        // The provider is resolved through the resolver at the top of every
        // step so a mid-turn model switch takes effect on the next LLM call.
        // The Arc is held for the step's duration — including its step-level
        // retries, which re-enqueue the same step and keep the same instance
        // (the switch lands on the *next* step, matching the design). The
        // resolver also reports the alias (M1) the step was resolved from:
        // the step's usage is attributed to THAT model, not to the raw
        // upstream name (M2) the adapter reports (spec 11 A13).
        let (llm, model_alias) = llm_resolver.resolve_with_alias().map_err(|reason| {
            EngineError::Internal(format!("LLM provider resolution failed: {reason}"))
        })?;

        // --- (E) Execute step (with step-level retry on failure) ---
        let step_result = loop {
            match execute_loop_step(TurnStepDeps {
                llm: llm.as_ref(),
                hooks,
                tools,
                validators,
                memory: memory.clone(),
                cancel,
                turn_id: &config.turn_id,
                step: steps,
                message_builders,
                media_degraded_active,
                media_stripped_active,
                max_retry_attempts: config.max_retry_attempts,
                live_callback,
                record_step_usage,
                wire_recorder,
                model_alias: model_alias.clone(),
                turn_abort_guard: config.turn_abort_guard.clone(),
                cache_root_id: config.cache_root_id.clone(),
                wait_registry: config.wait_registry.clone(),
            })
            .await
            {
                Ok(result) => {
                    // Success ends any failure streak for this step/driver in
                    // the step-level retry service.
                    if let Some(retry_service) = config.step_retry.as_deref() {
                        retry_service.record_success(steps, STEP_DRIVER_ID);
                    }
                    break result;
                }
                Err(e) if e.is_abort() => {
                    // Cancellation mid-step: close the interrupted turn
                    // (方式 B — append-only closure), then return the turn
                    // result with the steps and usage accumulated so far.
                    // This preserves the partial work information for the
                    // caller.
                    close_interrupted_turn(&memory, wire_recorder).await;
                    emit_turn_interrupted(live_callback, "aborted", steps, Some(steps), None);
                    return Ok(TurnResult {
                        stop_reason: TurnStopReason::Aborted,
                        steps,
                        usage: total_usage,
                    });
                }
                Err(error) => {
                    // Step-level retry: when the service grants a retry,
                    // re-enqueue the same step number (the top of the loop
                    // does not increment `steps` again); otherwise surface
                    // the error to the caller.
                    let Some(retry_service) = config.step_retry.as_deref() else {
                        close_empty_response_turn(&memory, wire_recorder, &error).await;
                        return Err(error);
                    };
                    match retry_service.should_retry(steps, STEP_DRIVER_ID, &error) {
                        Some(StepRetryDecision::Retry {
                            attempt,
                            max_retries,
                        }) => {
                            tracing::warn!(
                                turn_id = %config.turn_id,
                                step = %steps,
                                attempt = %attempt,
                                max_retries = %max_retries,
                                error = %error,
                                "step failed; re-enqueuing step for step-level retry"
                            );
                            emit_step_retrying(live_callback, attempt, max_retries);
                        }
                        Some(StepRetryDecision::GiveUp) | None => {
                            close_empty_response_turn(&memory, wire_recorder, &error).await;
                            return Err(error);
                        }
                    }
                }
            }
        };

        // Accumulate usage.
        total_usage.input_tokens += step_result.usage.input_tokens;
        total_usage.output_tokens += step_result.usage.output_tokens;
        total_usage.cache_read_tokens += step_result.usage.cache_read_tokens;
        total_usage.cache_creation_tokens += step_result.usage.cache_creation_tokens;

        // Propagate sticky media flags.
        media_degraded_active = media_degraded_active || step_result.media_degraded_resend_used;
        media_stripped_active = media_stripped_active || step_result.media_stripped_resend_used;

        // --- (E) ToolUse → continue ---
        if step_result.stop_reason == StepStopReason::ToolUse {
            continue;
        }

        // --- (F) Terminal stop → should_continue_after_stop hook ---
        let terminal = step_result.stop_reason;
        let turn_stop_reason = map_step_to_turn_stop_reason(&terminal);

        let continue_ctx = ContinueAfterStopContext {
            turn_id: config.turn_id.clone(),
            stop_reason: format!("{:?}", terminal),
            cancel: cancel.clone(),
        };
        let continuation = hooks.should_continue_after_stop(continue_ctx).await?;

        if !continuation.continue_turn {
            return Ok(TurnResult {
                stop_reason: turn_stop_reason,
                steps,
                usage: total_usage,
            });
        }

        // Hook says continue — loop to the next step.
    }
}

/// Close an interrupted turn (方式 B 取消闭合): synthesize interrupted tool
/// results for calls that never got results, settle any still-open assistant
/// step, flush deferred messages, and persist the closure loop events to the
/// wire log so a resume replays the identical closed state.
///
/// Append-only by construction — the next request is a pure extension of what
/// the model (and the user) already saw, so the delivered prefix stays valid
/// for prefix caching.
async fn close_interrupted_turn(
    memory: &Arc<Mutex<ContextMemory>>,
    wire_recorder: Option<&dyn WireOpRecorder>,
) {
    let events = {
        let mut mem = memory.lock().unwrap_or_else(|e| e.into_inner());
        mem.close_interrupted()
    };
    for event in &events {
        record_loop_event_wire(wire_recorder, event).await;
    }
}

/// The user-visible reminder appended when a turn dies to empty model
/// responses (turn-scheduling US1.4 / SDR-002 / ADR-002).
const EMPTY_RESPONSE_REMINDER: &str = "The previous turn ended because the model repeatedly \
returned an empty response (no text, no thinking, no tool calls). Please re-send your request.";

/// Close an empty-response turn (SDR-002/ADR-002): the model kept returning
/// nothing and the step-level retry budget is drained (or no service is
/// wired). Append the visible reminder to memory and the wire log so the
/// user — and a replayed session — sees why the turn died instead of a
/// silent "completed"; the turn then settles through the regular failure
/// path (activity "failed", StopFailure hook, error surfaced to the caller),
/// never a completed `turn.ended`. No-op for any other error.
///
/// Uses the same system-reminder mechanism as the user-cancel closure so
/// the append is replay-equivalent (ADR-009) and independent of any open
/// step (the failed attempt's step has no content events to attach to).
async fn close_empty_response_turn(
    memory: &Arc<Mutex<ContextMemory>>,
    wire_recorder: Option<&dyn WireOpRecorder>,
    error: &EngineError,
) {
    if !matches!(error, EngineError::EmptyLlmResponse) {
        return;
    }
    memory
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .append_system_reminder(EMPTY_RESPONSE_REMINDER.to_string());
    record_system_reminders(wire_recorder, &[EMPTY_RESPONSE_REMINDER.to_string()]).await;
}

/// Map a [`StepStopReason`] to a [`TurnStopReason`].
fn map_step_to_turn_stop_reason(step: &StepStopReason) -> TurnStopReason {
    match step {
        StepStopReason::EndTurn => TurnStopReason::EndTurn,
        StepStopReason::MaxTokens => TurnStopReason::MaxTokens,
        StepStopReason::Filtered => TurnStopReason::Filtered,
        StepStopReason::Paused => TurnStopReason::Paused,
        StepStopReason::Unknown => TurnStopReason::Unknown,
        StepStopReason::ToolUse => {
            // Unreachable in practice — ToolUse continues the loop.
            // Defensive: treat as Unknown.
            TurnStopReason::Unknown
        }
    }
}

/// Emit a `TurnInterrupted` live event if a callback is present.
fn emit_turn_interrupted(
    live_callback: Option<&LiveEventCallback>,
    reason: &str,
    attempted_steps: u32,
    active_step: Option<u32>,
    message: Option<String>,
) {
    if let Some(cb) = live_callback {
        cb(LiveEvent::TurnInterrupted {
            reason: reason.to_string(),
            attempted_steps,
            active_step,
            message,
        });
    }
}

/// Emit a `StepRetrying` live event for a step-level retry if a callback is
/// present. Step-level retries re-enqueue immediately (no delay), so
/// `delay_ms` is always 0.
fn emit_step_retrying(
    live_callback: Option<&LiveEventCallback>,
    retry_attempt: u32,
    max_retries: u32,
) {
    if let Some(cb) = live_callback {
        cb(LiveEvent::StepRetrying {
            failed_attempt: retry_attempt,
            next_attempt: retry_attempt + 1,
            max_attempts: max_retries + 1,
            delay_ms: 0,
        });
    }
}
