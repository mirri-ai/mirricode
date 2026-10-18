//! The loop engine — the core agent turn/step execution loop.
//!
//! This module implements the agent loop that:
//! 1. Runs LLM steps (one provider call per step).
//! 2. Executes tool calls when the model requests them.
//! 3. Continues looping until a terminal stop reason or max steps.
//! 4. Handles retries, media fallbacks, cancellation, and host hooks.
//!
//! ## Architecture
//!
//! - [`run_turn`] — the outer loop. Iterates steps until a terminal stop
//!   reason or max steps. Calls [`execute_loop_step`] for each step.
//! - [`execute_loop_step`] — a single provider step. Calls `chat_with_retry`,
//!   dispatches events to `ContextMemory`, runs tool batches, and applies
//!   host hooks.
//! - [`chat_with_retry`] — exponential backoff retry around `Llm::chat()`.
//!
//! ## Event Flow
//!
//! Block-level events (`StepBegin`, `ContentPart`, `ThinkPart`, `ToolCall`,
//! `ToolResult`, `StepEnd`) are recorded directly into `ContextMemory` via
//! [`LoopEvent::append_loop_event`]. Live-only events (deltas, retries,
//! turn interruption) are forwarded via [`LiveEventCallback`].

pub mod events;
pub mod retry;
pub mod run_turn;
pub mod shared;
pub mod step_retry;
pub mod tool_call_batch;
pub mod turn_step;

pub use events::{LiveEvent, LiveEventCallback};
pub use retry::{ChatResult, chat_with_retry};
pub(crate) use run_turn::resolve_step_llm;
pub use run_turn::run_turn;
pub use step_retry::{
    StepRetryConfig, StepRetryDecision, StepRetryService, StepRetrySnapshot, StepRetryState,
    is_retryable_error,
};
pub use tool_call_batch::{
    derive_step_stop_reason, execute_tool_calls, record_unexecuted_tool_calls,
};
pub use turn_step::{TurnStepDeps, execute_loop_step};

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use mirri_proto::WireOp;

use crate::context::ProjectOptions;
use crate::context_memory::ContextMemory;
use crate::error::EngineResult;
use crate::hooks::HookFuture;
use crate::llm::LlmUsage;
use crate::message::Message;
use serde::{Deserialize, Serialize};

/// Stop reason at the step level (from the LLM response).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepStopReason {
    /// Model finished naturally (end of turn).
    EndTurn,
    /// Model hit max output tokens.
    MaxTokens,
    /// Model wants to call tools (loop continues).
    ToolUse,
    /// Content was filtered.
    Filtered,
    /// Response was paused (some providers).
    Paused,
    /// Unknown stop reason.
    Unknown,
}

/// Stop reason at the turn level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnStopReason {
    /// Model finished naturally.
    EndTurn,
    /// Model hit max output tokens.
    MaxTokens,
    /// Content was filtered.
    Filtered,
    /// Response was paused.
    Paused,
    /// Unknown stop reason.
    Unknown,
    /// Turn was aborted (cancellation).
    Aborted,
    /// Turn was terminated by the stall watchdog (ADR-004): the turn went
    /// silent past the inactivity window and was cooperatively cancelled.
    /// User-initiated cancellations stay `Aborted`.
    Stalled,
    /// Turn was blocked by the `before_step` hook.
    BlockedByHook,
}

/// Result of running a single step.
#[derive(Debug, Clone)]
pub struct StepResult {
    /// Token usage from this step's LLM call.
    pub usage: LlmUsage,
    /// The effective stop reason (after tool batch and hooks).
    pub stop_reason: StepStopReason,
    /// Whether the media-degraded resend was used (sticky for the turn).
    pub media_degraded_resend_used: bool,
    /// Whether the media-stripped resend was used (sticky for the turn).
    pub media_stripped_resend_used: bool,
}

