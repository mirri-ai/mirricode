//! Compaction input types and user-message selection.
//!
//! Compaction (ADR-001/002) rewrites the model context as:
//! `[头部精华用户消息][省略说明?][摘要（CompactionSummary）][保留尾]` —
//! a head essence of the oldest real user messages from the compacted
//! prefix (whole-message granularity, within
//! [`COMPACT_USER_MESSAGE_HEAD_TOKENS`]), an optional `<system-reminder>`
//! elision marker when real user input was omitted in between, the summary
//! message, and the retention tail kept verbatim.
//!
//! The legacy [`select_compaction_user_messages`] (head + tail with
//! truncation, no retention tail) remains for callers that select user
//! messages directly.

use crate::context::ContextMessage;
use crate::message::{ContentPart, Message};
use crate::origin::PromptOrigin;

/// Maximum total tokens budget for kept user messages during compaction.
pub const COMPACT_USER_MESSAGE_MAX_TOKENS: u64 = 20_000;

/// Of [`COMPACT_USER_MESSAGE_MAX_TOKENS`], the slice reserved for the OLDEST
/// user messages once the pool no longer fits the budget. The earliest prompts
/// usually carry the original task statement, which a tail-only selection
/// would drop entirely.
pub const COMPACT_USER_MESSAGE_HEAD_TOKENS: u64 = 2_000;

/// Input for [`ContextMemory::apply_compaction`](super::ContextMemory::apply_compaction).
#[derive(Debug, Clone)]
pub struct CompactionInput {
    /// The raw summary text from the LLM summarizer.
    pub summary: String,
    /// The context summary text (prefixed with handoff marker).
    pub context_summary: String,
    /// Number of messages that were compacted.
    pub compacted_count: u32,
    /// Token count before compaction.
    pub tokens_before: u64,
    /// Number of messages dropped during overflow recovery.
    pub dropped_count: u32,
    /// Index into the pre-compaction history where the retention tail
    /// begins (messages `[tail_start..]` are kept verbatim). `None` aborts
    /// the apply (no legal cut — the caller should not have started the
    /// round).
    pub retention_tail_start: Option<usize>,
}

/// Outcome of an applied (or refused) compaction — carries the counts the
/// wire op must persist for resume reconstruction (ADR-005).
#[derive(Debug, Clone, Default)]
pub struct ApplyOutcome {
    pub applied: bool,
    /// Materialized memory slots kept verbatim as the retention tail.
    pub retained_tail_count: usize,
    /// Real user messages kept verbatim as the head essence.
    pub kept_head_user_message_count: usize,
    /// The elision marker exactly as inserted (serialized), `None` when
    /// nothing was omitted between head and summary.
    pub elision_message: Option<serde_json::Value>,
}

/// Whole-message head selection from the compacted prefix: the oldest
/// real user messages whose cumulative estimated tokens stay within
/// `head_tokens`. Stops at the first message that would exceed the budget
/// (no mid-message truncation — US2.5 keeps the original text).
///
/// Returns the kept messages and whether any real user input in the
/// prefix was left out (an elision marker is due).
pub fn select_head_user_messages(
    prefix: &[ContextMessage],
    head_tokens: u64,
) -> (Vec<ContextMessage>, bool) {
    let mut kept = Vec::new();
    let mut remaining = head_tokens;
    for m in prefix {
        if !m.origin.as_ref().is_some_and(|o| o.is_real_user_input()) {
            continue;
        }
        let tokens = super::token_estimator::estimate_message_tokens(m);
        if tokens > remaining {
            break;
        }
        remaining -= tokens;
        kept.push(m.clone());
    }
    let total_real = prefix
        .iter()
        .filter(|m| m.origin.as_ref().is_some_and(|o| o.is_real_user_input()))
        .count();
    let elided = kept.len() < total_real;
    (kept, elided)
}

/// Build the elision marker inserted between the head essence and the
/// summary when real user input was omitted in between.
pub fn elision_marker_message() -> ContextMessage {
    ContextMessage {
        message: Message::user(
            "<system-reminder>\nSome of this conversation's earlier user messages were omitted here during compaction: the messages above this note are the oldest user input, the messages below are the most recent, and the content in between is covered by the compaction summary that follows.\n</system-reminder>".to_string(),
        ),
        origin: Some(PromptOrigin::Injection),
        is_error: false,
        note: None,
        with_skills: None,
        entry_seq: 0,
    }
}

/// Result of selecting which user messages to keep after compaction.
#[derive(Debug, Clone)]
pub struct CompactionUserMessageSelection {
    /// Oldest user messages kept within the head budget, then most recent kept
    /// within the tail budget. When `elided` is false, this holds all user
    /// messages verbatim.
    pub kept: Vec<ContextMessage>,
    /// True when user content between head and tail was dropped.
    pub elided: bool,
    /// Estimated tokens of the dropped middle. 0 when `elided` is false.
    pub omitted_tokens: u64,
}

