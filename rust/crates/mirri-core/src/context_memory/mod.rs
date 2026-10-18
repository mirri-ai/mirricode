//! The **Context Memory** module — the canonical message history store with
//! projection pipeline.
//!
//! [`ContextMemory`] manages the canonical history and maintains the
//! invariant: **history must not contain an unresolved tool call exchange
//! except at the tail.**
//!
//! When the tail is unresolved, `pending_tool_result_ids` is exactly the set
//! of missing tool result ids for that tail exchange; [`append_message`]
//! keeps later messages in `deferred_messages` until those ids are resolved.
//!
//! Both `history` and `deferred_messages` are capped
//! ([`DEFAULT_MAX_HISTORY_MESSAGES`] / [`DEFAULT_MAX_DEFERRED_MESSAGES`]);
//! appends beyond a cap are rejected — logged via `tracing::warn!`, never
//! stored — so a runaway tool exchange cannot grow memory without bound.

pub mod compaction_input;
pub mod projector;
pub mod token_estimator;

use std::collections::{HashMap, HashSet};

use crate::context::{ContextMessage, ProjectOptions};
use crate::error::{EngineError, EngineResult};
use crate::message::{ContentPart, Message, Role, ToolCall, ToolCallFunction};
use crate::origin::PromptOrigin;
use crate::tool::ToolOutput;

pub use compaction_input::{
    ApplyOutcome, CompactionInput, CompactionUserMessageSelection, select_compaction_user_messages,
};
pub use projector::{project, report_projection_repairs};

/// The synthetic error output used when closing an interrupted tool call.
const TOOL_INTERRUPTED_ON_RESUME_OUTPUT: &str = "Tool execution was interrupted before its result was recorded. Do not assume the tool completed successfully.";

/// Default ceiling on the number of messages stored in the canonical history.
///
/// Large enough (10 000) that a normal session never reaches it, yet small
/// enough to bound memory: a runaway tool exchange that appends hundreds of
/// messages per turn cannot grow the history without limit before compaction
/// triggers. Appends beyond the ceiling are rejected and logged, never stored.
pub const DEFAULT_MAX_HISTORY_MESSAGES: usize = 10_000;

/// Default ceiling on the number of messages buffered in `deferred_messages`
/// while a tool exchange is open.
///
/// Buffering beyond the ceiling is rejected and logged, never stored.
pub const DEFAULT_MAX_DEFERRED_MESSAGES: usize = 1_000;

/// Events emitted by the agent loop that [`ContextMemory`] folds into the
/// canonical history.
///
/// Mirrors the TypeScript `LoopRecordedEvent` union. Each variant carries the
/// minimum fields the fold needs.
#[derive(Debug, Clone)]
pub enum LoopEvent {
    /// Open a new assistant step (streaming begins).
    ///
    /// `entry_seq` is the session-cumulative number (ADR-007) the step's
    /// partial assistant message already carries — set only by a replay of
    /// an event retained through compaction (the reducer stamps it onto
    /// the fold state); `None` on every live-recorded event.
    StepBegin {
        step_id: String,
        entry_seq: Option<u64>,
    },
    /// Append a text content part to the open step.
    ContentPart { step_id: String, text: String },
    /// Append a thinking/reasoning content part to the open step.
    ThinkPart { step_id: String, text: String },
    /// Append a tool call to the open step and mark its result as pending.
    ToolCall {
        step_id: String,
        tool_call_id: String,
        tool_name: String,
        args: serde_json::Value,
    },
    /// Push a tool result message and clear the pending id.
    ///
    /// `entry_seq` is as on [`LoopEvent::StepBegin`]: the number the
    /// result message already carries on a retained-event replay, `None`
    /// when recorded live.
    ToolResult {
        tool_call_id: String,
        output: ToolOutput,
        is_error: bool,
        note: Option<String>,
        entry_seq: Option<u64>,
    },
    /// Settle the open step and update token usage.
    ///
    /// The four usage buckets follow the dispatcher normalisation contract
    /// (see [`crate::llm::LlmUsage`]): `input_tokens` is the exclusive
    /// (non-cached) prompt token count. All four buckets — including the
    /// cache buckets — feed the context-pressure total via
    /// [`LoopEvent::context_tokens`].
    StepEnd {
        step_id: String,
        input_tokens: u64,
        output_tokens: u64,
        /// Prompt tokens served from cache (Anthropic
        /// `cache_read_input_tokens`, OpenAI/Google `cached_tokens`).
        cache_read_tokens: u64,
        /// Prompt tokens written to cache (Anthropic
        /// `cache_creation_input_tokens`).
        cache_creation_tokens: u64,
    },
}

impl LoopEvent {
    /// Serialise this event to the TS wire shape (`LoopRecordedEvent`), the
    /// `event` payload of a `context.append_loop_event` wire op.
    ///
    /// The shapes mirror the TypeScript kernel's loop events exactly (the
    /// step id travels as `uuid`), so a log written by this kernel replays
    /// identically through [`crate::session::replay::parse_loop_event`] and
    /// is interoperable with the TS kernel's own restore path.
    pub fn to_wire_value(&self) -> serde_json::Value {
        use serde_json::json;
        match self {
            LoopEvent::StepBegin { step_id, .. } => {
                json!({ "type": "step.begin", "uuid": step_id })
            }
            LoopEvent::ContentPart { step_id, text } => json!({
                "type": "content.part",
                "uuid": step_id,
                "part": { "type": "text", "text": text },
            }),
            LoopEvent::ThinkPart { step_id, text } => json!({
                "type": "think.part",
                "uuid": step_id,
                "part": { "type": "think", "text": text },
            }),
            LoopEvent::ToolCall {
                step_id,
                tool_call_id,
                tool_name,
                args,
            } => json!({
                "type": "tool.call",
                "uuid": step_id,
                "callId": tool_call_id,
                "name": tool_name,
                "input": args,
            }),
            LoopEvent::ToolResult {
                tool_call_id,
                output,
                is_error,
                note,
                ..
            } => {
                let output_parts = match output {
                    ToolOutput::Text(s) => json!([{ "type": "text", "text": s }]),
                    ToolOutput::Parts(parts) => serde_json::to_value(parts)
                        .expect("ContentPart serialization is infallible"),
                };
                json!({
                    "type": "tool.result",
                    "callId": tool_call_id,
                    "result": { "output": output_parts },
                    "isError": is_error,
                    "note": note,
                })
            }
            LoopEvent::StepEnd {
                step_id,
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_creation_tokens,
            } => json!({
                "type": "step.end",
                "uuid": step_id,
                "usage": {
                    "inputTokens": input_tokens,
                    "outputTokens": output_tokens,
                    "cacheReadTokens": cache_read_tokens,
                    "cacheCreationTokens": cache_creation_tokens,
                },
            }),
        }
    }

