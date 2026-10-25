//! The compaction round — a single LLM summarization call with a 4-level
//! fallback chain.
//!
//! [`compaction_round`] is the entry point. It:
//! 1. Snapshots the original history (for prefix-race check) and computes
//!    the retention-tail cut — the summarizer only sees the old segment.
//! 2. Strips dynamic tool context (system-reminder injections).
//! 3. Builds the compaction instruction prompt.
//! 4. Calls the LLM with a 4-level fallback chain.
//! 5. Prefix-race check (cancel if history changed).
//! 6. Post-processes the summary (append TODO list if present).
//! 7. Builds the context summary text (prefix + summary).
//! 8. Re-cuts against the live history and applies compaction to memory.
//! 9. Returns a [`CompactionResult`].
//!
//! ## 4-Level Fallback Chain
//!
//! - **Level 1 — Media strip**: If the error is a 413/too-large or image-format
//!   error AND media hasn't been stripped yet, replace all media parts with
//!   text markers (`[image]`, `[audio]`, `[video]`).
//! - **Level 2 — Overflow shrink**: If the error is context overflow or 413,
//!   shrink the history to keep only the most recent messages within a
//!   token-budget ratio (0.7 → 0.5 → 0.35).
//! - **Level 3 — Drop oldest**: If the response was truncated or empty, drop
//!   the oldest message and leading tool results.
//! - **Level 4 — Retry with backoff**: If the error is retryable
//!   (connection/timeout/overloaded), retry with exponential backoff.

use crate::cancel::CancelToken;
use crate::compaction::retention;
use crate::context::ContextMessage;
use crate::context::ProjectOptions;
use crate::context_memory::token_estimator::{estimate_message_tokens, estimate_messages_tokens};
use crate::context_memory::{ApplyOutcome, CompactionInput, ContextMemory, project};
use crate::error::{EngineError, EngineResult};
use crate::llm::{Llm, LlmChatParams, LlmChatResponse, ProviderErrorInfo, StopReason};
use crate::loop_engine::shared::{
    CollectedStream, backoff_delay, cancelable_sleep, collect_stream_events, random_jitter,
};
use crate::message::{ContentPart, Message, Role};
use crate::origin::PromptOrigin;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

// Re-use compaction constants from the parent module.
use super::{
    COMPACTION_OVERFLOW_SHRINK_RATIOS, COMPACTION_SUMMARY_PREFIX,
    MAX_COMPACTION_OVERFLOW_SHRINK_ATTEMPTS, MAX_COMPACTION_RETRY_ATTEMPTS,
};

/// Retention-tail budget for the synchronous round path (ADR-001).
///
/// The round has no model-window context of its own (callers drive it
/// directly, mostly from tests), so the budget falls back to the
/// overflow-recovery floor: keep at least the latest complete indivisible
/// unit — mirroring [`FullCompaction::retention_budget_tokens`](super::FullCompaction::retention_budget_tokens)
/// when the window is unknown.
const SYNC_RETENTION_BUDGET_TOKENS: u64 = 1;

// ---------------------------------------------------------------------------
// CompactionRoundInput
// ---------------------------------------------------------------------------

/// Input for [`compaction_round`].
pub struct CompactionRoundInput<'a> {
    /// The LLM adapter.
    pub llm: &'a dyn Llm,
    /// The context memory to compact (mutated in place).
    pub memory: &'a mut ContextMemory,
    /// Cancellation token.
    pub cancel: &'a CancelToken,
    /// Tool manager (for stripping dynamic tool context).
    ///
    /// Reserved for future use — the current implementation does not yet
    /// consult the tool manager during compaction, but the field is kept in
    /// the API so callers don't need to change when this is wired up.
    pub tools: &'a crate::tool_registry::ToolManager,
    /// Optional custom instruction appended to the compaction prompt.
    pub custom_instruction: Option<&'a str>,
    /// Optional max output size for the LLM call.
    ///
    /// Reserved for future use — the current implementation does not yet
    /// enforce a max output size on the compaction LLM call, but the field is
    /// kept in the API so callers don't need to change when this is wired up.
    pub max_output_size: Option<u64>,
    /// The session's todo list — non-empty lists are appended to the
    /// compaction summary (US3.1) so progress survives compaction.
    ///
    /// `None` in standalone/test rounds; production rounds pass the
    /// session's list so [`post_process_summary`] can append the
    /// `## TODO List` section.
    pub todo_list: Option<&'a crate::todo::TodoList>,
}

