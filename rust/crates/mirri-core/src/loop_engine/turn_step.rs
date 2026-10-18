//! `execute_loop_step` — a single provider step within the agent loop.
//!
//! The step:
//! 1. Runs the `before_step` hook (can block the step).
//! 2. Resolves tools and builds messages (with media-fallback selection).
//! 3. Calls `chat_with_retry` (with media-fallback chain on failure).
//! 4. Records block-level events into `ContextMemory`.
//! 5. Executes tool calls (if `ToolUse`) or records them as unexecuted.
//! 6. Runs the `after_step` hook (can force `EndTurn`).
//! 7. Returns a [`StepResult`].

use std::sync::{Arc, Mutex};

use mirri_proto::WireOp;
use uuid::Uuid;

use crate::cancel::CancelToken;
use crate::context_memory::{ContextMemory, LoopEvent};
use crate::error::{EngineError, EngineResult};
use crate::hooks::{AfterStepContext, BeforeStepContext, LoopHooks};
use crate::llm::{Llm, LlmChatParams};
use crate::loop_engine::events::{LiveEvent, LiveEventCallback};
use crate::loop_engine::retry::{ChatResult, chat_with_retry};
use crate::loop_engine::tool_call_batch::{
    derive_step_stop_reason, execute_tool_calls, record_unexecuted_tool_calls,
};
use crate::loop_engine::{
    MessageBuilders, RecordStepUsageFn, StepResult, StepStopReason, TurnAbortGuard, WireOpRecorder,
};
use crate::message_pipeline::{MediaCapabilityAdapter, MessagePipeline, VideoReferenceResolver};
use crate::system_reminder_wire::record_system_reminders;
use crate::tool_registry::{ToolManager, ValidatorCache};
use crate::turn_flow::wait_registry::{WaitKind, WaitSubject};

/// Dependencies for executing a single loop step.
///
/// All references are borrowed for the duration of the step. The lifetime
/// parameter `'a` ties all references to the caller's scope.
pub struct TurnStepDeps<'a> {
    /// The LLM adapter.
    pub llm: &'a dyn Llm,
    /// Host hooks (before_step, after_step, etc.).
    pub hooks: &'a dyn LoopHooks,
    /// The tool registry.
    pub tools: &'a ToolManager,
    /// Argument validators (mutated — caches validators per tool).
    pub validators: &'a mut ValidatorCache,
    /// The canonical context memory (shared via `Arc<Mutex<>>` — records events).
    ///
    /// The lock is held only for short synchronous critical sections, never
    /// across `.await` points. This allows the host hooks to also hold a
    /// clone of the same `Arc<Mutex<>>` and modify memory independently.
    pub memory: Arc<Mutex<ContextMemory>>,
    /// Cancellation token.
    pub cancel: &'a CancelToken,
    /// Turn ID.
    pub turn_id: &'a str,
    /// Step number (1-based).
    pub step: u32,
    /// Message builders for different projections.
    pub message_builders: &'a MessageBuilders,
    /// Whether media-degraded projection is active (sticky from prior steps).
    pub media_degraded_active: bool,
    /// Whether media-stripped projection is active (sticky from prior steps).
    pub media_stripped_active: bool,
    /// Maximum retry attempts for `chat_with_retry`.
    pub max_retry_attempts: u32,
    /// Optional callback for live events (deltas, retries).
    pub live_callback: Option<&'a LiveEventCallback>,
    /// Optional callback for recording step usage (host can stop the turn).
    pub record_step_usage: Option<&'a RecordStepUsageFn>,
    /// Optional wire-op recorder: loop events and step usage are persisted
    /// through it (e.g. to the session's `wire.jsonl`). `None` disables
    /// recording (unit tests, replay).
    pub wire_recorder: Option<&'a dyn WireOpRecorder>,
    /// The model ALIAS (M1) this step was resolved from, when the resolver
    /// reports one — the per-request attribution key for the wire
    /// `usage.record` op's model field (spec 11 A13, TS parity: the TS
    /// kernel records under `config.model`). `None` (resolvers with no
    /// alias notion — fixed-instance resolvers, test doubles) falls back
    /// to the LLM's raw upstream name (`llm.model_name()`, M2).
    pub model_alias: Option<String>,
    /// The turn-abort guard (session-consistency invariant 2, spec §2.2
    /// ③): polled immediately before the step's tool batch dispatches.
    /// A `false` return fails the step with `EngineError::Aborted`, which
    /// the turn loop folds into the 方式 B append-only closure. `None`
    /// (every existing caller) is a no-op.
    pub turn_abort_guard: Option<TurnAbortGuard>,
    /// The fork chain's cache root id (the provider prompt-cache routing
    /// key), threaded from the turn flow. Rides every step's
    /// [`LlmChatParams`](crate::llm::LlmChatParams) so the provider's
    /// request carries the session's cache lineage. `None` omits it.
    pub cache_root_id: Option<String>,
    /// The turn's ADR-006 wait registry: the step's await points register
    /// into it — `WaitingProvider` around the model-stream collection
    /// (inside `chat_with_retry`) and `WaitingTool` around the tool batch.
    /// `None` (standalone tests, non-flow callers) registers nothing.
    pub wait_registry: Option<std::sync::Arc<crate::turn_flow::WaitRegistry>>,
}