    /// The context-window pressure this event reports, in tokens.
    ///
    /// Only [`LoopEvent::StepEnd`] carries usage; every other event reports
    /// `0`. The value is the **four-bucket sum**:
    /// `input + output + cache_read + cache_creation` (saturating, so a
    /// garbage usage report cannot overflow the counter).
    ///
    /// Why the cache buckets belong in the pressure: cached prompt tokens
    /// still occupy the context window of the next request, and so do cache
    /// writes (Anthropic counts `cache_creation_input_tokens` in the real
    /// context total). Dropping them understates pressure and delays
    /// compaction on cache-heavy sessions.
    ///
    /// Why a plain sum is correct across providers: every mirri-kosong
    /// dispatcher normalises `input_tokens` to the **exclusive** (non-cached)
    /// value — see the normalisation contract on [`crate::llm::LlmUsage`].
    /// There is no provider that reports inclusive input by the time usage
    /// reaches the reducer, so the sum never double-counts.
    ///
    /// This is the single source for `ContextMemory::token_count` on both
    /// the live path and the wire-replay path, so restored sessions agree
    /// with live ones by construction.
    pub fn context_tokens(&self) -> u64 {
        match self {
            LoopEvent::StepEnd {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_creation_tokens,
                ..
            } => input_tokens
                .saturating_add(*output_tokens)
                .saturating_add(*cache_read_tokens)
                .saturating_add(*cache_creation_tokens),
            _ => 0,
        }
    }
}

/// The canonical message history store.
///
/// Owns the canonical `ContextMessage` history and maintains the invariant:
/// **history must not contain an unresolved tool call exchange except at the
/// tail.**
pub struct ContextMemory {
    /// The canonical store.
    history: Vec<ContextMessage>,
    /// Currently-open assistant steps (streaming), keyed by step_id → index
    /// into `history`.
    open_steps: HashMap<String, usize>,
    /// Tool call IDs awaiting results.
    pending_tool_result_ids: HashSet<String>,
    /// Messages buffered during an open exchange.
    deferred_messages: Vec<ContextMessage>,
    /// Ceiling on `history.len()`: appends beyond it are rejected (logged,
    /// not stored). Guards against runaway tool exchanges growing the memory
    /// without bound before compaction triggers.
    max_history_messages: usize,
    /// Ceiling on `deferred_messages.len()`: buffering beyond it is rejected
    /// (logged, not stored).
    max_deferred_messages: usize,
    /// Provider-reported token count.
    token_count: u64,
    /// How many messages `token_count` covers.
    token_count_covered_message_count: usize,
    /// Floor for compaction skip check.
    last_compacted_token_count: u64,
    /// The session-cumulative entry counter (ADR-007): the next number a
    /// materializing message takes. Compaction and undo remove messages but
    /// never roll this counter back (numbers of removed entries are not
    /// recycled); capacity-rejected appends never reach
    /// [`Self::push_history`] and so consume no number.
    next_entry_seq_counter: u64,
}

impl Default for ContextMemory {
    fn default() -> Self {
        Self::new()
    }
}

impl ContextMemory {
    /// Create a new, empty context memory with the default message-count
    /// ceilings ([`DEFAULT_MAX_HISTORY_MESSAGES`] /
    /// [`DEFAULT_MAX_DEFERRED_MESSAGES`]).
    pub fn new() -> Self {
        Self::with_limits(DEFAULT_MAX_HISTORY_MESSAGES, DEFAULT_MAX_DEFERRED_MESSAGES)
    }

    /// Create a new, empty context memory with explicit message-count
    /// ceilings.
    ///
    /// `max_history_messages` caps the canonical history; `max_deferred_messages`
    /// caps the buffer used while a tool exchange is open. Appends beyond
    /// either ceiling are rejected (logged via `tracing::warn!`, never
    /// stored). Existing messages are never evicted by these ceilings — they
    /// only gate future appends.
    pub fn with_limits(max_history_messages: usize, max_deferred_messages: usize) -> Self {
        Self {
            history: Vec::new(),
            open_steps: HashMap::new(),
            pending_tool_result_ids: HashSet::new(),
            deferred_messages: Vec::new(),
            max_history_messages,
            max_deferred_messages,
            token_count: 0,
            token_count_covered_message_count: 0,
            last_compacted_token_count: 0,
            next_entry_seq_counter: 0,
        }
    }

    /// Update the message-count ceilings.
    ///
    /// Only future appends are affected; messages already stored are never
    /// evicted. Useful to apply limits sourced from a
    /// [`crate::compaction::CompactionStrategy`] at turn start.
    pub fn set_limits(&mut self, max_history_messages: usize, max_deferred_messages: usize) {
        self.max_history_messages = max_history_messages;
        self.max_deferred_messages = max_deferred_messages;
    }

    // -----------------------------------------------------------------------
    // Read accessors
    // -----------------------------------------------------------------------

    /// Read-only access to the canonical history.
    pub fn history(&self) -> &[ContextMessage] {
        &self.history
    }

    /// Remove and return the newest history message.
    ///
    /// Used by swarm-mode exit to retract an enter reminder that is still the
    /// newest message (TS `popSwarmModeReminder` semantics). Does not touch
    /// the deferred buffer — callers only pop after exchanges are closed.
    pub fn pop_last_message(&mut self) -> Option<ContextMessage> {
        self.history.pop()
    }

    /// Projected messages with normal options (strips metadata).
    pub fn messages(&self) -> Vec<Message> {
        let options = ProjectOptions::normal();
        self.project(&options).unwrap_or_default()
    }

    /// Projected messages with strict options.
    pub fn strict_messages(&self) -> Vec<Message> {
        let options = ProjectOptions::strict();
        self.project(&options).unwrap_or_default()
    }

    /// The provider-reported token count.
    pub fn token_count(&self) -> u64 {
        self.token_count
    }

    /// `token_count + estimate(pending messages)`.
    ///
    /// "Pending messages" are the messages in history that come after the
    /// last provider-reported token count coverage point.
    pub fn token_count_with_pending(&self) -> u64 {
        let pending_messages = &self.history[self.token_count_covered_message_count..];
        self.token_count + token_estimator::estimate_messages_tokens(pending_messages)
    }

    /// Estimate of the size of the request the next step would actually send:
    /// the token estimate of the **normal projection**. Micro-compaction no
    /// longer runs in the normal projection (it mutated the request prefix
    /// and destroyed the provider's prompt cache), so this is the un-pruned
    /// measure today; it stays the seam where a future gated, cache-aligned
    /// trim would re-measure before a summarizer call.
    pub fn pruned_request_token_estimate(&self) -> u64 {
        let estimate_message = |m: &crate::message::Message| {
            let mut total = 0u64;
            for part in &m.content {
                match part {
                    ContentPart::Text { text } | ContentPart::Think { text } => {
                        total += token_estimator::estimate_tokens(text);
                    }
                    ContentPart::ImageUrl { .. }
                    | ContentPart::AudioUrl { .. }
                    | ContentPart::VideoUrl { .. }
                    | ContentPart::File { .. } => {
                        total += token_estimator::MEDIA_TOKEN_ESTIMATE;
                    }
                }
            }
            for tc in &m.tool_calls {
                total += token_estimator::estimate_tokens(&tc.function.arguments);
            }
            total
        };
        self.messages().iter().map(estimate_message).sum()
    }

    /// True if `pending_tool_result_ids` is non-empty (an open tool exchange
    /// exists).
    pub fn has_open_tool_exchange(&self) -> bool {
        !self.pending_tool_result_ids.is_empty()
    }

    // -----------------------------------------------------------------------
    // Append methods
    // -----------------------------------------------------------------------