/// Render a todo list into the pre-rendered item lines
/// [`post_process_summary`] consumes (US3.1).
///
/// Each item becomes `  [status] title` — the TS `renderTodoList` line
/// shape (`fullCompactionService.postProcessSummary` appends
/// `renderTodoList(todos, '## TODO List')`, whose items are two-space
/// indented status-marked titles). An empty list yields `None`: no section
/// is appended.
pub(crate) fn todo_lines(list: &crate::todo::TodoList) -> Option<Vec<String>> {
    let lines: Vec<String> = list
        .get()
        .iter()
        .map(|item| format!("  [{}] {}", item.status.as_str(), item.title))
        .collect();
    if lines.is_empty() { None } else { Some(lines) }
}

// ---------------------------------------------------------------------------
// CompactionLlmOutput (internal — result of the LLM call without memory application)
// ---------------------------------------------------------------------------

/// Output of the LLM summarization call (steps 1-7 of compaction_round),
/// before applying to memory.
pub(crate) struct CompactionLlmOutput {
    /// The raw summary text from the LLM (after post-processing).
    pub summary: String,
    /// The context summary text (prefixed with the compaction handoff marker).
    pub context_summary: String,
    /// Number of messages that were in the original history.
    pub compacted_count: usize,
    /// Number of messages dropped during overflow recovery.
    pub dropped_count: usize,
    /// Indices (ascending) into the OLD segment of the messages dropped by
    /// the overflow-recovery ladder (Levels 2 and 3) — entries that were
    /// never even covered by the summary input (ADR-007's
    /// "not recoverable from this note" boundary). The apply path maps
    /// these positions to entry seqs (ADR-007 `droppedEntrySeqs`).
    pub dropped_entry_positions: Vec<usize>,
}

// ---------------------------------------------------------------------------
// compaction_round
// ---------------------------------------------------------------------------