/// The model-visible interrupted anchor appended to a partially-delivered
/// assistant step when the stream is cancelled mid-flight (方式 B 取消闭合).
/// Deterministic, byte-stable text so the next request keeps the delivered
/// prefix and marks where the output was cut.
pub const INTERRUPTED_ANCHOR: &str = "[Request interrupted]";
/// The anchor wording for user-initiated cancellations.
pub const INTERRUPTED_ANCHOR_USER: &str = "[Request interrupted by user]";

/// Pick the interrupted anchor text for the current cancellation state.
fn interrupted_anchor_text(cancel: &CancelToken) -> &'static str {
    if cancel.is_user_cancelled() {
        INTERRUPTED_ANCHOR_USER
    } else {
        INTERRUPTED_ANCHOR
    }
}

/// Check if an error message indicates a 413 / too-large error.
fn is_too_large_error(msg: &str) -> bool {
    msg.contains("413")
        || msg.contains("too large")
        || msg.contains("Too Large")
        || msg.contains("payload too large")
        || msg.contains("request too large")
}

/// Check if an error message indicates an image format/unsupported error.
fn is_image_format_error(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    lower.contains("image") && (lower.contains("format") || lower.contains("unsupported"))
}

/// Check if an error message indicates a 400 / structure error.
fn is_structure_error(msg: &str) -> bool {
    msg.contains("400")
        && (msg.contains("structure")
            || msg.contains("invalid")
            || msg.contains("adjacent")
            || msg.contains("alternating"))
}

/// Build `LlmChatParams` from messages, tools, cancel token, and the fork
/// chain's cache root id (the provider prompt-cache routing key — `None`
/// omits the provider's cache-routing field).
fn build_chat_params(
    messages: Vec<crate::message::Message>,
    tools: &[crate::tool::ToolDefinition],
    cancel: &CancelToken,
    cache_root_id: Option<&str>,
) -> LlmChatParams {
    LlmChatParams {
        messages,
        tools: tools.to_vec(),
        cancel: cancel.clone(),
        request_log_fields: None,
        cache_root_id: cache_root_id.map(|s| s.to_string()),
    }
}

/// Select the message builder based on sticky media flags.
///
/// - If `media_degraded_active` → use `media_degraded` builder (if available).
/// - Else if `media_stripped_active` → use `media_stripped` builder (if available).
/// - Else → use `normal` builder.
fn select_message_builder(
    message_builders: &MessageBuilders,
    media_degraded_active: bool,
    media_stripped_active: bool,
) -> EngineResult<Vec<crate::message::Message>> {
    if media_degraded_active && let Some(ref builder) = message_builders.media_degraded {
        return builder();
    }
    if media_stripped_active && let Some(ref builder) = message_builders.media_stripped {
        return builder();
    }
    (message_builders.normal)()
}

/// Build the per-step post-projection media adaptation pipeline (multimodal
/// spec §5), bound to this LLM's capabilities. Rebuilt per use so a mid-turn
/// model switch takes effect on the very next request.
///
/// Every request assembly passes through it — the step's main request AND
/// every in-step fallback retry — so no request shape can hand provider
/// media to a model that does not support the modality (ROADMAP §3).
fn media_pipeline(llm: &dyn Llm) -> MessagePipeline {
    let mut pipeline = MessagePipeline::new();
    // ADR-003 revision: an undeclared capability is unsupported — every
    // model without video_in gets the video → text pre-replacement.
    pipeline.register(Box::new(VideoReferenceResolver::for_capability(
        llm.capability(),
    )));
    pipeline.register(Box::new(MediaCapabilityAdapter::new(
        llm.capability().clone(),
    )));
    pipeline
}