    /// Append a user message with the given content and origin.
    ///
    /// If an open tool exchange exists, the message is deferred until the
    /// exchange closes.
    pub fn append_user_message(&mut self, content: Vec<ContentPart>, origin: PromptOrigin) {
        self.append_user_message_with_skills(content, origin, None);
    }

    /// Append a user message with content, origin, and an optional skills
    /// attachment (ADR-006). Only the admission-stamped BODY of an inline
    /// skill turn carries `Some(...)` — every other append path passes
    /// `None`, so the field never leaks onto wrap messages or mid-turn
    /// injections.
    pub fn append_user_message_with_skills(
        &mut self,
        content: Vec<ContentPart>,
        origin: PromptOrigin,
        with_skills: Option<Vec<crate::message::WithSkillsEntry>>,
    ) {
        if content.is_empty() {
            return;
        }
        let msg = ContextMessage {
            message: Message {
                role: Role::User,
                name: None,
                content,
                tool_calls: Vec::new(),
                tool_call_id: None,
                partial: false,
            },
            origin: Some(origin),
            is_error: false,
            note: None,
            with_skills,
            entry_seq: 0,
        };
        self.append_message(msg);
    }

    /// Append a system-reminder message (wrapped in `<system-reminder>` tag,
    /// origin Injection).
    pub fn append_system_reminder(&mut self, text: String) {
        let wrapped = format!("<system-reminder>\n{}\n</system-reminder>", text.trim());
        let msg = ContextMessage {
            message: Message {
                role: Role::User,
                name: None,
                content: vec![ContentPart::Text { text: wrapped }],
                tool_calls: Vec::new(),
                tool_call_id: None,
                partial: false,
            },
            origin: Some(PromptOrigin::Injection),
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        };
        self.append_message(msg);
    }

    /// General append — deferred if an open tool exchange exists.
    ///
    /// When the deferred buffer is at [`max_deferred_messages`](Self::max_deferred_messages),
    /// the message is rejected with a `tracing::warn!` instead of buffered.
    pub fn append_message(&mut self, msg: ContextMessage) {
        if self.has_open_tool_exchange() {
            if self.deferred_messages.len() >= self.max_deferred_messages {
                tracing::warn!(
                    deferred_len = self.deferred_messages.len(),
                    max_deferred_messages = self.max_deferred_messages,
                    "rejecting context message: deferred buffer at max_deferred_messages cap"
                );
                return;
            }
            self.deferred_messages.push(msg);
            return;
        }
        self.push_history(msg);
    }

    /// Consume a loop event and mutate state.
    ///
    /// See [`LoopEvent`] for the semantics of each variant.
    pub fn append_loop_event(&mut self, event: LoopEvent) {
        // Context pressure this event carries (the four-bucket StepEnd usage
        // sum; 0 for every other event kind). Computed before the match
        // moves the event so the formula lives in exactly one place —
        // [`LoopEvent::context_tokens`].
        let step_usage = event.context_tokens();
        match event {
            LoopEvent::StepBegin { step_id, entry_seq } => {
                // Close any pending tool results from an earlier step
                // (interrupted — the invariant guarantees this never happens
                // live, so this is a no-op outside replay).
                self.close_pending_tool_results();

                let msg = ContextMessage {
                    message: Message {
                        role: Role::Assistant,
                        name: None,
                        content: Vec::new(),
                        tool_calls: Vec::new(),
                        tool_call_id: None,
                        partial: true,
                    },
                    origin: None,
                    is_error: false,
                    note: None,
                    // ADR-007: a retained-event replay already knows the
                    // number this partial assistant carried; `push_history`
                    // preserves it. A live event carries none.
                    with_skills: None,
                    entry_seq: entry_seq.unwrap_or(0),
                };
                let index = self.history.len();
                // Only track the step as open when the partial assistant
                // message was actually stored — a rejected push must not leave
                // a stale open_steps entry pointing past the end of history
                // (its events would silently no-op, but the bookkeeping should
                // stay consistent).
                if self.push_history(msg) {
                    self.open_steps.insert(step_id, index);
                }
            }
            LoopEvent::ContentPart { step_id, text } => {
                if let Some(&index) = self.open_steps.get(&step_id)
                    && let Some(msg) = self.history.get_mut(index)
                {
                    msg.message.content.push(ContentPart::Text { text });
                }
            }
            LoopEvent::ThinkPart { step_id, text } => {
                if let Some(&index) = self.open_steps.get(&step_id)
                    && let Some(msg) = self.history.get_mut(index)
                {
                    msg.message.content.push(ContentPart::Think { text });
                }
            }
            LoopEvent::ToolCall {
                step_id,
                tool_call_id,
                tool_name,
                args,
            } => {
                if let Some(&index) = self.open_steps.get(&step_id)
                    && let Some(msg) = self.history.get_mut(index)
                {
                    msg.message.tool_calls.push(ToolCall {
                        id: tool_call_id.clone(),
                        call_type: "function".into(),
                        function: ToolCallFunction {
                            name: tool_name,
                            arguments: args.to_string(),
                        },
                    });
                }
                self.pending_tool_result_ids.insert(tool_call_id);
            }
            LoopEvent::ToolResult {
                tool_call_id,
                output,
                is_error,
                note,
                entry_seq,
            } => {
                // Drop a result for an id that is not awaiting one.
                if !self.pending_tool_result_ids.contains(&tool_call_id) {
                    return;
                }
                let content = match output {
                    ToolOutput::Text(s) => vec![ContentPart::Text { text: s }],
                    ToolOutput::Parts(parts) => parts,
                };
                let msg = ContextMessage {
                    message: Message {
                        role: Role::Tool,
                        name: None,
                        content,
                        tool_calls: Vec::new(),
                        tool_call_id: Some(tool_call_id.clone()),
                        partial: false,
                    },
                    origin: None,
                    is_error,
                    note,
                    // ADR-007: as on StepBegin — the number a
                    // retained-event replay already carries.
                    with_skills: None,
                    entry_seq: entry_seq.unwrap_or(0),
                };
                self.push_history(msg);
                self.pending_tool_result_ids.remove(&tool_call_id);
                self.flush_deferred_if_exchange_closed();
            }
            LoopEvent::StepEnd { step_id, .. } => {
                let open_step_index = self.open_steps.get(&step_id).copied();
                self.open_steps.remove(&step_id);

                // Settle the open assistant step: clear the `partial` flag so
                // the message survives projection (partial messages are
                // dropped). If the step ended up with no content and no tool
                // calls, drop it entirely — nothing sendable was recorded.
                if let Some(index) = open_step_index
                    && let Some(msg) = self.history.get_mut(index)
                {
                    if msg.message.tool_calls.is_empty()
                        && msg.message.content.iter().all(is_vacuous_content_part)
                    {
                        // Drop the empty partial assistant.
                        self.history.remove(index);
                        // No need to adjust open_steps indices — this step
                        // is already removed. But later steps pushed after
                        // this one would have higher indices. In practice,
                        // steps are sequential (one open at a time), so
                        // this is the tail and no index fixup is needed.
                    } else {
                        msg.message.partial = false;
                    }
                }

                // Update token count from usage. All four buckets count
                // (F7): cached prompt tokens and cache writes occupy the
                // context window of the next request, so omitting them
                // understates compaction pressure on cache-heavy sessions.
                // The formula lives in [`LoopEvent::context_tokens`].
                let total_usage = step_usage;
                let covered_count = if let Some(index) = open_step_index {
                    (index + 1).min(self.history.len())
                } else {
                    self.history.len()
                };

                if total_usage > 0 {
                    self.token_count = total_usage;
                } else if covered_count > self.token_count_covered_message_count {
                    // Provider reported zero usage — add an estimate for newly
                    // covered messages so the invariant stays intact. Clamp the
                    // range to avoid OOB when the open step was removed (empty
                    // assistant message dropped the history by one).
                    let previous_covered = self.token_count_covered_message_count;
                    let end = covered_count.min(self.history.len());
                    if end > previous_covered {
                        self.token_count += token_estimator::estimate_messages_tokens(
                            &self.history[previous_covered..end],
                        );
                    }
                }
                self.token_count_covered_message_count = covered_count;

                self.flush_deferred_if_exchange_closed();
            }
        }
    }