/// Run a single compaction round.
///
/// See the [module-level documentation](self) for the full pipeline.
pub async fn compaction_round(input: CompactionRoundInput<'_>) -> EngineResult<CompactionResult> {
    // US3.1: capture the todo handle before the destructure moves the
    // input apart (the `&mut` fields cannot be re-borrowed afterwards).
    // The lines feed the summary appendix; the handle clears the undo
    // checkpoint stack when the round applies.
    let todo_list = input.todo_list;
    let todo_lines = todo_list.and_then(todo_lines);
    let CompactionRoundInput {
        llm,
        memory,
        cancel,
        // Reserved for future use — see CompactionRoundInput docs.
        tools: _,
        custom_instruction,
        // Reserved for future use — see CompactionRoundInput docs.
        max_output_size: _,
        todo_list: _,
    } = input;

    // Step 1: Snapshot original history (for prefix-race check) and compute
    // the retention-tail cut — the summarizer only ever sees the OLD
    // segment (ADR-002). No legal cut → give up (US2.6: abort without
    // writing anything, the summarizer never starts). `CompactionUnable` —
    // a "nothing to compact" condition, not a failure (SDR-003).
    let original_history: Vec<ContextMessage> = memory.history().to_vec();
    let tokens_before = memory.token_count_with_pending();
    let Some(cut) =
        retention::compute_retention_tail(&original_history, SYNC_RETENTION_BUDGET_TOKENS)
    else {
        return Err(EngineError::CompactionUnable(
            "no legal retention-tail cut".to_string(),
        ));
    };
    let old_segment: &[ContextMessage] = &original_history[..cut.tail_start];
    // ADR-007: the planned cut's replaced-range bracket — what this round is
    // asked to summarize, threaded into the summarizer instruction. Same
    // derivation as the begin path, through the shared `replaced_entry_range`
    // helper.
    let planned_entry_range = super::replaced_entry_range(&original_history, cut.tail_start);

    // Steps 2-4, 6-7: Run the LLM call with fallback chain.
    let output = run_compaction_llm(
        llm,
        old_segment,
        cancel,
        custom_instruction,
        &[],
        planned_entry_range,
        todo_lines.as_deref(),
    )
    .await?;

    // Step 5: Prefix-race check (cancel if history changed during the LLM call).
    let current_history = memory.history();
    if !prefix_race_check(&original_history, current_history) {
        return Err(EngineError::CompactionFailed(
            "history changed during compaction (prefix-race detected)".to_string(),
        ));
    }

    // Step 8: Re-cut against the live history (prefix-race semantics: real
    // user input appended during the LLM call must be re-included in the
    // tail), then apply compaction to memory (subject to the shrink check —
    // a summary that does not shrink the history is rejected and the
    // original history kept). An applied round clears the todo undo
    // checkpoint stack (the live mirror of the reducer's
    // `context.apply_compaction` rule — Task 4 follow-up).
    let live_history: Vec<ContextMessage> = memory.history().to_vec();
    let live_cut = retention::compute_retention_tail(&live_history, SYNC_RETENTION_BUDGET_TOKENS);
    let outcome = match &live_cut {
        Some(live_cut) => memory.apply_compaction(CompactionInput {
            summary: output.summary.clone(),
            context_summary: output.context_summary.clone(),
            compacted_count: live_cut.tail_start as u32,
            tokens_before,
            dropped_count: output.dropped_count as u32,
            retention_tail_start: Some(live_cut.tail_start),
        }),
        None => ApplyOutcome::default(),
    };
    // Live mirror of the reducer's `context.apply_compaction` rule: an
    // applied round clears the todo undo checkpoint stack —
    // pre-compaction anchor positions are no longer reachable by undo.
    // Only on `applied` (a refused round left memory byte-identical, so
    // its anchors stay valid).
    if outcome.applied
        && let Some(todo_list) = todo_list
    {
        todo_list.clear_checkpoints();
    }

    // Once applied, the reported compacted count is the LIVE prefix
    // length: real user input appended during the LLM call moves the
    // re-cut forward, and the wire op must persist what was actually
    // compacted, not the summarizer's snapshot. Refusals keep the
    // snapshot value.
    let compacted_count = match (&live_cut, outcome.applied) {
        (Some(live_cut), true) => live_cut.tail_start,
        _ => output.compacted_count,
    };
    // ADR-007 provenance: the ladder's dropped positions mapped through the
    // old segment's entry numbers (by position — never by dropped_count,
    // which counts hook-context slots the positions skip), and the replaced
    // live prefix's entry range.
    let dropped_entry_seqs = output
        .dropped_entry_positions
        .iter()
        .map(|&position| old_segment[position].entry_seq)
        .collect::<Vec<u64>>();
    let (first_replaced_entry_seq, last_replaced_entry_seq) = match (&live_cut, outcome.applied) {
        (Some(live_cut), true) => {
            // Same derivation as the FullCompaction apply paths, through the
            // shared `replaced_entry_range` helper so the stamped-0 sentinel
            // debug_assert and the defensive prefix slicing cover this site
            // too.
            super::replaced_entry_range(&live_history, live_cut.tail_start)
        }
        _ => (None, None),
    };

    // Step 9: Return CompactionResult.
    let tokens_after = memory.token_count_with_pending();

    Ok(CompactionResult {
        summary: output.summary,
        context_summary: output.context_summary,
        compacted_count,
        tokens_before,
        tokens_after,
        dropped_count: output.dropped_count,
        dropped_entry_positions: output.dropped_entry_positions,
        dropped_entry_seqs,
        applied: outcome.applied,
        retained_tail_count: outcome.retained_tail_count,
        kept_head_user_message_count: outcome.kept_head_user_message_count,
        elision_message: outcome.elision_message,
        first_replaced_entry_seq,
        last_replaced_entry_seq,
    })
}

// ---------------------------------------------------------------------------
// run_compaction_llm (internal — shared by compaction_round and FullCompaction::begin)
// ---------------------------------------------------------------------------