/// Execute a single loop step.
///
/// See module-level documentation for the full sequence.
pub async fn execute_loop_step(deps: TurnStepDeps<'_>) -> EngineResult<StepResult> {
    let TurnStepDeps {
        llm,
        hooks,
        tools,
        validators,
        memory,
        cancel,
        turn_id,
        step,
        message_builders,
        media_degraded_active,
        media_stripped_active,
        max_retry_attempts,
        live_callback,
        record_step_usage,
        wire_recorder,
        model_alias,
        turn_abort_guard,
        cache_root_id,
        wait_registry,
    } = deps;

    // The model this step's usage is attributed to: the alias (M1) the
    // resolver resolved the step's LLM from, falling back to the LLM's
    // raw upstream name (M2) when the resolver reports no alias (spec 11
    // A13 — both the wire `usage.record` model field and the in-memory
    // `by_model` key use the alias).
    let usage_model = model_alias.as_deref().unwrap_or(llm.model_name());

    // --- 1. before_step hook ---
    let before_ctx = BeforeStepContext {
        turn_id: turn_id.to_string(),
        step,
        cancel: cancel.clone(),
    };
    let before_result = hooks.before_step(before_ctx).await?;
    if before_result.block {
        return Err(EngineError::HookBlocked(
            before_result
                .block_reason
                .unwrap_or_else(|| "Step blocked by before_step hook".to_string()),
        ));
    }

    // --- 2. Cancel check ---
    cancel.check()?;

    // --- 3. Resolve tools ---
    let mut step_tools = tools.loop_tools();
    // Multimodal spec US4.3 (ADR-003 revision: undefined is unsupported):
    // a model that does not declare image or video input is not offered
    // the media-reading tool (it would only set up a doomed call), mirroring
    // the TS registration rule (image_in || video_in). Evaluated per step
    // so a mid-turn model switch takes effect immediately.
    {
        let caps = llm.capability();
        if !caps.image_in && !caps.video_in {
            step_tools.retain(|tool| tool.name != "ReadMediaFile");
        }
    }

    // --- 4. Build messages (with sticky media selection) ---
    let messages = select_message_builder(
        message_builders,
        media_degraded_active,
        media_stripped_active,
    )?;

    // --- 4b. Post-projection media adaptation (multimodal spec §5) ---
    // The pipeline is rebuilt per step so a mid-turn model switch takes
    // effect on the very next step; the capability-gap injection stays
    // sticky to the turn-start model (known, accepted limitation).
    // Applies to every request shape (normal / degraded / stripped /
    // strict) — adaptation happens before protocol encoding (§5.1). The
    // in-step fallback retries go through the same pipeline (see
    // [`try_media_fallback`]) so no request assembly bypasses it.
    let projection_label = if media_stripped_active {
        "media_stripped"
    } else if media_degraded_active {
        "media_degraded"
    } else {
        "normal"
    };
    let messages = media_pipeline(llm).run(projection_label, messages);

    // --- 5. Cancel check ---
    cancel.check()?;

    // --- 6. Generate step UUID ---
    let step_id = Uuid::new_v4().to_string();

    // --- 7. Dispatch StepBegin ---
    let step_begin = LoopEvent::StepBegin {
        step_id: step_id.clone(),
        entry_seq: None,
    };
    memory
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .append_loop_event(step_begin.clone());
    record_loop_event_wire(wire_recorder, &step_begin).await;

    // --- 8. Build LlmChatParams ---
    let params = build_chat_params(messages, &step_tools, cancel, cache_root_id.as_deref());

    // --- 9. pre_llm_request hook (fire-and-forget observer) ---
    hooks.pre_llm_request(&params);

    // --- 10. chat_with_retry ---
    let chat_result = match chat_with_retry(
        llm,
        params,
        max_retry_attempts,
        live_callback,
        turn_id,
        step,
        wait_registry.as_ref(),
    )
    .await
    {
        Ok(result) => result,
        Err(error) => {
            // --- 11. Media fallback chain ---
            match try_media_fallback(
                llm,
                tools,
                &step_tools,
                cancel,
                message_builders,
                max_retry_attempts,
                live_callback,
                turn_id,
                step,
                &error,
                cache_root_id.as_deref(),
                wait_registry.as_ref(),
            )
            .await
            {
                Some((result, degraded, stripped)) => {
                    // The fallback projection changed the request envelope:
                    // record the explicit prefix-drift marker (P8a-4) so the
                    // change is auditable on the wire log.
                    if degraded || stripped {
                        let reason = if degraded {
                            "media_projection_degraded"
                        } else {
                            "media_projection_stripped"
                        };
                        if let Some(recorder) = wire_recorder {
                            recorder
                                .record_op(WireOp::RequestHeaderChanged {
                                    reason: reason.to_string(),
                                })
                                .await;
                        }
                    }
                    // Return early with media fallback flags.
                    return finalize_step(
                        result,
                        step_id,
                        step,
                        turn_id,
                        memory.clone(),
                        cancel,
                        hooks,
                        validators,
                        tools,
                        live_callback,
                        record_step_usage,
                        wire_recorder,
                        usage_model,
                        degraded,
                        stripped,
                        turn_abort_guard.as_ref(),
                        wait_registry.as_ref(),
                    )
                    .await;
                }
                None => return Err(error),
            }
        }
    };

    // --- 12-21. Finalize step (no media fallback used) ---
    finalize_step(
        chat_result,
        step_id,
        step,
        turn_id,
        memory,
        cancel,
        hooks,
        validators,
        tools,
        live_callback,
        record_step_usage,
        wire_recorder,
        usage_model,
        false,
        false,
        turn_abort_guard.as_ref(),
        wait_registry.as_ref(),
    )
    .await
}