/// Select which user messages to keep after compaction.
///
/// Rules (matching the TypeScript `selectCompactionUserMessages`):
/// - Only keep messages where `origin.is_real_user_input()` (User,
///   SkillActivation, PluginCommand).
/// - If total fits in `max_tokens` → keep all (no elision).
/// - Otherwise: HEAD = oldest `head_tokens` (truncated keeping BEGINNING)
///   + TAIL = most recent `max_tokens - head_tokens` (truncated keeping END)
///   + elision marker between them.
///
/// The head may extend into the beginning of the same message whose end anchors
/// the tail, so a single oversized message still keeps both its start and its
/// most recent part.
pub fn select_compaction_user_messages(
    history: &[ContextMessage],
    max_tokens: u64,
) -> CompactionUserMessageSelection {
    select_with_head_budget(history, max_tokens, COMPACT_USER_MESSAGE_HEAD_TOKENS)
}

/// Same as [`select_compaction_user_messages`] but with an explicit head budget
/// (used by tests).
pub fn select_with_head_budget(
    history: &[ContextMessage],
    max_tokens: u64,
    head_tokens: u64,
) -> CompactionUserMessageSelection {
    // Collect only real user input messages (skip compaction summaries).
    let user_messages: Vec<&ContextMessage> = history
        .iter()
        .filter(|m| m.origin.as_ref().is_some_and(|o| o.is_real_user_input()))
        .collect();

    if user_messages.is_empty() {
        return CompactionUserMessageSelection {
            kept: vec![],
            elided: false,
            omitted_tokens: 0,
        };
    }

    let total_tokens: u64 = user_messages
        .iter()
        .map(|m| super::token_estimator::estimate_message_tokens(m))
        .sum();

    if total_tokens <= max_tokens {
        return CompactionUserMessageSelection {
            kept: user_messages.into_iter().cloned().collect(),
            elided: false,
            omitted_tokens: 0,
        };
    }

    let head_budget = head_tokens.min(max_tokens);
    let tail_budget = max_tokens.saturating_sub(head_budget);

    // Tail: newest messages first. The boundary message keeps its END.
    let mut tail: Vec<ContextMessage> = Vec::new();
    let mut tail_remaining = tail_budget;
    let mut head_end_exclusive = user_messages.len();
    let mut tail_boundary_dropped_prefix: Option<ContextMessage> = None;

    for i in (0..user_messages.len()).rev() {
        if tail_remaining == 0 {
            break;
        }
        let msg = user_messages[i];
        let tokens = super::token_estimator::estimate_message_tokens(msg);
        if tokens <= tail_remaining {
            tail.push(msg.clone());
            tail_remaining -= tokens;
            head_end_exclusive = i;
            continue;
        }
        // Boundary message: truncate keeping the END.
        let full_text = extract_text(msg);
        let kept_suffix =
            super::token_estimator::truncate_text_to_tokens_from_end(&full_text, tail_remaining);
        tail.push(replace_message_text(msg, &kept_suffix));
        head_end_exclusive = i;
        // The cut-off beginning is still head-eligible.
        let dropped_prefix_len = full_text.len().saturating_sub(kept_suffix.len());
        if dropped_prefix_len > 0 {
            let dropped_prefix_text = full_text[..dropped_prefix_len].to_string();
            tail_boundary_dropped_prefix = Some(replace_message_text(msg, &dropped_prefix_text));
        }
        break;
    }
    tail.reverse();

    // Head: oldest messages first, over everything the tail did not keep.
    // The boundary message keeps its BEGINNING.
    let mut owned_head_candidates: Vec<ContextMessage> = user_messages[..head_end_exclusive]
        .iter()
        .map(|m| (*m).clone())
        .collect();
    if let Some(prefix) = &tail_boundary_dropped_prefix {
        owned_head_candidates.push(prefix.clone());
    }

    let mut head: Vec<ContextMessage> = Vec::new();
    let mut head_remaining = head_budget;
    for msg in &owned_head_candidates {
        if head_remaining == 0 {
            break;
        }
        let tokens = super::token_estimator::estimate_message_tokens(msg);
        if tokens <= head_remaining {
            head.push(msg.clone());
            head_remaining -= tokens;
            continue;
        }
        // Boundary: truncate keeping the BEGINNING.
        let full_text = extract_text(msg);
        let truncated = super::token_estimator::truncate_text_to_tokens(&full_text, head_remaining);
        head.push(replace_message_text(msg, &truncated));
        break;
    }

    // Compute omitted tokens from total minus what we kept.
    let kept_tokens: u64 = head
        .iter()
        .chain(tail.iter())
        .map(super::token_estimator::estimate_message_tokens)
        .sum();
    let omitted_tokens = total_tokens.saturating_sub(kept_tokens);

    let mut kept = head;
    kept.extend(tail);

    CompactionUserMessageSelection {
        kept,
        elided: true,
        omitted_tokens,
    }
}

/// Extract all text from a message's content parts, concatenated.
fn extract_text(msg: &ContextMessage) -> String {
    let mut text = String::new();
    for part in &msg.message.content {
        if let ContentPart::Text { text: t } = part {
            text.push_str(t);
        }
    }
    text
}

/// Rebuild a message around new text content, preserving origin and other
/// metadata. Dropping to text only loses any media attachments — media cannot
/// be partially truncated, and keeping it whole would overshoot the budget.
fn replace_message_text(msg: &ContextMessage, text: &str) -> ContextMessage {
    let mut cloned = msg.clone();
    cloned.message.content = vec![ContentPart::Text {
        text: text.to_string(),
    }];
    cloned.message.tool_calls.clear();
    cloned
}