/// Run the LLM summarization call with the 4-level fallback chain.
///
/// This does steps 2-4, 6-7 of `compaction_round`:
/// 2. Strip dynamic tool context.
/// 3. Build compaction instruction prompt.
/// 4. Call LLM with fallback chain.
/// 6. Post-process summary.
/// 7. Build context summary text.
///
/// `planned_entry_range` is the ADR-007 bracket of the segment this round
/// was asked to summarize — the planned cut's replaced range, threaded in
/// from the caller ([`FullCompaction::begin`](super::FullCompaction::begin)
/// after its cut is known; the synchronous
/// [`compaction_round`](super::compaction_round) after its own). The
/// instruction states the range so the summary may cite the entry numbers;
/// the apply path later derives its own bracket from the live re-cut, which
/// may differ when real user input lands between begin and apply — the
/// instruction describes what the round WAS asked to summarize.
pub(crate) async fn run_compaction_llm(
    llm: &dyn Llm,
    history: &[ContextMessage],
    cancel: &CancelToken,
    custom_instruction: Option<&str>,
    hook_additional_context: &[ContextMessage],
    planned_entry_range: (Option<u64>, Option<u64>),
    todo_lines: Option<&[String]>,
) -> EngineResult<CompactionLlmOutput> {
    let compacted_count = history.len();
    let (planned_first, planned_last) = planned_entry_range;

    // Step 2: Strip dynamic tool context.
    let mut working_history = strip_dynamic_tool_context(history);
    // Hook-injected context (PreCompact `additionalContext`) rides along
    // after the strip — TS parity: `historyForModel = [...stripDynamic
    // (originalHistory), ...hookInjectedMessages]`. These messages are
    // summarizer-side only and never land in session memory.
    working_history.extend(hook_additional_context.iter().cloned());

    // Old-segment index of each working-copy slot, in working-copy order.
    // The strip above filters Injection-origin messages out of the old
    // segment, so a working-copy index is an old-segment index only when
    // nothing was stripped — this table does the translation. Hook-context
    // slots map to `None`: they were never old-segment entries and must
    // not surface as dropped positions.
    let mut old_index_of: Vec<Option<usize>> = history
        .iter()
        .enumerate()
        .filter(|(_, msg)| msg.origin.as_ref() != Some(&PromptOrigin::Injection))
        .map(|(idx, _)| Some(idx))
        .collect();
    old_index_of.extend(hook_additional_context.iter().map(|_| None));

    // Step 4: Call LLM with 4-level fallback chain.
    let mut media_stripped = false;
    let mut overflow_shrink_count: u32 = 0;
    let mut empty_or_truncated_shrink_count: u32 = 0;
    let mut retry_count: u32 = 0;
    let mut dropped_count: usize = 0;
    let mut dropped_entry_positions: Vec<usize> = Vec::new();

    let summary = loop {
        cancel.check()?;

        // Step 3 (rebuilt per attempt): the instruction carries the planned
        // range plus the ladder's accumulated drops — mapped through the
        // old segment's entry numbers (by position, same rule the apply
        // path's `dropped_entry_seqs` mapping uses) — so every retry tells
        // the summarizer exactly which entries its input now excludes.
        let dropped_entry_seqs: Vec<u64> = dropped_entry_positions
            .iter()
            .map(|&position| history[position].entry_seq)
            .collect();
        let instruction = build_compaction_instruction(
            custom_instruction,
            planned_first,
            planned_last,
            &dropped_entry_seqs,
        );

        // Project the working history for the LLM call.
        let projected = project(&working_history, &ProjectOptions::compaction())?;
        let mut messages = projected;
        messages.push(Message::user(&instruction));

        let params = LlmChatParams {
            messages,
            tools: Vec::new(), // No tools for compaction.
            cancel: cancel.clone(),
            request_log_fields: None,
            cache_root_id: None,
        };

        match call_llm_once(llm, params).await {
            Ok(response) => {
                // Check for truncated response (Level 3 trigger).
                if response.stop_reason == StopReason::MaxTokens {
                    empty_or_truncated_shrink_count += 1;
                    if empty_or_truncated_shrink_count > MAX_COMPACTION_RETRY_ATTEMPTS {
                        return Err(EngineError::CompactionFailed(
                            "compaction summary truncated too many times".to_string(),
                        ));
                    }
                    let before = working_history.len();
                    working_history = drop_oldest_and_leading_tool_results(&working_history);
                    let dropped = before.saturating_sub(working_history.len());
                    dropped_count += dropped;
                    record_dropped_prefix(&mut old_index_of, dropped, &mut dropped_entry_positions);
                    retry_count = 0;
                    continue;
                }

                let text = response.text.trim();
                if text.is_empty() {
                    // Empty response (Level 3 trigger).
                    empty_or_truncated_shrink_count += 1;
                    if empty_or_truncated_shrink_count > MAX_COMPACTION_RETRY_ATTEMPTS {
                        return Err(EngineError::CompactionFailed(
                            "compaction summary empty too many times".to_string(),
                        ));
                    }
                    let before = working_history.len();
                    working_history = drop_oldest_and_leading_tool_results(&working_history);
                    let dropped = before.saturating_sub(working_history.len());
                    dropped_count += dropped;
                    record_dropped_prefix(&mut old_index_of, dropped, &mut dropped_entry_positions);
                    retry_count = 0;
                    continue;
                }

                // Success — break with the summary text.
                break text.to_string();
            }
            Err(error) => {
                // Level 1: Media strip.
                if is_too_large_or_image_error(&error) && !media_stripped {
                    working_history = replace_media_with_markers(&working_history);
                    media_stripped = true;
                    retry_count = 0;
                    continue;
                }

                // Level 2: Overflow shrink.
                if is_context_overflow_error(&error) {
                    overflow_shrink_count += 1;
                    if overflow_shrink_count > MAX_COMPACTION_OVERFLOW_SHRINK_ATTEMPTS {
                        return Err(EngineError::ContextOverflow {
                            attempts: overflow_shrink_count,
                        });
                    }
                    let before = working_history.len();
                    working_history =
                        shrink_history_after_overflow(&working_history, overflow_shrink_count);
                    let dropped = before.saturating_sub(working_history.len());
                    dropped_count += dropped;
                    record_dropped_prefix(&mut old_index_of, dropped, &mut dropped_entry_positions);
                    retry_count = 0;
                    continue;
                }

                // Level 4: Retry with backoff (for retryable errors).
                if is_retryable_error(llm, &error) {
                    if retry_count + 1 >= MAX_COMPACTION_RETRY_ATTEMPTS {
                        return Err(error);
                    }
                    let delay = backoff_delay(retry_count + 1, random_jitter());
                    cancelable_sleep(cancel, delay).await?;
                    retry_count += 1;
                    continue;
                }

                // Non-retryable, non-fallback error.
                return Err(error);
            }
        }
    };

    // Step 6: Post-process summary (append the todo list if present — US3.1:
    // after compaction the model still knows the list). The lines carry the
    // TS `renderTodoList` item shape (`  [status] title`) so the appended
    // `## TODO List` section matches the TS `postProcessSummary` output.
    let summary = post_process_summary(&summary, todo_lines);

    // Step 7: Build context summary text.
    let context_summary = build_compaction_summary_text(&summary);

    Ok(CompactionLlmOutput {
        summary,
        context_summary,
        compacted_count,
        dropped_count,
        dropped_entry_positions,
    })
}