/// Try media fallback projections when `chat_with_retry` fails.
///
/// Implements a true **chain**: if one fallback fails, the next is tried.
///
/// - 413 / too large → `media_degraded` → (if still 413) → `media_stripped` → (if still 413) → `strict`
/// - image format error → `media_stripped` → `strict`  (skips degraded — not a size issue)
/// - 400 / structure error → `strict`
///
/// Returns `Some((ChatResult, media_degraded_used, media_stripped_used))` if a
/// fallback succeeded, or `None` if no fallback was applicable.
#[allow(clippy::too_many_arguments)]
async fn try_media_fallback(
    llm: &dyn Llm,
    _tools: &ToolManager,
    step_tools: &[crate::tool::ToolDefinition],
    cancel: &CancelToken,
    message_builders: &MessageBuilders,
    max_retry_attempts: u32,
    live_callback: Option<&LiveEventCallback>,
    turn_id: &str,
    step: u32,
    original_error: &EngineError,
    cache_root_id: Option<&str>,
    wait_registry: Option<&std::sync::Arc<crate::turn_flow::WaitRegistry>>,
) -> Option<(ChatResult, bool, bool)> {
    let error_msg = original_error.to_string();

    // Determine the fallback sequence based on the error type.
    let sequence = if is_too_large_error(&error_msg) {
        // 413: try degraded → stripped → strict. The stripped placeholders
        // keep the size-limit copy (US6.1) — the flag stays clear.
        FallbackLevel::Degraded
    } else if is_image_format_error(&error_msg) {
        // Image format: skip degraded (not a size issue), go straight to
        // stripped. Set the sticky flag FIRST so the stripped retry (and
        // every sticky stripped replay for the rest of the turn) builds
        // format-rejected placeholders carrying the conversion guidance
        // (US6.2) instead of the size-limit copy.
        message_builders
            .format_rejected_flag
            .store(true, std::sync::atomic::Ordering::SeqCst);
        FallbackLevel::Stripped
    } else if is_structure_error(&error_msg) {
        // 400 structure: only try strict.
        FallbackLevel::Strict
    } else {
        // Error type not eligible for media fallback.
        return None;
    };

    // Walk the chain from the starting level.
    let mut level = sequence;
    loop {
        let (builder, degraded_used, stripped_used, next_level) = match level {
            FallbackLevel::Degraded => (
                message_builders.media_degraded.as_ref(),
                true,
                false,
                Some(FallbackLevel::Stripped),
            ),
            FallbackLevel::Stripped => (
                message_builders.media_stripped.as_ref(),
                false,
                true,
                Some(FallbackLevel::Strict),
            ),
            FallbackLevel::Strict => (message_builders.strict.as_ref(), false, false, None),
        };

        if let Some(builder) = builder {
            let level_label = match level {
                FallbackLevel::Degraded => "media-degraded",
                FallbackLevel::Stripped => "media-stripped",
                FallbackLevel::Strict => "strict",
            };
            tracing::info!(
                turn_id = %turn_id,
                step = %step,
                "Retrying with {} projection (fallback chain)",
                level_label
            );

            match builder() {
                Ok(alt_messages) => {
                    // 4b parity: the fallback retry is a request assembly
                    // too — run it through the same media adaptation
                    // pipeline before protocol encoding, so a strict
                    // projection rebuilt straight from context memory
                    // cannot hand provider media to a model known not to
                    // support the modality (ROADMAP §3, plan constraint:
                    // every request shape, including retries/fallbacks).
                    let pipeline = media_pipeline(llm);
                    let alt_messages = pipeline.run(level_label, alt_messages);
                    let alt_params =
                        build_chat_params(alt_messages, step_tools, cancel, cache_root_id);
                    match chat_with_retry(
                        llm,
                        alt_params,
                        max_retry_attempts,
                        live_callback,
                        turn_id,
                        step,
                        wait_registry,
                    )
                    .await
                    {
                        Ok(result) => {
                            return Some((result, degraded_used, stripped_used));
                        }
                        Err(fallback_error) => {
                            tracing::info!(
                                turn_id = %turn_id,
                                step = %step,
                                error = %fallback_error,
                                "{} fallback failed, continuing chain",
                                level_label
                            );
                        }
                    }
                }
                Err(build_error) => {
                    tracing::warn!(
                        turn_id = %turn_id,
                        step = %step,
                        error = %build_error,
                        "{} builder failed, continuing chain",
                        level_label
                    );
                }
            }
        }

        // Move to the next level in the chain, or stop.
        match next_level {
            Some(next) => level = next,
            None => return None,
        }
    }
}

