//! Live-only events — forwarded to the UI/RPC layer but never recorded in
//! [`ContextMemory`](crate::context_memory::ContextMemory).
//!
//! These events represent incremental, fire-and-forget notifications that the
//! host uses for streaming UI updates (token-by-token text, thinking fragments,
//! tool-call argument chunks) and loop-level lifecycle notifications (retry
//! attempts, turn interruption).
//!
//! Block-level events (`TextPart`, `ThinkPart`, `ToolCall`, `ToolResult`,
//! `StepBegin`, `StepEnd`) are recorded directly into `ContextMemory` via
//! [`LoopEvent`](crate::context_memory::LoopEvent) and are NOT modeled here.

use std::sync::Arc;

use crate::tool::ToolUpdate;

/// Live-only events (never recorded in `ContextMemory`, forwarded to UI/host).
#[derive(Debug, Clone)]
pub enum LiveEvent {
    /// Incremental text delta (chunk-level, fire-and-forget).
    TextDelta { text: String },
    /// Incremental thinking/reasoning delta (chunk-level, fire-and-forget).
    ThinkDelta { text: String },
    /// Incremental tool-call argument fragment (index-addressed). `call_id`/
    /// `tool_name` carry the fragment's identity when known — see
    /// `LlmEvent::ToolCallPart`.
    ToolCallPart {
        index: u64,
        arguments_part: String,
        call_id: Option<String>,
        tool_name: Option<String>,
    },
    /// The turn was interrupted (abort, max steps, etc.).
    TurnInterrupted {
        reason: String,
        attempted_steps: u32,
        active_step: Option<u32>,
        message: Option<String>,
    },
    /// A step is retrying after a provider error.
    StepRetrying {
        failed_attempt: u32,
        next_attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
    },
    /// A tool call started executing (the engine is about to run the tool).
    /// `args` is the full parsed argument object (the provider's tool-call
    /// arguments), so a live consumer can show what the tool was invoked
    /// with before the result arrives — the same field the TS protocol's
    /// `tool.call.started` carries.
    ToolCallStarted {
        tool_name: String,
        tool_call_id: String,
        args: serde_json::Value,
    },
    /// A tool call completed (result is available, success or error).
    /// `tool_name` rides along so a consumer that missed the started event
    /// (or joined mid-stream) can still attribute the result to its tool —
    /// the wire `tool.result` name field. `output` carries the result's
    /// output (text or structured parts — the wire `tool.result` output
    /// field, the same shape the TS protocol carries, so a live consumer
    /// renders the result without waiting for a snapshot reload).
    ToolCallCompleted {
        tool_call_id: String,
        tool_name: String,
        is_error: bool,
        output: crate::tool::ToolOutput,
    },
    /// An incremental update from a running tool call (streamed stdout,
    /// progress percent, status text, or a custom payload).
    ToolProgress {
        tool_call_id: String,
        update: ToolUpdate,
    },
    /// An LLM request is in flight but no first event has arrived yet —
    /// a volatile heartbeat for the stall-watchdog feed (a slow
    /// provider's first token must not read as a dead engine). Two
    /// consumers: the main turn's live-callback wrapper resets the
    /// watchdog directly, and `ActivityTracker::observe_live` stamps
    /// the activity clock so the parent of a foreground subagent keeps
    /// its forwarding feed alive. Never relayed to the event bus, never
    /// recorded to the wire log. Fires ONLY while the stream's first
    /// event is outstanding — mid-stream silence stays watchdog
    /// territory.
    LlmPending { elapsed_ms: u64 },
}

/// Callback for live events. The host wires this to the UI/RPC streaming layer.
///
/// `Arc` (not `Box`) so the callback can be cloned into per-tool progress
/// sinks that outlive the borrowing turn step.
pub type LiveEventCallback = Arc<dyn Fn(LiveEvent) + Send + Sync>;