    /// Synthesize error results for all pending tool call IDs.
    ///
    /// Closes the open exchange by pushing a tool message with
    /// [`TOOL_INTERRUPTED_ON_RESUME_OUTPUT`] for each pending id. Called at
    /// every replayed step boundary and at end-of-resume.
    pub fn close_pending_tool_results(&mut self) {
        if self.pending_tool_result_ids.is_empty() {
            return;
        }
        let interrupted_ids: Vec<String> = self.pending_tool_result_ids.drain().collect();
        for tool_call_id in interrupted_ids {
            let msg = ContextMessage {
                message: Message {
                    role: Role::Tool,
                    name: None,
                    content: vec![ContentPart::Text {
                        text: TOOL_INTERRUPTED_ON_RESUME_OUTPUT.to_string(),
                    }],
                    tool_calls: Vec::new(),
                    tool_call_id: Some(tool_call_id),
                    partial: false,
                },
                origin: None,
                is_error: true,
                note: None,
                with_skills: None,
                entry_seq: 0,
            };
            self.push_history(msg);
        }
        self.flush_deferred_if_exchange_closed();
    }

    /// Settle every still-open step the way its recorded `StepEnd` would.
    ///
    /// Resume/replay ends here after a hard-quit mid-step: the journal
    /// carries the step's events but no `step.end`, so the step's assistant
    /// message is still flagged `partial` — a flag the projection drops
    /// wholesale, which orphans the synthesized interrupted results of
    /// [`Self::close_pending_tool_results`]. Settling first (a step that
    /// delivered content or tool calls loses the flag; a vacuous one is
    /// removed) keeps the assistant in the wire payload so the exchange
    /// stays paired.
    pub fn settle_open_steps(&mut self) {
        // Remove from the highest index down so a removal never shifts a
        // not-yet-processed step's index.
        let mut open_indices: Vec<usize> = self.open_steps.values().copied().collect();
        open_indices.sort_unstable_by(|a, b| b.cmp(a));
        for index in open_indices {
            if let Some(msg) = self.history.get_mut(index) {
                if msg.message.tool_calls.is_empty()
                    && msg.message.content.iter().all(is_vacuous_content_part)
                {
                    self.history.remove(index);
                } else {
                    msg.message.partial = false;
                }
            }
        }
        self.open_steps.clear();
    }

    /// Close out a turn that ended while it was interrupted (方式 B 取消闭
    /// 合): synthesize interrupted results for tool calls that never got
    /// results, settle any still-open assistant step, and flush deferred
    /// messages — all append-only, so the next request is a pure extension
    /// of what the model (and the user) already saw.
    ///
    /// Returns the loop events that were applied, in application order, so
    /// the caller can persist them to the wire log and a resume replays the
    /// identical closed state.
    pub fn close_interrupted(&mut self) -> Vec<LoopEvent> {
        let mut events = Vec::new();

        // 1. Close every pending tool call with the interrupted-error result.
        let pending_ids: Vec<String> = self.pending_tool_result_ids.iter().cloned().collect();
        for tool_call_id in pending_ids {
            let event = LoopEvent::ToolResult {
                tool_call_id,
                output: ToolOutput::Text(TOOL_INTERRUPTED_ON_RESUME_OUTPUT.to_string()),
                is_error: true,
                note: None,
                entry_seq: None,
            };
            self.append_loop_event(event.clone());
            events.push(event);
        }

        // 2. Settle every still-open step. `StepEnd` with zero usage seals a
        // step that carried delivered output (its content survives
        // projection) and drops a vacuous one (nothing sendable was
        // recorded), exactly as the normal settle path does.
        let open_step_ids: Vec<String> = self.open_steps.keys().cloned().collect();
        for step_id in open_step_ids {
            let event = LoopEvent::StepEnd {
                step_id,
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            };
            self.append_loop_event(event.clone());
            events.push(event);
        }

        events
    }

    /// Push deferred messages into history if the exchange is closed.
    ///
    /// The batch is truncated at the remaining history capacity: when the
    /// history cap is exhausted, the overflow is dropped with a single
    /// `tracing::warn!` (oldest deferred messages win — the tail is dropped).
    /// This avoids one warn per rejected message when flooding a full history.
    pub fn flush_deferred_if_exchange_closed(&mut self) {
        if self.has_open_tool_exchange() || self.deferred_messages.is_empty() {
            return;
        }
        let deferred = std::mem::take(&mut self.deferred_messages);
        let history_capacity = self.max_history_messages.saturating_sub(self.history.len());
        if history_capacity < deferred.len() {
            tracing::warn!(
                dropped_deferred_messages = deferred.len() - history_capacity,
                history_len = self.history.len(),
                max_history_messages = self.max_history_messages,
                "dropping deferred messages: history at max_history_messages cap"
            );
        }
        for msg in deferred.into_iter().take(history_capacity) {
            self.push_history(msg);
        }
    }

    // -----------------------------------------------------------------------
    // Projection
    // -----------------------------------------------------------------------

    /// Run the projection pipeline (pure, read-side — never mutates
    /// `history`).
    pub fn project(&self, options: &ProjectOptions) -> EngineResult<Vec<Message>> {
        projector::project(&self.history, options)
    }

    // -----------------------------------------------------------------------
    // Compaction
    // -----------------------------------------------------------------------