/// The level of media fallback to attempt.
#[derive(Clone, Copy)]
enum FallbackLevel {
    /// Replace media with text placeholders (first level for 413).
    Degraded,
    /// Strip all media entirely (second level for 413, first for image errors).
    Stripped,
    /// Strict wire projection (final level for 413/structure errors).
    Strict,
}

/// Finalize the step: record events, execute tools, run hooks, return result.
///
/// This is the shared tail of `execute_loop_step` for both the normal path
/// and the media-fallback path.
///
/// Memory is accessed via `Arc<Mutex<ContextMemory>>`. Each access locks the
/// mutex for a short synchronous critical section, never across `.await`
/// points.
#[allow(clippy::too_many_arguments)]
async fn finalize_step(
    chat_result: ChatResult,
    step_id: String,
    step: u32,
    turn_id: &str,
    memory: Arc<Mutex<ContextMemory>>,
    cancel: &CancelToken,
    hooks: &dyn LoopHooks,
    validators: &mut ValidatorCache,
    tools: &ToolManager,
    live_callback: Option<&LiveEventCallback>,
    record_step_usage: Option<&RecordStepUsageFn>,
    wire_recorder: Option<&dyn WireOpRecorder>,
    model_name: &str,
    media_degraded_used: bool,
    media_stripped_used: bool,
    turn_abort_guard: Option<&TurnAbortGuard>,
    wait_registry: Option<&std::sync::Arc<crate::turn_flow::WaitRegistry>>,
) -> EngineResult<StepResult> {
    let ChatResult {
        response,
        think_text,
        interrupted,
    } = chat_result;
    let usage = response.usage.clone();
    let has_tool_calls = !response.tool_calls.is_empty();

    // --- 12. Record usage (host callback) ---
    // The response's NATURAL stop reason rides the callback (before any
    // forced stop is applied): the hard-budget grace decision in the
    // turn-flow closure needs to distinguish "the model ended the step in
    // tool calls" (grace step so it can write a final message) from a
    // natural end (stop immediately). Same for the 1-based step number —
    // the grace is only granted while the turn's step budget has room.
    let natural_stop = derive_step_stop_reason(&response.stop_reason, has_tool_calls);
    let mut stop_turn_after_usage = false;
    if let Some(cb) = record_step_usage {
        let result = cb(usage.clone(), natural_stop.clone(), step as usize).await?;
        if result == Some(true) {
            stop_turn_after_usage = true;
        }
    }

    // --- 12b. Persist step usage to the wire log (best-effort) ---
    // The model field carries the step's ALIAS (M1) when the resolver
    // reported one — TS parity (the TS kernel records under
    // `config.model`); the raw upstream name only serves resolvers with
    // no alias notion (spec 11 A13).
    if let Some(recorder) = wire_recorder {
        recorder
            .record_op(WireOp::UsageRecord {
                model: model_name.to_string(),
                usage: serde_json::json!({
                    "inputTokens": usage.input_tokens,
                    "outputTokens": usage.output_tokens,
                    "cacheReadTokens": usage.cache_read_tokens,
                    "cacheCreationTokens": usage.cache_creation_tokens,
                }),
                usage_scope: None,
            })
            .await;
    }

    // --- 13. Derive step stop reason ---
    let stop_reason = natural_stop;

    // --- 14. post_llm_request hook (fire-and-forget observer) ---
    hooks.post_llm_request(&response);

    // --- 14b. Empty-response guard (turn-scheduling US1.4 / SDR-002 /
    // ADR-002) ---
    // A step that delivered NOTHING (no text, no thinking, no tool calls)
    // under any stop shape that settles as "completed" on the wire — every
    // non-tool-use, non-filtered stop reason (EndTurn, Unknown, MaxTokens,
    // Paused all render "completed" in `turn_stop_reason_wire`) — is
    // anomalous and never a legitimate turn completion. Surface it as the
    // dedicated error so the step-level retry service re-enqueues the step;
    // a drained budget fails the turn explicitly (with the wire-visible
    // marker appended by `run_turn`) instead of silently completing it.
    // Cancellation is excluded — an interrupted stream takes the abort path
    // (its delivered-partial-output closure is 方式 B); a cancel that races
    // past the `interrupted` read is caught by the step-end cancel check.
    // Filtered is excluded — the provider already said content was removed,
    // and the turn settles "filtered", not "completed". The empty attempt
    // appends nothing to memory, so a retry reuses the identical request
    // prefix and the provider's prefix cache still hits. The guard runs
    // AFTER post_llm_request so response observers see every attempt.
    let empty_settling_as_completed = !interrupted
        && matches!(
            stop_reason,
            StepStopReason::EndTurn
                | StepStopReason::Unknown
                | StepStopReason::MaxTokens
                | StepStopReason::Paused
        )
        && response.text.is_empty()
        && think_text.is_empty()
        && response.tool_calls.is_empty();
    if empty_settling_as_completed {
        tracing::warn!(
            turn_id = turn_id,
            step = step,
            stop_reason = ?stop_reason,
            "empty LLM response (no text, no thinking, no tool calls); \
             surfacing as a step error for step-level retry"
        );
        return Err(EngineError::EmptyLlmResponse);
    }

    // --- 15. Dispatch recorded events from ChatResult. Loop events are
    // folded into memory under the lock, then persisted to the wire log
    // after it is released (recording is async) — order is preserved.
    let step_events = {
        let mut mem = memory.lock().unwrap_or_else(|e| e.into_inner());
        let mut events = Vec::new();
        // Think first, then text: the streaming providers send reasoning
        // before content (OpenAI/DeepSeek convention), and the live
        // projector renders parts in arrival order — the wire record must
        // preserve that order or a re-rendered session shows the thinking
        // block after the answer (the wire replay equivalence invariant).
        let has_delivered_output = !response.text.is_empty() || !think_text.is_empty();
        if !think_text.is_empty() {
            let event = LoopEvent::ThinkPart {
                step_id: step_id.clone(),
                text: think_text,
            };
            mem.append_loop_event(event.clone());
            events.push(event);
        }
        if !response.text.is_empty() {
            let event = LoopEvent::ContentPart {
                step_id: step_id.clone(),
                text: response.text.clone(),
            };
            mem.append_loop_event(event.clone());
            events.push(event);
        }
        if interrupted && has_delivered_output {
            // 方式 B 取消闭合（方式 B cancellation closure): the stream was
            // cancelled after the user already saw (and we just recorded) a
            // partial answer. Append the interrupted anchor so the next
            // request keeps the delivered prefix and marks where it was cut,
            // instead of silently presenting the partial text as complete.
            let event = LoopEvent::ContentPart {
                step_id: step_id.clone(),
                text: interrupted_anchor_text(cancel).to_string(),
            };
            mem.append_loop_event(event.clone());
            events.push(event);
        }
        // (Tool calls are NOT recorded here: their `LoopEvent::ToolCall` and
        // the live `ToolCallStarted` carry the final — possibly
        // hook-rewritten — arguments, which are only known after the
        // prepare hooks adjudicate. The dispatch closure below records
        // them in provider order as each call settles. TS parity:
        // `dispatchToolCall` fires from `prepareToolCall`, not from the
        // response handling.)
        events
    };
    for event in &step_events {
        record_loop_event_wire(wire_recorder, event).await;
    }

    // --- 16-17. Tool call batch + effective stop reason ---
    let mut effective = stop_reason.clone();

    // If record_step_usage says stop, force EndTurn.
    if stop_turn_after_usage && effective == StepStopReason::ToolUse {
        effective = StepStopReason::EndTurn;
    }

    let mut stop_turn_from_tools = false;

    if effective == StepStopReason::ToolUse {
        // Mid-turn tool-batch guard (session-consistency invariant 2, spec
        // §2.2 ③): the LAST check before the batch's tool calls start
        // running — the exact boundary the spec names ("下个工具批前").
        // A tripped guard fails the step with `EngineError::Aborted`,
        // which the turn loop's `is_abort` branch folds into the 方式 B
        // append-only closure (the step's own content events are already
        // recorded above — the partial work stays durable). `None`
        // (every existing caller) is a no-op.
        if turn_abort_guard.is_some_and(|guard| !guard()) {
            tracing::warn!(
                turn_id = %turn_id,
                step = step,
                "turn-abort guard tripped before the tool batch; aborting the turn"
            );
            return Err(EngineError::Aborted(
                crate::loop_engine::shared::TURN_ABORTED_BY_GUARD_MESSAGE.to_string(),
            ));
        }

        // Dispatch: record each settled tool call — after preflight and the
        // prepare hooks (PreToolUse rewriting, authorization) have decided
        // its final arguments — into context memory, the wire log queue, and
        // the live event stream. The UI and the replayed session therefore
        // see the arguments the call actually runs with (TS parity:
        // `dispatchToolCall` publishes `tool.call.started` and records the
        // loop event with the effective args).
        let mut tool_call_events: Vec<LoopEvent> = Vec::new();
        let mut dispatch_tool_call = |record: &crate::tool_registry::DispatchedToolCall<'_>| {
            let event = LoopEvent::ToolCall {
                step_id: step_id.clone(),
                tool_call_id: record.tool_call_id.to_string(),
                tool_name: record.tool_name.to_string(),
                args: record.args.clone(),
            };
            memory
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .append_loop_event(event.clone());
            tool_call_events.push(event);
            if let Some(cb) = live_callback {
                cb(LiveEvent::ToolCallStarted {
                    tool_name: record.tool_name.to_string(),
                    tool_call_id: record.tool_call_id.to_string(),
                    args: record.args.clone(),
                });
            }
        };

        // Execute tool calls — under the ADR-006 `WaitingTool`
        // registration: one entry per batch, the tool's own deadline as
        // the owner fact (a bounded tool's timeout is the fact source;
        // an unbounded silent tool rides existence-liveness — the kind
        // window still applies, the best available fact for the class).
        let _tool_wait = wait_registry.as_ref().map(|registry| {
            registry.enter(WaitKind::WaitingTool, WaitSubject::Tool { deadline: None })
        });
        let tool_results = execute_tool_calls(
            &response.tool_calls,
            tools,
            validators,
            turn_id,
            step,
            cancel,
            live_callback,
            hooks,
            &mut dispatch_tool_call,
            wait_registry,
        )
        .await;

        // Persist the dispatched tool-call events (the memory appends
        // already happened inside the dispatch closure; the wire records
        // follow them, and precede the tool results below — the wire replay
        // equivalence invariant).
        for event in &tool_call_events {
            record_loop_event_wire(wire_recorder, event).await;
        }

        // Record tool results in memory, then persist them (order kept).
        let (tool_result_events, tool_injected_reminders) = {
            let mut mem = memory.lock().unwrap_or_else(|e| e.into_inner());
            let mut events = Vec::new();
            let mut injected_reminders = Vec::new();
            // Resolve each call id back to its tool name so the live
            // completion event can carry it (the wire `tool.result` name —
            // a consumer that missed the started event still attributes
            // the result to its tool). Unknown ids (orphaned results)
            // degrade to an empty name.
            let names: std::collections::HashMap<&str, &str> = response
                .tool_calls
                .iter()
                .map(|tc| (tc.id.as_str(), tc.name.as_str()))
                .collect();
            for (tool_call_id, result) in &tool_results {
                let event = LoopEvent::ToolResult {
                    tool_call_id: tool_call_id.clone(),
                    output: result.output.clone(),
                    is_error: result.is_error,
                    note: result.note.clone(),
                    entry_seq: None,
                };
                mem.append_loop_event(event.clone());
                events.push(event);
                if result.stop_turn {
                    stop_turn_from_tools = true;
                }
                // 当工具返回了注入内容时 (如 SkillTool), 以系统提醒的形式
                // 追加到上下文记忆中, 使模型能在下一步看到 skill 指令。
                // The reminder is ALSO collected for the wire record below —
                // a session restored from the wire log must see the same
                // reminder the live session appended (the wire replay
                // equivalence invariant, ADR-009). (When several results
                // are pending the memory append defers to the exchange
                // close; the record follows the append, so the common
                // single-result case replays at the identical position.)
                if let Some(ref injection) = result.injection {
                    mem.append_system_reminder(injection.clone());
                    injected_reminders.push(injection.clone());
                }
                // Emit live event so the UI can show "tool completed".
                if let Some(cb) = live_callback {
                    cb(LiveEvent::ToolCallCompleted {
                        tool_call_id: tool_call_id.clone(),
                        tool_name: names
                            .get(tool_call_id.as_str())
                            .copied()
                            .unwrap_or_default()
                            .to_string(),
                        is_error: result.is_error,
                        output: result.output.clone(),
                    });
                }
            }
            (events, injected_reminders)
        };
        for event in &tool_result_events {
            record_loop_event_wire(wire_recorder, event).await;
        }
        record_system_reminders(wire_recorder, &tool_injected_reminders).await;

        if stop_turn_from_tools {
            effective = StepStopReason::EndTurn;
        }
    } else if matches!(
        effective,
        StepStopReason::Paused | StepStopReason::Unknown | StepStopReason::MaxTokens
    ) && has_tool_calls
    {
        // Model stopped before tool execution could begin — record
        // unexecuted synthetic results. The events are returned so they can
        // be persisted after the memory lock is released.
        let unexecuted_events = {
            let mut mem = memory.lock().unwrap_or_else(|e| e.into_inner());
            record_unexecuted_tool_calls(&response.tool_calls, &step_id, &mut mem)
        };
        for event in &unexecuted_events {
            record_loop_event_wire(wire_recorder, event).await;
        }
    }

    // --- 18. Dispatch StepEnd ---
    let step_end = LoopEvent::StepEnd {
        step_id: step_id.clone(),
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_tokens: usage.cache_read_tokens,
        cache_creation_tokens: usage.cache_creation_tokens,
    };
    memory
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .append_loop_event(step_end.clone());
    record_loop_event_wire(wire_recorder, &step_end).await;

    // --- 19. after_step hook ---
    let after_ctx = AfterStepContext {
        turn_id: turn_id.to_string(),
        step,
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_tokens: usage.cache_read_tokens,
        cache_creation_tokens: usage.cache_creation_tokens,
        stop_reason: format!("{:?}", effective),
    };
    let after_result = hooks.after_step(after_ctx).await?;
    let stop_turn_after_step = after_result.stop_turn;

    // --- 20. Cancel check ---
    cancel.check()?;

    // --- 21. Return StepResult ---
    let final_stop_reason = if stop_turn_after_step && effective == StepStopReason::ToolUse {
        StepStopReason::EndTurn
    } else {
        effective
    };

    Ok(StepResult {
        usage,
        stop_reason: final_stop_reason,
        media_degraded_resend_used: media_degraded_used,
        media_stripped_resend_used: media_stripped_used,
    })
}

/// Persist a loop event to the wire log as a `context.append_loop_event` op.
///
/// No-op when no recorder is installed; failures are swallowed by the
/// recorder (best-effort persistence never fails the engine loop).
pub(crate) async fn record_loop_event_wire(
    wire_recorder: Option<&dyn WireOpRecorder>,
    event: &LoopEvent,
) {
    if let Some(recorder) = wire_recorder {
        recorder
            .record_op(WireOp::ContextAppendLoopEvent {
                event: event.to_wire_value(),
            })
            .await;
    }
}