// ---------------------------------------------------------------------------
// call_llm_once (internal — single LLM call)
// ---------------------------------------------------------------------------

/// Make a single LLM call and consume the stream.
///
/// Media materialization (T11a) runs once before the chat: the fallback
/// chain above rebuilds params from the (still `data:`-URI) history, so each
/// rebuilt attempt re-materializes — idempotent against the
/// content-addressed upload cache. A materialization error keeps the inline
/// payloads and is only warned: materialization never fails the call.
///
/// Returns the [`LlmChatResponse`] on success, or an [`EngineError`] on
/// failure (either from `llm.chat()` or from the stream itself).
async fn call_llm_once(llm: &dyn Llm, mut params: LlmChatParams) -> EngineResult<LlmChatResponse> {
    if let Err(error) = llm.materialize_media(&mut params).await {
        tracing::warn!(
            %error,
            "compaction media materialization failed; sending inline media unchanged"
        );
    }
    let stream = llm.chat(params)?;
    consume_stream(stream).await
}

// ---------------------------------------------------------------------------
// consume_stream (internal — wraps shared::collect_stream_events)
// ---------------------------------------------------------------------------

/// Consume an `LlmEventStream`, collecting all data into an [`LlmChatResponse`].
///
/// Returns an error if the stream emits an `Error` event.
async fn consume_stream(stream: crate::llm::LlmEventStream) -> EngineResult<LlmChatResponse> {
    let collected = collect_stream_events(stream, None).await;

    if let Some(msg) = collected.stream_error {
        return Err(EngineError::ProviderError(msg));
    }

    let stop_reason = collected.derived_stop_reason();
    let CollectedStream {
        text_parts,
        tool_calls,
        usage,
        ..
    } = collected;

    Ok(LlmChatResponse {
        stop_reason,
        text: text_parts.join(""),
        tool_calls,
        usage,
    })
}