    /// Rewrite history to `[head essence users][elision marker?][summary][retention tail]`.
    ///
    /// The compacted prefix (`history[..retention_tail_start]`) contributes
    /// its oldest real user messages as the head essence (whole-message
    /// granularity, within
    /// [`COMPACT_USER_MESSAGE_HEAD_TOKENS`](compaction_input::COMPACT_USER_MESSAGE_HEAD_TOKENS));
    /// an elision `<system-reminder>` marker is inserted when real user
    /// input in the prefix was left out. A single `CompactionSummary`-origin
    /// user message carrying the context summary follows (ADR-002: the
    /// summary sits **before** the retention tail), and the retention tail
    /// `history[retention_tail_start..]` is kept verbatim (ADR-001).
    ///
    /// **Refusal:** `retention_tail_start: None` aborts the apply with a
    /// `tracing::warn!` — the caller should not have started the round.
    ///
    /// **Shrink check (变小校验, P8a-3):** the rewritten history must be
    /// smaller than the original — `estimate(new_history) <
    /// token_count_with_pending()` before the rewrite. A summary that comes
    /// back larger (or equal) is rejected with a `tracing::warn!` and the
    /// original history is kept; the outcome reports `applied: false` so
    /// callers can skip persisting an apply that never happened. The
    /// overflow-recovery path already has a bounded retry budget for the
    /// case where a rejected summary leaves the context over the limit.
    pub fn apply_compaction(&mut self, input: CompactionInput) -> ApplyOutcome {
        let Some(tail_start) = input.retention_tail_start else {
            tracing::warn!("compaction apply refused: no retention-tail cut");
            return ApplyOutcome::default();
        };
        let tail_start = tail_start.min(self.history.len());
        let tokens_before = self.token_count_with_pending();

        let prefix = &self.history[..tail_start];
        let (mut head, elided) = compaction_input::select_head_user_messages(
            prefix,
            compaction_input::COMPACT_USER_MESSAGE_HEAD_TOKENS,
        );
        let mut elision_message: Option<serde_json::Value> = None;
        if elided {
            let marker = compaction_input::elision_marker_message();
            elision_message = serde_json::to_value(&marker).ok();
            head.push(marker);
        }
        let kept_head_user_message_count = head.len() - usize::from(elided);

        let summary_msg = ContextMessage {
            message: Message::user(input.context_summary.clone()),
            origin: Some(PromptOrigin::CompactionSummary),
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        };

        let mut new_history = head;
        new_history.push(summary_msg);
        let retained_tail_count = self.history.len() - tail_start;
        new_history.extend(self.history[tail_start..].iter().cloned());

        let tokens_after: u64 = token_estimator::estimate_messages_tokens(&new_history);
        if tokens_after >= tokens_before {
            tracing::warn!(
                tokens_before,
                tokens_after,
                "rejecting compaction summary: the rewritten history is not smaller (shrink check failed); keeping the original history"
            );
            return ApplyOutcome::default();
        }

        // Session-cumulative entry numbering (ADR-007): the retained head
        // essence and tail keep their original numbers (cloned verbatim — no
        // renumbering), while the elision marker and the summary are the only
        // NEW materializations of an apply, taking the next counter values in
        // history order (marker first). Numbers are assigned only after the
        // shrink check: a rejected apply materializes nothing and must
        // consume no number, so the counter never gains a gap the fold
        // cannot re-derive. The marker sits at `summary_index - 1` (it was
        // pushed as the head's last element, immediately before the summary).
        let summary_index = new_history.len() - retained_tail_count - 1;
        if elided {
            new_history[summary_index - 1].entry_seq = self.next_entry_seq_counter;
            self.next_entry_seq_counter += 1;
        }
        new_history[summary_index].entry_seq = self.next_entry_seq_counter;
        self.next_entry_seq_counter += 1;

        self.history = new_history;
        self.open_steps.clear();
        self.pending_tool_result_ids.clear();
        self.deferred_messages.clear();
        self.token_count = tokens_after;
        self.token_count_covered_message_count = self.history.len();
        self.last_compacted_token_count = self.token_count_with_pending();
        ApplyOutcome {
            applied: true,
            retained_tail_count,
            kept_head_user_message_count,
            elision_message,
        }
    }

    // -----------------------------------------------------------------------
    // Undo
    // -----------------------------------------------------------------------

    /// Remove the last user-origin message and any trailing non-user messages
    /// after it.
    ///
    /// Walks backward from the tail, skipping injection-origin messages,
    /// stopping at a compaction boundary. Finds the last undo anchor (real
    /// user input), removes it and everything after it.
    pub fn undo(&mut self) -> EngineResult<()> {
        self.undo_count(1)
    }