/// The turn-abort guard predicate (session-consistency batch 2, ADR-001,
/// invariant 2): polled by the engine loop at two points — the top of each
/// step iteration and immediately before a tool batch dispatches. A `false`
/// return aborts the turn through the existing cancellation path
/// (`EngineError::Aborted` → the 方式 B append-only closure), so the
/// already-recorded partial work stays durable and the turn settles with
/// `TurnStopReason::Aborted`.
///
/// The guard is deliberately synchronous and cheap (the embedded layer
/// installs a single-`stat` journal probe): it runs on the engine loop's
/// critical path twice per step. `Arc<dyn Fn>` mirrors the `LiveEventCallback`
/// convention (cloneable, no lifetime plumbing).
pub type TurnAbortGuard = Arc<dyn Fn() -> bool + Send + Sync>;

/// Result of running a turn.
#[derive(Debug, Clone)]
pub struct TurnResult {
    /// The terminal stop reason.
    pub stop_reason: TurnStopReason,
    /// Number of steps executed.
    pub steps: u32,
    /// Aggregated token usage across all steps.
    pub usage: LlmUsage,
}

/// Configuration for [`run_turn`].
#[derive(Clone)]
pub struct RunTurnConfig {
    /// Unique turn identifier.
    pub turn_id: String,
    /// Maximum number of steps (None or 0 = unlimited).
    pub max_steps: Option<u32>,
    /// Maximum retry attempts for `chat_with_retry`.
    pub max_retry_attempts: u32,
    /// Optional step-level retry service.
    ///
    /// When set, a failed step is re-enqueued (re-run with the same step
    /// number) when [`StepRetryService::should_retry`] grants a retry;
    /// success resets the step/driver counters. `None` disables step-level
    /// retry (request-level `chat_with_retry` still applies).
    pub step_retry: Option<Arc<StepRetryService>>,
    /// Optional turn-abort guard (session-consistency invariant 2): polled
    /// before each step and before each tool batch. `None` (the default —
    /// every existing caller) keeps the loop guard-free.
    pub turn_abort_guard: Option<TurnAbortGuard>,
    /// The fork chain's cache root id (the provider prompt-cache routing
    /// key). Threaded from the turn flow into every step's
    /// [`LlmChatParams`](crate::llm::LlmChatParams) so the provider request
    /// carries the session's cache lineage (TS parity: `promptCacheKey`).
    /// `None` (the default) omits the provider's cache-routing field.
    pub cache_root_id: Option<String>,
    /// The turn's ADR-006 wait registry — the step loop's await points
    /// register into it (`WaitingProvider` around the model stream,
    /// `WaitingTool` around the tool batch). `None` (the default —
    /// standalone `run_turn` callers, tests) registers nothing.
    pub wait_registry: Option<std::sync::Arc<crate::turn_flow::WaitRegistry>>,
}

impl RunTurnConfig {
    /// Create a new config with the given turn ID and defaults.
    pub fn new(turn_id: impl Into<String>) -> Self {
        Self {
            turn_id: turn_id.into(),
            max_steps: None,
            max_retry_attempts: 10,
            step_retry: None,
            turn_abort_guard: None,
            cache_root_id: None,
            wait_registry: None,
        }
    }
}

/// Callbacks for building messages with different projections.
///
/// Each closure returns projected `Vec<WireMessage>` ready for the LLM.
/// The loop uses these to implement media fallback: if the normal projection
/// fails with a 413 or image error, the loop retries with an alternate
/// projection.
pub struct MessageBuilders {
    /// Normal projection (full media, normal options).
    pub normal: Box<dyn Fn() -> EngineResult<Vec<Message>> + Send + Sync>,
    /// Media-degraded projection (lower-resolution images).
    pub media_degraded: Option<Box<dyn Fn() -> EngineResult<Vec<Message>> + Send + Sync>>,
    /// Media-stripped projection (no images at all).
    pub media_stripped: Option<Box<dyn Fn() -> EngineResult<Vec<Message>> + Send + Sync>>,
    /// Strict projection (wire-strict, no metadata).
    pub strict: Option<Box<dyn Fn() -> EngineResult<Vec<Message>> + Send + Sync>>,
    /// The sticky format-rejected flag shared between the media-stripped
    /// projection and the media fallback chain (multimodal spec US6.2):
    /// set by `try_media_fallback` when the provider rejects an image
    /// FORMAT, read by the `media_stripped` builder to pick the placeholder
    /// cause (format-rejected conversion guidance vs the size-limit copy).
    /// Once set, every stripped replay for the rest of the turn keeps the
    /// format-rejected form.
    pub format_rejected_flag: Arc<AtomicBool>,
}