// ---------------------------------------------------------------------------
// Compaction instruction prompt
// ---------------------------------------------------------------------------

/// Build the compaction instruction prompt.
///
/// The prompt instructs the LLM to write a first-person handoff note
/// summarizing the conversation. The ADR-007 provenance header opens the
/// instruction: it states the entry-number range of the segment the round
/// was asked to summarize (the *planned* cut's bracket — entry numbers are
/// stable across compactions, so the note may cite them) and, once the
/// overflow-recovery ladder has excluded entries from the summarizer
/// input, lists those excluded numbers explicitly (they are not
/// recoverable from the note). An unknown range (empty prefix) degrades to
/// the plain instruction rather than a malformed header. If
/// `custom_instruction` is provided, it is appended as an optional user
/// instruction.
fn build_compaction_instruction(
    custom_instruction: Option<&str>,
    first_entry_seq: Option<u64>,
    last_entry_seq: Option<u64>,
    dropped_entry_seqs: &[u64],
) -> String {
    let custom_block = match custom_instruction {
        Some(instr) if !instr.trim().is_empty() => {
            format!("\nOptional user instruction:\n{}\n", instr.trim())
        }
        _ => String::new(),
    };

    // ADR-007 provenance header. The exclusion sentences are omitted
    // entirely when nothing has been dropped (never an empty list).
    let provenance = match (first_entry_seq, last_entry_seq) {
        (Some(first), Some(last)) => {
            let dropped_block = if dropped_entry_seqs.is_empty() {
                String::new()
            } else {
                let list = dropped_entry_seqs
                    .iter()
                    .map(|seq| format!("#{seq}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(
                    " Entries excluded from your input due to size limits: {list}.\n\
                     Those excluded entries are not recoverable from this note."
                )
            };
            format!(
                "You are summarizing conversation entries #{first} through #{last}. \
                 You may reference\nthese entry numbers in your note \
                 (e.g. \"see entries #60-#70\") — they are stable\n\
                 across compactions.{dropped_block}\n\n"
            )
        }
        _ => String::new(),
    };

    format!(
        r#"{provenance}You are writing a handoff note to yourself. Summarize the conversation so far in first person, present tense.

Preserve:
- The user's intent and any explicit constraints
- What has been done: exact commands, file paths, and key results
- What is still unknown or uncertain
- The forward plan

Be concise but complete. Respond with text only. Do not call any tools.
{custom_block}"#
    )
}

// ---------------------------------------------------------------------------
// post_process_summary
// ---------------------------------------------------------------------------

/// Post-process the summary by appending a TODO list if present.
///
/// If `todos` is `Some` and non-empty, appends a `## TODO List` section to
/// the summary — the exact TS `postProcessSummary` shape
/// (`fullCompactionService.ts:774`): the trimmed summary, a blank line,
/// the `## TODO List` heading, then the pre-rendered item lines joined by
/// newlines (TS `renderTodoList(todos, '## TODO List')`, whose items are
/// `  [status] title`). Otherwise, returns the summary unchanged.
pub fn post_process_summary(summary: &str, todos: Option<&[String]>) -> String {
    match todos {
        Some(todos) if !todos.is_empty() => {
            let todo_list = todos.join("\n");
            format!("{}\n\n## TODO List\n{}", summary.trim(), todo_list)
        }
        _ => summary.to_string(),
    }
}

// ---------------------------------------------------------------------------
// build_compaction_summary_text
// ---------------------------------------------------------------------------

/// Build the context summary text by prepending the compaction prefix to the
/// summary.
///
/// If the summary is empty, uses `"(no summary available)"` as the body.
pub fn build_compaction_summary_text(summary: &str) -> String {
    let suffix = summary.trim();
    let body = if suffix.is_empty() {
        "(no summary available)"
    } else {
        suffix
    };
    format!("{}\n{}", COMPACTION_SUMMARY_PREFIX, body)
}

// ---------------------------------------------------------------------------
// prefix_race_check
// ---------------------------------------------------------------------------

/// Check whether it's safe to apply a compaction result.
///
/// Returns `true` if:
/// 1. The current history is at least as long as the original.
/// 2. The original prefix is unchanged (all messages match).
/// 3. Any new messages at the tail are real user input (not injections or
///    compaction summaries).
///
/// Returns `false` if any of these conditions are violated, indicating the
/// history was modified during the compaction LLM call and the result should
/// be discarded.
pub fn prefix_race_check(original: &[ContextMessage], current: &[ContextMessage]) -> bool {
    // Condition 1: current must be at least as long as original.
    if current.len() < original.len() {
        return false;
    }

    // Condition 2: original prefix must be unchanged.
    for i in 0..original.len() {
        if !context_messages_equal(&original[i], &current[i]) {
            return false;
        }
    }

    // Condition 3: tail messages must be real user input.
    for msg in &current[original.len()..] {
        if !is_real_user_input(msg) {
            return false;
        }
    }

    true
}

/// Whether a message is real user input (User role + real-user origin).
fn is_real_user_input(msg: &ContextMessage) -> bool {
    msg.message.role == Role::User
        && msg
            .origin
            .as_ref()
            .is_some_and(PromptOrigin::is_real_user_input)
}

/// Field-by-field comparison of two `ContextMessage`s.
///
/// `ContextMessage` does not derive `PartialEq`, so we compare fields manually.
fn context_messages_equal(a: &ContextMessage, b: &ContextMessage) -> bool {
    a.message == b.message && a.origin == b.origin && a.is_error == b.is_error && a.note == b.note
}

// ---------------------------------------------------------------------------
// strip_dynamic_tool_context
// ---------------------------------------------------------------------------

/// Remove dynamic tool context messages from the history.
///
/// Filters out messages with `origin: Injection` (system-reminder injections),
/// since these are ephemeral and should not be part of the compaction summary.
/// All other messages are kept verbatim.
pub fn strip_dynamic_tool_context(history: &[ContextMessage]) -> Vec<ContextMessage> {
    history
        .iter()
        .filter(|msg| msg.origin.as_ref() != Some(&PromptOrigin::Injection))
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// replace_media_with_markers
// ---------------------------------------------------------------------------

/// Replace all media content parts with text markers.
///
/// - `ImageUrl` → `[image]`
/// - `AudioUrl` → `[audio]`
/// - `VideoUrl` → `[video]`
///
/// Text and Think parts are preserved. This is the Level 1 fallback: it
/// reduces the request size by removing large media payloads.
pub fn replace_media_with_markers(history: &[ContextMessage]) -> Vec<ContextMessage> {
    history
        .iter()
        .map(|msg| {
            let mut cloned = msg.clone();
            cloned.message.content = msg
                .message
                .content
                .iter()
                .map(|part| match part {
                    ContentPart::ImageUrl { .. } => ContentPart::Text {
                        text: "[image]".to_string(),
                    },
                    ContentPart::AudioUrl { .. } => ContentPart::Text {
                        text: "[audio]".to_string(),
                    },
                    ContentPart::VideoUrl { .. } => ContentPart::Text {
                        text: "[video]".to_string(),
                    },
                    other => other.clone(),
                })
                .collect();
            cloned
        })
        .collect()
}

// ---------------------------------------------------------------------------
// shrink_history_after_overflow
// ---------------------------------------------------------------------------

/// Shrink the history to keep only the most recent messages within a token
/// budget determined by the overflow shrink ratio.
///
/// The ratio is selected from [`COMPACTION_OVERFLOW_SHRINK_RATIOS`] based on
/// the attempt number (1-indexed):
/// - Attempt 1: 0.7 (keep 70% of tokens)
/// - Attempt 2: 0.5 (keep 50%)
/// - Attempt 3: 0.35 (keep 35%)
///
/// Messages are kept from the tail (most recent) backward until the budget is
/// exhausted.
pub fn shrink_history_after_overflow(
    history: &[ContextMessage],
    attempt: u32,
) -> Vec<ContextMessage> {
    if history.is_empty() {
        return Vec::new();
    }

    let ratio = COMPACTION_OVERFLOW_SHRINK_RATIOS
        .get((attempt.saturating_sub(1)) as usize)
        .copied()
        .unwrap_or(*COMPACTION_OVERFLOW_SHRINK_RATIOS.last().unwrap_or(&0.35));

    let total_tokens = estimate_messages_tokens(history);
    let budget = (total_tokens as f64 * ratio) as u64;

    // Keep most recent messages within budget.
    let mut kept: Vec<ContextMessage> = Vec::new();
    let mut accumulated: u64 = 0;
    for msg in history.iter().rev() {
        let tokens = estimate_message_tokens(msg);
        if accumulated + tokens > budget && !kept.is_empty() {
            break;
        }
        kept.push(msg.clone());
        accumulated += tokens;
    }
    kept.reverse();
    kept
}

// ---------------------------------------------------------------------------
// drop_oldest_and_leading_tool_results
// ---------------------------------------------------------------------------

/// Drop the oldest message (index 0) and any leading tool-result messages
/// that follow.
///
/// This is the Level 3 fallback: when the LLM response is truncated or empty,
/// we reduce the history by removing the oldest message. If the oldest message
/// was an assistant with tool calls, the following tool results become orphans
/// and are also dropped.
pub fn drop_oldest_and_leading_tool_results(history: &[ContextMessage]) -> Vec<ContextMessage> {
    if history.is_empty() {
        return Vec::new();
    }
    // Drop index 0, then drop leading tool results.
    let remaining = &history[1..];
    drop_leading_tool_results(remaining)
}

// ---------------------------------------------------------------------------
// drop_leading_tool_results
// ---------------------------------------------------------------------------

/// Drop leading tool-role messages from the history.
///
/// Tool messages at the start of the history are orphans (no preceding
/// assistant tool call) and cannot be sent to the provider.
pub fn drop_leading_tool_results(history: &[ContextMessage]) -> Vec<ContextMessage> {
    let mut start = 0;
    while start < history.len() && history[start].message.role == Role::Tool {
        start += 1;
    }
    history[start..].to_vec()
}

// ---------------------------------------------------------------------------
// record_dropped_prefix
// ---------------------------------------------------------------------------

/// Record the old-segment positions of the working-copy prefix just dropped
/// by an overflow-recovery ladder rung.
///
/// Both drop sites — [`drop_oldest_and_leading_tool_results`] (Level 3) and
/// [`shrink_history_after_overflow`] (Level 2) — keep a *suffix* of the
/// working history, so the dropped slots are always a prefix of
/// `old_index_of`. Draining them keeps the table aligned with the working
/// history across successive drops; `flatten()` skips hook-context slots
/// (`None` — those were never old-segment entries). Positions accumulate in
/// ascending order because each rung drops the slots the previous rungs left
/// in place.
fn record_dropped_prefix(
    old_index_of: &mut Vec<Option<usize>>,
    dropped: usize,
    dropped_entry_positions: &mut Vec<usize>,
) {
    old_index_of
        .drain(..dropped)
        .flatten()
        .for_each(|position| dropped_entry_positions.push(position));
}

// ---------------------------------------------------------------------------
// Error classification helpers
// ---------------------------------------------------------------------------

/// Whether an error indicates a context overflow or 413 too-large response.
fn is_context_overflow_error(error: &EngineError) -> bool {
    match error {
        EngineError::ContextOverflow { .. } => true,
        EngineError::ProviderError(msg) => {
            let lower = msg.to_lowercase();
            lower.contains("413")
                || lower.contains("too many tokens")
                || lower.contains("context length")
                || lower.contains("context window")
                || lower.contains("maximum context")
                || lower.contains("request too large")
        }
        _ => false,
    }
}

/// Whether an error indicates a 413/too-large or image/media format error.
///
/// This triggers the Level 1 media-strip fallback.
fn is_too_large_or_image_error(error: &EngineError) -> bool {
    match error {
        EngineError::ProviderError(msg) => {
            let lower = msg.to_lowercase();
            lower.contains("413")
                || lower.contains("too large")
                || lower.contains("image")
                || lower.contains("media")
                || lower.contains("unsupported media")
                || lower.contains("invalid image")
        }
        _ => false,
    }
}

/// Whether an error is retryable, consulting the LLM adapter.
fn is_retryable_error(llm: &dyn Llm, error: &EngineError) -> bool {
    match error {
        EngineError::ProviderError(msg) => {
            llm.is_retryable_error(&ProviderErrorInfo::from_message(msg))
        }
        _ => false,
    }
}

// ---------------------------------------------------------------------------
// Re-exports for tests and external use
// ---------------------------------------------------------------------------

// Re-export CompactionResult from parent module.
pub use super::CompactionResult;