    /// Undo `count` user-origin messages.
    ///
    /// Walks backward counting undo anchors (real user input). Removes each
    /// anchor and everything after it, back to the anchor's position.
    ///
    /// Also cleans up state that references the truncated history:
    /// - `open_steps`: remove entries whose history index is >= the cut point.
    /// - `pending_tool_result_ids`: clear all (they reference tool calls in the
    ///   truncated tail or the open step that preceded the cut).
    /// - `deferred_messages`: clear all (they were buffered for an exchange
    ///   that may have been in the truncated portion).
    pub fn undo_count(&mut self, count: usize) -> EngineResult<()> {
        if count == 0 {
            return Ok(());
        }
        let cut_index = compute_undo_cut(&self.history, count);

        if cut_index.is_none() {
            return Err(EngineError::RequestInvalid(
                "Nothing to undo: no user message to undo".to_string(),
            ));
        }

        let cut = cut_index.unwrap();

        // Clean up open_steps: remove any step whose history index is at or
        // past the cut point. The step is gone from history, so its entry is
        // stale.
        self.open_steps.retain(|_step_id, &mut index| index < cut);

        // Clean up pending_tool_result_ids: any pending tool result was for a
        // tool call in the truncated portion (or the open step just before it).
        // Keeping them would prevent new messages from being appended directly
        // (they'd be deferred).
        self.pending_tool_result_ids.clear();

        // Clean up deferred_messages: they were buffered while an open tool
        // exchange existed. After truncation the exchange they were waiting on
        // may no longer exist, so flushing them into history would resurrect
        // truncated content.
        self.deferred_messages.clear();

        self.history.truncate(cut);

        // Remove any leading ephemeral hook messages (hook result renderings
        // and additional context) that were prepended before the undone
        // user prompt. They have no associated user intent and would be
        // stale orphans after truncation.
        while self.history.first().is_some_and(|m| {
            matches!(
                m.origin,
                Some(PromptOrigin::HookAdditionalContext) | Some(PromptOrigin::HookResult)
            )
        }) {
            self.history.remove(0);
        }

        // Reset token coverage to the new history length.
        if self.token_count_covered_message_count > self.history.len() {
            self.token_count_covered_message_count = self.history.len();
            self.token_count = token_estimator::estimate_messages_tokens(&self.history);
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Reset
    // -----------------------------------------------------------------------

    /// Reset the memory to its empty state: drop the history, any open
    /// assistant steps, pending tool results, deferred messages, and all
    /// token counters.
    ///
    /// Equivalent to constructing a fresh [`ContextMemory`]; the instance
    /// stays fully usable afterwards.
    pub fn clear(&mut self) {
        self.history.clear();
        self.open_steps.clear();
        self.pending_tool_result_ids.clear();
        self.deferred_messages.clear();
        self.token_count = 0;
        self.token_count_covered_message_count = 0;
        self.last_compacted_token_count = 0;
        // A cleared memory is a fresh one (`ContextClear` on the wire clears
        // the fold state the same way), so the numbering restarts at 0 on
        // both sides and live == replayed keeps holding.
        self.next_entry_seq_counter = 0;
    }

    // -----------------------------------------------------------------------
    // Internal
    // -----------------------------------------------------------------------

    /// Push a message into the canonical history — the single
    /// materialization point every append path funnels through
    /// ([`Self::append_message`]'s direct branch, the message-materializing
    /// branches of [`Self::append_loop_event`], and the deferred flush).
    ///
    /// Session-cumulative entry numbering (ADR-007): a fresh message takes
    /// the next counter value; a message that already carries a non-zero
    /// number — one retained through compaction, arriving from a replay —
    /// keeps it and only raises the counter floor so later appends continue
    /// past it. A message stamped `0` (the very first entry ever, replayed
    /// at the head of the rebuilt timeline where the counter still sits at
    /// 0) is indistinguishable from a fresh one and takes the same 0 either
    /// way. A capacity-rejected push returns before any number is consumed.
    fn push_history(&mut self, mut msg: ContextMessage) -> bool {
        if self.history.len() >= self.max_history_messages {
            tracing::warn!(
                history_len = self.history.len(),
                max_history_messages = self.max_history_messages,
                "rejecting context message: history at max_history_messages cap"
            );
            return false;
        }
        if msg.entry_seq == 0 {
            msg.entry_seq = self.next_entry_seq_counter;
        }
        self.next_entry_seq_counter = self
            .next_entry_seq_counter
            .max(msg.entry_seq.saturating_add(1));
        self.history.push(msg);
        true
    }
}

/// Compute the cut index for undoing `count` user-origin messages.
///
/// Walks backward, skipping injection-origin messages, stopping at a
/// compaction boundary. Returns the index to truncate at, or `None` if not
/// enough undo anchors exist.
fn compute_undo_cut(history: &[ContextMessage], count: usize) -> Option<usize> {
    let mut remaining = count;
    let mut cut_index: Option<usize> = None;

    for i in (0..history.len()).rev() {
        if remaining == 0 {
            break;
        }
        let message = &history[i];

        // Skip ephemeral-origin messages that are not real user intent.
        if let Some(ref origin) = message.origin {
            if *origin == PromptOrigin::Injection {
                continue;
            }
            // Stop at compaction boundary.
            if *origin == PromptOrigin::CompactionSummary {
                break;
            }
        }

        if is_undo_anchor(message) {
            remaining -= 1;
            cut_index = Some(i);
        }
    }

    if remaining > 0 {
        return None;
    }
    cut_index
}

/// Whether a message is an undo anchor (real user input).
fn is_undo_anchor(message: &ContextMessage) -> bool {
    if message.message.role != Role::User {
        return false;
    }
    match &message.origin {
        None => true,
        Some(origin) => origin.is_real_user_input(),
    }
}

/// Whether a content part carries nothing the provider wire can represent.
///
/// Vacuous means an empty or whitespace-only text block. A `Think` block that
/// is empty is also vacuous (no provider signature in the Rust model). Media
/// parts always carry content.
fn is_vacuous_content_part(part: &ContentPart) -> bool {
    match part {
        ContentPart::Text { text } => text.trim().is_empty(),
        ContentPart::Think { text } => text.trim().is_empty(),
        ContentPart::ImageUrl { .. }
        | ContentPart::AudioUrl { .. }
        | ContentPart::VideoUrl { .. }
        | ContentPart::File { .. } => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system_reminder_wire::system_reminder_text;

    #[test]
    fn should_create_empty_memory_when_constructing_new_context_memory() {
        let mem = ContextMemory::new();
        assert!(mem.history().is_empty());
        assert_eq!(mem.token_count(), 0);
        assert!(!mem.has_open_tool_exchange());
    }

    #[test]
    fn should_reset_history_and_token_counters_when_clearing_context_memory() {
        let mut mem = ContextMemory::new();
        mem.append_user_message(
            vec![ContentPart::Text {
                text: "hello".to_string(),
            }],
            PromptOrigin::User,
        );
        // Settle a step with provider-reported usage so token_count is non-zero.
        mem.append_loop_event(LoopEvent::StepEnd {
            step_id: "step-1".to_string(),
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        });
        assert_eq!(mem.history().len(), 1);
        assert_eq!(mem.token_count(), 15);

        mem.clear();

        assert!(mem.history().is_empty());
        assert_eq!(mem.token_count(), 0);
        assert!(!mem.has_open_tool_exchange());

        // The memory is fully usable after a clear.
        mem.append_user_message(
            vec![ContentPart::Text {
                text: "after clear".to_string(),
            }],
            PromptOrigin::User,
        );
        assert_eq!(mem.history().len(), 1);
    }

    #[test]
    fn should_count_all_three_input_buckets_in_token_count_when_step_end_carries_cache_usage() {
        // Given a step end whose usage carries non-zero cache buckets on top
        // of the plain input/output pair (the shape every cache-enabled
        // provider reports: Anthropic cache_read/cache_creation, OpenAI
        // cached_tokens, Google cachedContentTokenCount)
        let mut mem = ContextMemory::new();
        mem.append_user_message(
            vec![ContentPart::Text {
                text: "hello".to_string(),
            }],
            PromptOrigin::User,
        );
        mem.append_loop_event(LoopEvent::StepBegin {
            step_id: "step-1".to_string(),
            entry_seq: None,
        });
        mem.append_loop_event(LoopEvent::StepEnd {
            step_id: "step-1".to_string(),
            input_tokens: 15,
            output_tokens: 5,
            cache_read_tokens: 200,
            cache_creation_tokens: 40,
        });

        // Then token_count is the four-bucket sum: 15 + 5 + 200 + 40 = 260.
        // Cached prompt tokens and cache writes occupy the context window of
        // the next request, so they must count toward compaction pressure
        // (F7; contract 5 three-bucket evidence).
        assert_eq!(mem.token_count(), 260);
        // And with full coverage there is no pending estimate on top.
        assert_eq!(mem.token_count_with_pending(), 260);
    }

    #[test]
    fn should_keep_zero_usage_estimate_path_intact_when_step_end_reports_no_usage() {
        // Given two settled steps whose StepEnd reports zero usage (the
        // close_interrupted / degraded-provider shape)
        let mut mem = ContextMemory::new();
        mem.append_user_message(
            vec![ContentPart::Text {
                text: "a".repeat(400), // ≈ 100 estimated tokens
            }],
            PromptOrigin::User,
        );
        mem.append_loop_event(LoopEvent::StepBegin {
            step_id: "step-1".to_string(),
            entry_seq: None,
        });
        mem.append_loop_event(LoopEvent::ContentPart {
            step_id: "step-1".to_string(),
            text: "b".repeat(400), // ≈ 100 estimated tokens
        });
        mem.append_loop_event(LoopEvent::StepEnd {
            step_id: "step-1".to_string(),
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        });
        // Then the first zero-usage settle estimates the newly covered range.
        assert_eq!(mem.token_count(), 200);

        // And a second zero-usage step adds only the newly covered messages
        // (the covered-pointer incremental estimate path stays intact — the
        // four-bucket formula must not disturb it).
        mem.append_user_message(
            vec![ContentPart::Text {
                text: "c".repeat(400), // ≈ 100 estimated tokens
            }],
            PromptOrigin::User,
        );
        mem.append_loop_event(LoopEvent::StepBegin {
            step_id: "step-2".to_string(),
            entry_seq: None,
        });
        mem.append_loop_event(LoopEvent::StepEnd {
            step_id: "step-2".to_string(),
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        });
        assert_eq!(mem.token_count(), 300);
        assert_eq!(
            mem.token_count(),
            token_estimator::estimate_messages_tokens(mem.history())
        );
    }

    /// Append `count` user messages with distinct text, for cap tests.
    fn append_user_texts(mem: &mut ContextMemory, count: usize) {
        for i in 0..count {
            mem.append_user_message(
                vec![ContentPart::Text {
                    text: format!("message-{i}"),
                }],
                PromptOrigin::User,
            );
        }
    }

    #[test]
    fn should_accept_history_appends_when_history_below_max_history_messages_cap() {
        const CAP: usize = 5;
        let mut mem = ContextMemory::with_limits(CAP, CAP);
        append_user_texts(&mut mem, CAP);
        assert_eq!(mem.history().len(), CAP);
    }

    #[test]
    fn should_reject_history_append_when_history_reaches_max_history_messages_cap() {
        const CAP: usize = 5;
        let mut mem = ContextMemory::with_limits(CAP, CAP);
        append_user_texts(&mut mem, CAP);
        // One past the cap: rejected (logged, not stored).
        append_user_texts(&mut mem, 1);
        assert_eq!(mem.history().len(), CAP);
    }

    #[test]
    fn should_reject_step_begin_when_history_reaches_max_history_messages_cap() {
        const CAP: usize = 5;
        let mut mem = ContextMemory::with_limits(CAP, CAP);
        append_user_texts(&mut mem, CAP);
        mem.append_loop_event(LoopEvent::StepBegin {
            step_id: "step-1".to_string(),
            entry_seq: None,
        });
        assert_eq!(
            mem.history().len(),
            CAP,
            "a rejected step begin must not grow the history"
        );
        assert!(
            mem.open_steps.is_empty(),
            "a rejected step begin must not be tracked as an open step"
        );
    }

    /// Given a wire log persisted by the pre-fix kernel whose tool call
    /// carries `input: null`, When the event is replayed through the wire
    /// parser into context memory and projected for the provider, Then the
    /// outgoing `function.arguments` must be the JSON object `{}` — never
    /// the literal string `"null"`, which strict OpenAI-compatible servers
    /// reject with "function.arguments must be a JSON object" on every
    /// replayed turn.
    #[test]
    fn should_sanitize_null_tool_arguments_when_replaying_persisted_wire_event() {
        // The exact wire shape a poisoned session carries: the accumulated
        // arguments degraded to null before the call was flushed.
        let wire = serde_json::json!({
            "type": "tool.call",
            "uuid": "step-1",
            "callId": "call_bad",
            "name": "Edit",
            "input": serde_json::Value::Null,
        });
        let event = crate::session::replay::parse_loop_event(&wire)
            .expect("tool.call with null input parses");

        let mut mem = ContextMemory::with_limits(100, 5);
        append_user_texts(&mut mem, 1);
        mem.append_loop_event(LoopEvent::StepBegin {
            step_id: "step-1".to_string(),
            entry_seq: None,
        });
        mem.append_loop_event(event);
        mem.append_loop_event(LoopEvent::ToolResult {
            tool_call_id: "call_bad".to_string(),
            output: ToolOutput::Text(
                "Invalid args for tool Edit: null is not of type \"object\"".to_string(),
            ),
            is_error: true,
            note: None,
            entry_seq: None,
        });
        mem.append_loop_event(LoopEvent::StepEnd {
            step_id: "step-1".to_string(),
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        });

        let messages = mem.messages();
        let assistant = messages
            .iter()
            .find(|m| m.role == Role::Assistant && !m.tool_calls.is_empty())
            .expect("the assistant tool call must survive projection");
        assert_eq!(
            assistant.tool_calls[0].function.arguments, "{}",
            "replayed null arguments must be sanitized to an empty JSON object"
        );
        assert_eq!(assistant.tool_calls[0].function.name, "Edit");
        assert_eq!(assistant.tool_calls[0].id, "call_bad");
    }

    #[test]
    fn should_accept_deferred_messages_when_deferred_below_max_deferred_messages_cap() {
        const DEFERRED_CAP: usize = 5;
        // History cap must comfortably hold the assistant step plus the
        // flushed deferred batch, so the flush below is not truncated.
        let mut mem = ContextMemory::with_limits(100, DEFERRED_CAP);
        // Open a tool exchange so user appends are buffered, not stored.
        mem.append_loop_event(LoopEvent::StepBegin {
            step_id: "step-1".to_string(),
            entry_seq: None,
        });
        mem.append_loop_event(LoopEvent::ToolCall {
            step_id: "step-1".to_string(),
            tool_call_id: "call-1".to_string(),
            tool_name: "tool-1".to_string(),
            args: serde_json::json!({}),
        });
        append_user_texts(&mut mem, DEFERRED_CAP);
        assert_eq!(mem.deferred_messages.len(), DEFERRED_CAP);
        assert_eq!(
            mem.history().len(),
            1,
            "the open assistant step lives in history; only user appends are deferred"
        );

        // Closing the exchange pushes the tool result, then flushes the whole
        // deferred batch into history.
        mem.append_loop_event(LoopEvent::ToolResult {
            tool_call_id: "call-1".to_string(),
            output: ToolOutput::Text("ok".to_string()),
            is_error: false,
            note: None,
            entry_seq: None,
        });
        assert_eq!(mem.history().len(), 2 + DEFERRED_CAP); // assistant step + tool result + flushed batch
        assert!(mem.deferred_messages.is_empty());
        assert!(!mem.has_open_tool_exchange());
    }

    #[test]
    fn should_reject_deferred_message_when_deferred_reaches_max_deferred_messages_cap() {
        const DEFERRED_CAP: usize = 5;
        let mut mem = ContextMemory::with_limits(100, DEFERRED_CAP);
        mem.append_loop_event(LoopEvent::StepBegin {
            step_id: "step-1".to_string(),
            entry_seq: None,
        });
        mem.append_loop_event(LoopEvent::ToolCall {
            step_id: "step-1".to_string(),
            tool_call_id: "call-1".to_string(),
            tool_name: "tool-1".to_string(),
            args: serde_json::json!({}),
        });
        append_user_texts(&mut mem, DEFERRED_CAP);
        // One past the cap: rejected (logged, not buffered).
        append_user_texts(&mut mem, 1);
        assert_eq!(mem.deferred_messages.len(), DEFERRED_CAP);
    }

    #[test]
    fn should_drop_deferred_messages_when_history_cap_is_exhausted_at_flush() {
        const HISTORY_CAP: usize = 5;
        let mut mem = ContextMemory::with_limits(HISTORY_CAP, 100);
        // Fill the history to its cap.
        append_user_texts(&mut mem, HISTORY_CAP);
        // Open a tool exchange: the step begin is rejected (history full) but
        // the tool call still registers its pending result id.
        mem.append_loop_event(LoopEvent::StepBegin {
            step_id: "step-1".to_string(),
            entry_seq: None,
        });
        mem.append_loop_event(LoopEvent::ToolCall {
            step_id: "step-1".to_string(),
            tool_call_id: "call-1".to_string(),
            tool_name: "tool-1".to_string(),
            args: serde_json::json!({}),
        });
        assert!(mem.has_open_tool_exchange());
        // A user message during the open exchange is buffered as deferred.
        append_user_texts(&mut mem, 1);
        assert_eq!(mem.deferred_messages.len(), 1);

        // Resolving the tool result closes the exchange; the deferred message
        // has no remaining history capacity and is dropped.
        mem.append_loop_event(LoopEvent::ToolResult {
            tool_call_id: "call-1".to_string(),
            output: ToolOutput::Text("ok".to_string()),
            is_error: false,
            note: None,
            entry_seq: None,
        });
        assert_eq!(mem.history().len(), HISTORY_CAP);
        assert!(
            mem.deferred_messages.is_empty(),
            "unflushable deferred messages must be dropped"
        );
        assert!(
            !mem.has_open_tool_exchange(),
            "the exchange must close even when its result message is rejected"
        );
    }

    #[test]
    fn should_apply_updated_limits_when_set_limits_called() {
        const SMALL_CAP: usize = 3;
        let mut mem = ContextMemory::with_limits(100, 100);
        mem.set_limits(SMALL_CAP, SMALL_CAP);
        append_user_texts(&mut mem, SMALL_CAP);
        append_user_texts(&mut mem, 1); // rejected per new history cap
        assert_eq!(mem.history().len(), SMALL_CAP);
    }

    #[test]
    fn should_pop_and_return_last_message_when_history_nonempty() {
        let mut mem = ContextMemory::new();
        mem.append_system_reminder("first".to_string());
        mem.append_system_reminder("second".to_string());

        let popped = mem.pop_last_message();

        assert_eq!(
            popped.expect("last message exists").message.text_content(),
            system_reminder_text("second")
        );
        assert_eq!(mem.history().len(), 1);
    }

    #[test]
    fn should_return_none_when_popping_empty_history() {
        let mut mem = ContextMemory::new();
        assert!(mem.pop_last_message().is_none());
    }
}

#[test]
fn should_close_pending_exchange_and_settle_open_step_when_closing_interrupted_turn() {
    // Given an interrupted turn: an open assistant step with delivered
    // text, a tool call awaiting its result, and a deferred message
    let mut mem = ContextMemory::new();
    mem.append_loop_event(LoopEvent::StepBegin {
        step_id: "step-1".to_string(),
        entry_seq: None,
    });
    mem.append_loop_event(LoopEvent::ContentPart {
        step_id: "step-1".to_string(),
        text: "Partial output".to_string(),
    });
    mem.append_loop_event(LoopEvent::ToolCall {
        step_id: "step-1".to_string(),
        tool_call_id: "call-1".to_string(),
        tool_name: "Echo".to_string(),
        args: serde_json::json!({}),
    });
    mem.append_user_message(
        vec![ContentPart::Text {
            text: "stranded input".to_string(),
        }],
        PromptOrigin::User,
    );
    assert!(mem.has_open_tool_exchange());
    assert_eq!(mem.deferred_messages.len(), 1);

    // When the turn is closed as interrupted
    let events = mem.close_interrupted();

    // Then the interrupted tool result and the settling step end are
    // returned as replayable loop events, in that order.
    assert_eq!(events.len(), 2, "tool.result + step.end expected");
    match &events[0] {
        LoopEvent::ToolResult {
            tool_call_id,
            is_error,
            ..
        } => {
            assert_eq!(tool_call_id, "call-1");
            assert!(*is_error, "the interrupted result must be an error");
        }
        other => panic!("expected ToolResult, got {other:?}"),
    }
    match &events[1] {
        LoopEvent::StepEnd { step_id, .. } => assert_eq!(step_id, "step-1"),
        other => panic!("expected StepEnd, got {other:?}"),
    }

    // And the memory state is fully closed: no pending ids, no deferred
    // stranding, the assistant step sealed (survives projection), and the
    // stranded user input flushed into history.
    assert!(!mem.has_open_tool_exchange());
    assert!(mem.deferred_messages.is_empty());
    let messages = mem.messages();
    let assistant = messages
        .iter()
        .find(|m| m.role == Role::Assistant)
        .expect("the settled assistant must survive projection");
    assert!(assistant.text_content().contains("Partial output"));
    let tool = messages
        .iter()
        .find(|m| m.role == Role::Tool)
        .expect("the interrupted tool result must be in history");
    assert_eq!(tool.tool_call_id.as_deref(), Some("call-1"));
    assert!(
        messages
            .iter()
            .any(|m| m.text_content().contains("stranded input")),
        "the deferred user message must be flushed, not stranded"
    );
}

#[test]
fn should_return_empty_events_when_closing_interrupted_turn_with_nothing_open() {
    // Given a clean memory (turn aborted between steps)
    let mut mem = ContextMemory::new();
    mem.append_user_message(
        vec![ContentPart::Text {
            text: "hello".to_string(),
        }],
        PromptOrigin::User,
    );

    // When the interrupted turn is closed
    let events = mem.close_interrupted();

    // Then nothing was closed and no events are recorded (no wire churn).
    assert!(events.is_empty());
}

#[test]
fn should_reject_compaction_summary_when_rewritten_history_is_not_smaller() {
    // Given a small history and a summary that would make it BIGGER
    let mut mem = ContextMemory::new();
    mem.append_user_message(
        vec![ContentPart::Text {
            text: "hello".to_string(),
        }],
        PromptOrigin::User,
    );
    mem.append_user_message(
        vec![ContentPart::Text {
            text: "world".to_string(),
        }],
        PromptOrigin::User,
    );
    let history_before = mem.history().to_vec();
    let tokens_before = mem.token_count_with_pending();

    // When a compaction whose summary exceeds the original size applies
    // (retention tail = the newest user message)
    let outcome = mem.apply_compaction(CompactionInput {
        summary: "huge".to_string(),
        context_summary: "s".repeat(400),
        compacted_count: 2,
        tokens_before,
        dropped_count: 0,
        retention_tail_start: Some(1),
    });

    // Then it is rejected: the original history is kept verbatim.
    assert!(!outcome.applied, "a non-shrinking summary must be rejected");
    assert!(
        mem.history().len() == history_before.len(),
        "the original history must be preserved on rejection"
    );
    for (kept, before) in mem.history().iter().zip(history_before.iter()) {
        assert_eq!(kept.message.text_content(), before.message.text_content());
    }
    assert_eq!(mem.token_count_with_pending(), tokens_before);
}

#[test]
fn should_apply_compaction_when_rewritten_history_is_smaller() {
    // Given a history whose user messages are large
    let mut mem = ContextMemory::new();
    for i in 0..4 {
        mem.append_user_message(
            vec![ContentPart::Text {
                text: format!("message {i}: {}", "x".repeat(30_000)),
            }],
            PromptOrigin::User,
        );
    }
    let tokens_before = mem.token_count_with_pending();

    // When a compaction with a small summary applies (retention tail =
    // the newest big user message)
    let outcome = mem.apply_compaction(CompactionInput {
        summary: "small summary".to_string(),
        context_summary: "compact: the conversation was about caching".to_string(),
        compacted_count: 4,
        tokens_before,
        dropped_count: 0,
        retention_tail_start: Some(3),
    });

    // Then it is applied and the memory really did shrink.
    assert!(outcome.applied);
    assert!(
        mem.token_count_with_pending() < tokens_before,
        "the applied compaction must measurably shrink the memory"
    );
    assert!(
        mem.history()
            .iter()
            .any(|m| matches!(m.origin, Some(PromptOrigin::CompactionSummary))),
        "the rewritten history must carry the compaction summary message"
    );
}