impl MessageBuilders {
    /// Build the production message builders over a shared context memory.
    ///
    /// Each builder locks memory for a short synchronous critical section to
    /// produce the projected messages; the locks are never held across
    /// `.await` points.
    ///
    /// The `media_stripped` builder reads the shared `format_rejected_flag`
    /// at each build to select the placeholder cause — see the field docs.
    pub fn from_shared_memory(memory: Arc<Mutex<ContextMemory>>) -> Self {
        let format_rejected_flag = Arc::new(AtomicBool::new(false));
        let flag_for_stripped = Arc::clone(&format_rejected_flag);

        let mem_for_normal = memory.clone();
        let mem_for_degraded = memory.clone();
        let mem_for_stripped = memory.clone();
        let mem_for_strict = memory;

        Self {
            normal: Box::new(move || {
                let mem = mem_for_normal.lock().unwrap_or_else(|e| e.into_inner());
                Ok(mem.messages())
            }),
            media_degraded: Some(Box::new(move || {
                // Media degraded: project with media parts replaced by text
                // placeholders. This is the first-level fallback when the
                // provider rejects the normal projection due to payload
                // size (413).
                let mem = mem_for_degraded.lock().unwrap_or_else(|e| e.into_inner());
                let options = ProjectOptions::degraded();
                Ok(mem.project(&options).unwrap_or_default())
            })),
            media_stripped: Some(Box::new(move || {
                // Media stripped: all media parts replaced by text markers,
                // plus strict repair passes. The placeholder cause follows
                // the sticky flag: format-rejected (US6.2) once the
                // fallback chain saw a format error, size-limit (US6.1)
                // otherwise.
                let mem = mem_for_stripped.lock().unwrap_or_else(|e| e.into_inner());
                let options = if flag_for_stripped.load(Ordering::SeqCst) {
                    ProjectOptions::stripped_format_rejected()
                } else {
                    ProjectOptions::stripped()
                };
                Ok(mem.project(&options).unwrap_or_default())
            })),
            strict: Some(Box::new(move || {
                let mem = mem_for_strict.lock().unwrap_or_else(|e| e.into_inner());
                let options = ProjectOptions::strict();
                Ok(mem.project(&options).unwrap_or_default())
            })),
            format_rejected_flag,
        }
    }
}

/// Callback for recording step usage (host callback).
///
/// Called after each LLM response with the token usage, the response's
/// natural stop reason, and the 1-based step number. Returns:
/// - `Ok(None)` — continue normally.
/// - `Ok(Some(true))` — stop the turn after this step (budget exhausted,
///   no grace granted or already spent).
/// - `Ok(Some(false))` — the usage was accounted but the step is released:
///   either the goal's budget still has room, or (T9) the host granted the
///   one-per-turn hard-budget grace step so the model can write its final
///   message. The engine does not stop the turn in either case.
/// - `Err(_)` — propagate the error.
pub type RecordStepUsageFn = Box<
    dyn Fn(LlmUsage, StepStopReason, usize) -> HookFuture<EngineResult<Option<bool>>> + Send + Sync,
>;

/// A sink for wire ops produced during engine turns (e.g. the session's
/// `wire.jsonl`).
///
/// The loop engine is stateless and host-agnostic: it never constructs or
/// owns a wire log itself. Hosts that want loop activity persisted (loop
/// events, step usage) implement this trait and install it on
/// [`TurnFlow`](crate::turn_flow::TurnFlow) via its `set_wire_recorder`
/// setter. A missing recorder disables recording — unit tests and replay
/// never write.
///
/// Recording is **best-effort**: implementations must log and swallow
/// append failures rather than propagate them — the engine loop must not
/// fail because the wire log is unwritable.
pub trait WireOpRecorder: Send + Sync {
    /// Record a single wire op, awaiting its durability (fsync).
    fn record_op<'a>(&'a self, op: WireOp) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;
}
