//! Retention-tail cutting (压缩域 ADR-001).
//!
//! When compaction fires, the most recent conversation is kept verbatim
//! (the retention tail) and only the older history is summarized. The cut
//! line is computed by a token budget walked from the newest message
//! backwards; the budget is a **minimum** — an illegal cut (one that would
//! split a tool call from its result) retreats OLDER only, so the tail can
//! only grow, never shrink.

use crate::context::ContextMessage;
use crate::context_memory::token_estimator::estimate_message_tokens;
use crate::message::Role;

/// Default ratio of the model's context window reserved for the retention
/// tail (压缩域 ADR-001: budget = window × 0.16, configurable in M5).
pub const RETENTION_TAIL_RATIO: f64 = 0.16;

/// The retention-tail cut result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionCut {
    /// Index into the original history where the tail begins; messages
    /// `[tail_start..]` are the retention tail.
    pub tail_start: usize,
    /// Estimated tokens of the tail (≥ the requested budget, because
    /// illegal cuts only retreat older).
    pub tail_tokens: u64,
}

/// Whether the history may be cut *after* `index` (i.e. messages
/// `..=index` form the compacted prefix and `index+1..` the tail).
///
/// Port of the TS `canSplitAfter` legality rules:
/// - never cut right after a user message (its following injections/tools
///   belong with it),
/// - never cut right after an assistant message that still carries tool
///   calls (the results are pending),
/// - never cut right before a tool result,
/// - never cut when the prefix ends inside an open tool exchange (fewer
///   tool results than the assistant above them issued).
pub fn can_split_after(history: &[ContextMessage], index: usize) -> bool {
    let Some(m) = history.get(index) else {
        return false;
    };
    if matches!(&m.message.role, Role::User) {
        return false;
    }
    if matches!(&m.message.role, Role::Assistant) && !m.message.tool_calls.is_empty() {
        return false;
    }
    if matches!(
        history.get(index + 1).map(|next| &next.message.role),
        Some(Role::Tool)
    ) {
        return false;
    }
    !prefix_ends_with_open_tool_exchange(history, index)
}

/// True when `history[..=index]` ends with tool results that do not close
/// all the tool calls of the assistant above them.
fn prefix_ends_with_open_tool_exchange(history: &[ContextMessage], index: usize) -> bool {
    if !matches!(
        history.get(index).map(|m| &m.message.role),
        Some(Role::Tool)
    ) {
        return false;
    }
    let mut tool_result_count = 0usize;
    for m in history[..=index].iter().rev() {
        if matches!(&m.message.role, Role::Tool) {
            tool_result_count += 1;
            continue;
        }
        return matches!(&m.message.role, Role::Assistant)
            && m.message.tool_calls.len() > tool_result_count;
    }
    false
}

/// Compute the retention-tail cut for `history` under `budget_tokens`.
///
/// Walk from the newest message backwards accumulating estimated tokens
/// until the budget is met — that index is the initial `tail_start`.
/// While the cut there is illegal, retreat older (decrement `tail_start`).
///
/// Returns `None` when:
/// - the whole history fits within the budget (nothing to compact), or
/// - the retreat reaches index 0 — the whole history would be the tail,
///   no compactable prefix remains (US2.6: give up, write nothing).
pub fn compute_retention_tail(
    history: &[ContextMessage],
    budget_tokens: u64,
) -> Option<RetentionCut> {
    if history.is_empty() || budget_tokens == 0 {
        return None;
    }

    let mut accumulated: u64 = 0;
    let mut tail_start: Option<usize> = None;
    for i in (0..history.len()).rev() {
        accumulated = accumulated.saturating_add(estimate_message_tokens(&history[i]));
        if accumulated >= budget_tokens {
            tail_start = Some(i);
            break;
        }
    }

    // The whole history fits within the budget — nothing to compact.
    let mut start = tail_start?;

    // Retreat older while the cut is illegal. Reaching 0 means the whole
    // history would be the tail — no compactable prefix remains.
    while start > 0 && !can_split_after(history, start - 1) {
        start -= 1;
    }
    if start == 0 {
        return None;
    }

    Some(RetentionCut {
        tail_start: start,
        tail_tokens: accumulated,
    })
}
