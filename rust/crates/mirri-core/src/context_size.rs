//! Context-size tracking — measured vs estimated token counts.
//!
//! Ports the TypeScript `contextSize` domain. The tracker owns the last
//! *measured* context token count — the real value reported by the provider
//! after an LLM exchange — and derives `size` / `measured` / `estimated`
//! breakdowns for arbitrary context slices.
//!
//! ## Measured prefix
//!
//! After each measured LLM exchange the caller invokes [`ContextSizeTracker::measured`]
//! with the full context length and the provider-reported token total. That
//! establishes a *deterministic measured prefix*: messages `[0 .. measured_length)`
//! are covered by `measured_tokens`.
//!
//! Messages beyond the measured prefix are *unmeasured* — their token count is
//! only known heuristically via [`token_estimator`].
//!
//! ## Slice semantics
//!
//! [`ContextSizeTracker::get`] accepts `start` / `end` indices that follow
//! JavaScript `Array.prototype.slice` semantics: `None` defaults to the full
//! range, negative values count back from the end, and an inverted range
//! (`from >= to`) yields an empty slice.
//!
//! [`token_estimator`]: crate::context_memory::token_estimator

use crate::context::ContextMessage;
use crate::context_memory::token_estimator;

/// A snapshot of context size broken into measured and estimated portions.
///
/// - `size` — total tokens for the requested range (`measured + estimated`).
/// - `measured` — tokens from the deterministic measured prefix.
/// - `estimated` — heuristic token estimate for the unmeasured tail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ContextSize {
    /// Total tokens for the requested range (`measured + estimated`).
    pub size: usize,
    /// Tokens from the deterministic measured prefix.
    pub measured: usize,
    /// Heuristic estimate for the not-yet-measured portion.
    pub estimated: usize,
}

/// Tracks the last measured context token count and derives size breakdowns.
///
/// Owns three values:
///
/// - `measured_length` — number of messages in the measured prefix.
/// - `measured_tokens` — provider-reported token count for that prefix.
/// - `last_emitted_tokens` — watermark of the last emitted measured value
///   (used by the host to detect changes and avoid redundant status updates).
///
/// After compaction, undo, or history clear the caller should invoke
/// [`reset`](Self::reset) so stale measured data does not leak into future
/// reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextSizeTracker {
    /// Number of messages in the deterministic measured prefix.
    measured_length: usize,
    /// Token count reported by the provider for the measured prefix.
    measured_tokens: usize,
    /// Watermark of the last emitted measured token value.
    last_emitted_tokens: usize,
}

impl Default for ContextSizeTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ContextSizeTracker {
    /// Create a new tracker with no measured data.
    pub fn new() -> Self {
        Self {
            measured_length: 0,
            measured_tokens: 0,
            last_emitted_tokens: 0,
        }
    }

    /// Current measured prefix length (in messages).
    pub fn measured_length(&self) -> usize {
        self.measured_length
    }

    /// Current measured token count for the prefix.
    pub fn measured_tokens(&self) -> usize {
        self.measured_tokens
    }

    /// Last emitted token watermark.
    pub fn last_emitted_tokens(&self) -> usize {
        self.last_emitted_tokens
    }

    /// Compute the [`ContextSize`] for a slice of `context`.
    ///
    /// `start` and `end` follow JavaScript `Array.prototype.slice` semantics:
    ///
    /// - `None` for `start` defaults to `0`; `None` for `end` defaults to the
    ///   full context length.
    /// - Negative values count back from the end (e.g. `-1` = last message).
    /// - An inverted range (`from >= to`) yields `{ size: 0, measured: 0,
    ///   estimated: 0 }`.
    ///
    /// The measured portion uses the stored `measured_tokens` only when the
    /// requested range covers the **entire** measured prefix (`from == 0` and
    /// `measured_end == measured_length`). Sub-ranges of the measured prefix
    /// fall back to a per-message estimate because the exact aggregate is only
    /// known for the full prefix.
    pub fn get(
        &self,
        context: &[ContextMessage],
        start: Option<isize>,
        end: Option<isize>,
    ) -> ContextSize {
        let context_length = context.len();

        // Clamp the stored measured length to the live context — a corrupt or
        // overshooting record must not push reads onto the estimate branch.
        let measured_length = self.measured_length.min(context_length);

        let from = normalize_slice_index(start.unwrap_or(0), context_length);
        let to = normalize_slice_index(end.unwrap_or(context_length as isize), context_length);

        // Inverted range → empty slice.
        if from >= to {
            return ContextSize::default();
        }

        let measured_end = to.min(measured_length);
        let estimated_start = from.max(measured_length);

        // The exact aggregate is only known for the full measured prefix. A
        // sub-range of the prefix falls back to per-message estimation.
        let measured = if from == 0 && measured_end == measured_length {
            self.measured_tokens
        } else {
            estimate_slice_tokens(context, from, measured_end)
        };

        let estimated = estimate_slice_tokens(context, estimated_start, to);

        ContextSize {
            size: measured + estimated,
            measured,
            estimated,
        }
    }

    /// Record a measured LLM exchange.
    ///
    /// `context_length` is the number of messages in the context at the time
    /// of the exchange; `usage_tokens` is the provider-reported token total
    /// (input + output) for that exchange. The measured prefix is repointed to
    /// the full context length — every exchange establishes a fresh prefix.
    ///
    /// Returns `true` if the measured value changed (i.e. the new
    /// `measured_tokens` differs from `last_emitted_tokens`), allowing the
    /// caller to decide whether to emit a status event.
    pub fn measured(&mut self, context_length: usize, usage_tokens: usize) -> bool {
        self.measured_length = context_length;
        self.measured_tokens = usage_tokens;

        if self.measured_tokens == self.last_emitted_tokens {
            false
        } else {
            self.last_emitted_tokens = self.measured_tokens;
            true
        }
    }

    /// Clear all measured state.
    ///
    /// Called after compaction, undo, or history clear so stale measured data
    /// does not leak into future reads. Resets all three fields to zero.
    pub fn reset(&mut self) {
        self.measured_length = 0;
        self.measured_tokens = 0;
        self.last_emitted_tokens = 0;
    }
}

/// Normalize a slice index following JavaScript `Array.prototype.slice`
/// semantics.
///
/// - Negative `index` counts back from the end (`length + index`), clamped to
///   `0`.
/// - Non-negative `index` is clamped to `length`.
///
/// # Examples
/// ```
/// use mirri_core::context_size::normalize_slice_index;
/// assert_eq!(normalize_slice_index(0, 5), 0);
/// assert_eq!(normalize_slice_index(-1, 5), 4);
/// assert_eq!(normalize_slice_index(-10, 5), 0);
/// assert_eq!(normalize_slice_index(10, 5), 5);
/// ```
pub fn normalize_slice_index(index: isize, length: usize) -> usize {
    if index < 0 {
        // Saturating subtraction: if |index| > length, clamp to 0.
        let from_end = (-index) as usize;
        length.saturating_sub(from_end)
    } else {
        (index as usize).min(length)
    }
}

/// Estimate tokens for `context[start..end]` using the token estimator.
///
/// Returns `0` for empty or inverted ranges.
fn estimate_slice_tokens(context: &[ContextMessage], start: usize, end: usize) -> usize {
    if start >= end || start >= context.len() {
        return 0;
    }
    let end = end.min(context.len());
    token_estimator::estimate_messages_tokens(&context[start..end]) as usize
}

/// Calculate the maximum completion (output) tokens for a request.
///
/// Derives the budget from the current context token count and the model's
/// context-window limit, reserving a fixed safety margin for the response.
/// Saturates to zero when the context leaves no room for completion.
pub fn calculate_max_tokens(context_tokens: usize, model_limit: usize) -> usize {
    const SAFETY_MARGIN: usize = 1024;
    model_limit
        .saturating_sub(context_tokens)
        .saturating_sub(SAFETY_MARGIN)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::ContextMessage;
    use crate::message::{ContentPart, Message, Role};
    use crate::origin::PromptOrigin;

    /// Build a simple user-text `ContextMessage`.
    fn user_msg(text: &str) -> ContextMessage {
        ContextMessage {
            message: Message {
                role: Role::User,
                name: None,
                content: vec![ContentPart::Text {
                    text: text.to_string(),
                }],
                tool_calls: Vec::new(),
                tool_call_id: None,
                partial: false,
            },
            origin: Some(PromptOrigin::User),
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        }
    }

    /// Build a context with `n` user messages, each 8 ASCII chars (≈2 tokens).
    fn context_with_n_messages(n: usize) -> Vec<ContextMessage> {
        (0..n).map(|i| user_msg(&format!("msg{:04}", i))).collect()
    }

    // -----------------------------------------------------------------------
    // ContextSize struct
    // -----------------------------------------------------------------------

    #[test]
    fn should_default_to_zero_when_no_data() {
        let size = ContextSize::default();
        assert_eq!(size.size, 0);
        assert_eq!(size.measured, 0);
        assert_eq!(size.estimated, 0);
    }

    // -----------------------------------------------------------------------
    // normalize_slice_index
    // -----------------------------------------------------------------------

    #[test]
    fn should_return_zero_when_index_is_zero() {
        assert_eq!(normalize_slice_index(0, 5), 0);
    }

    #[test]
    fn should_count_from_end_when_index_is_negative() {
        assert_eq!(normalize_slice_index(-1, 5), 4);
        assert_eq!(normalize_slice_index(-3, 10), 7);
    }

    #[test]
    fn should_clamp_to_zero_when_negative_index_exceeds_length() {
        assert_eq!(normalize_slice_index(-10, 5), 0);
    }

    #[test]
    fn should_clamp_to_length_when_positive_index_exceeds_length() {
        assert_eq!(normalize_slice_index(10, 5), 5);
    }

    // -----------------------------------------------------------------------
    // ContextSizeTracker::get — measured prefix
    // -----------------------------------------------------------------------

    #[test]
    fn should_return_measured_tokens_when_range_covers_full_prefix() {
        // Given a tracker with 3 measured messages at 100 tokens
        let mut tracker = ContextSizeTracker::new();
        tracker.measured(3, 100);

        // And a context of 3 messages
        let context = context_with_n_messages(3);

        // When getting the full range
        let size = tracker.get(&context, None, None);

        // Then measured should be the stored value, estimated should be 0
        assert_eq!(size.measured, 100);
        assert_eq!(size.estimated, 0);
        assert_eq!(size.size, 100);
    }

    #[test]
    fn should_estimate_measured_portion_when_range_is_sub_slice_of_prefix() {
        // Given a tracker with 5 measured messages at 500 tokens
        let mut tracker = ContextSizeTracker::new();
        tracker.measured(5, 500);

        // And a context of 5 messages (each ~2 tokens)
        let context = context_with_n_messages(5);

        // When getting a sub-range [0, 2) of the measured prefix
        let size = tracker.get(&context, Some(0), Some(2));

        // Then measured falls back to per-message estimate (not the stored 500)
        assert!(size.measured > 0);
        assert!(size.measured < 500);
        assert_eq!(size.estimated, 0);
        assert_eq!(size.size, size.measured);
    }

    // -----------------------------------------------------------------------
    // ContextSizeTracker::get — unmeasured tail
    // -----------------------------------------------------------------------

    #[test]
    fn should_estimate_unmeasured_tail_when_context_grows_beyond_prefix() {
        // Given a tracker with 2 measured messages at 50 tokens
        let mut tracker = ContextSizeTracker::new();
        tracker.measured(2, 50);

        // And a context of 4 messages (2 measured + 2 unmeasured)
        let context = context_with_n_messages(4);

        // When getting the full range
        let size = tracker.get(&context, None, None);

        // Then measured = 50 (full prefix), estimated > 0 (tail)
        assert_eq!(size.measured, 50);
        assert!(size.estimated > 0);
        assert_eq!(size.size, 50 + size.estimated);
    }

    #[test]
    fn should_return_all_estimated_when_no_measured_data() {
        // Given a fresh tracker with no measured data
        let tracker = ContextSizeTracker::new();

        // And a context with messages
        let context = context_with_n_messages(3);

        // When getting the full range
        let size = tracker.get(&context, None, None);

        // Then measured = 0, estimated > 0, size = estimated
        assert_eq!(size.measured, 0);
        assert!(size.estimated > 0);
        assert_eq!(size.size, size.estimated);
    }

    // -----------------------------------------------------------------------
    // ContextSizeTracker::get — slice semantics
    // -----------------------------------------------------------------------

    #[test]
    fn should_support_negative_indices_when_requesting_a_slice() {
        // Given a tracker with 2 measured messages at 50 tokens
        let mut tracker = ContextSizeTracker::new();
        tracker.measured(2, 50);

        // And a context of 4 messages
        let context = context_with_n_messages(4);

        // When getting the last 2 messages (unmeasured tail)
        let size = tracker.get(&context, Some(-2), None);

        // Then measured = 0 (all in unmeasured portion), estimated > 0
        assert_eq!(size.measured, 0);
        assert!(size.estimated > 0);
        assert_eq!(size.size, size.estimated);
    }

    #[test]
    fn should_return_empty_when_range_is_inverted() {
        // Given a tracker with some measured data
        let mut tracker = ContextSizeTracker::new();
        tracker.measured(2, 50);

        // And a context
        let context = context_with_n_messages(4);

        // When getting an inverted range [3, 1)
        let size = tracker.get(&context, Some(3), Some(1));

        // Then all fields are 0
        assert_eq!(size.size, 0);
        assert_eq!(size.measured, 0);
        assert_eq!(size.estimated, 0);
    }

    // -----------------------------------------------------------------------
    // ContextSizeTracker::get — clamping
    // -----------------------------------------------------------------------

    #[test]
    fn should_clamp_measured_length_to_live_context_when_stored_value_overshoots() {
        // Given a tracker with a measured length of 5 (corrupt/overshooting)
        let mut tracker = ContextSizeTracker::new();
        tracker.measured(5, 1234);

        // But a live context of only 1 message
        let context = context_with_n_messages(1);

        // When getting the full range
        let size = tracker.get(&context, None, None);

        // Then measured = 1234 (clamped prefix covers the entire 1-message context)
        assert_eq!(size.measured, 1234);
        assert_eq!(size.estimated, 0);
        assert_eq!(size.size, 1234);
    }

    // -----------------------------------------------------------------------
    // ContextSizeTracker::measured
    // -----------------------------------------------------------------------

    #[test]
    fn should_repoint_measured_prefix_when_a_newer_exchange_is_recorded() {
        // Given a tracker measured at 2 messages / 100 tokens
        let mut tracker = ContextSizeTracker::new();
        tracker.measured(2, 100);

        // When a second exchange reports 4 messages / 300 tokens
        let changed = tracker.measured(4, 300);

        // Then the tracker is repointed
        assert!(changed);
        assert_eq!(tracker.measured_length(), 4);
        assert_eq!(tracker.measured_tokens(), 300);
        assert_eq!(tracker.last_emitted_tokens(), 300);
    }

    #[test]
    fn should_return_false_when_measured_tokens_unchanged() {
        // Given a tracker measured at 100 tokens
        let mut tracker = ContextSizeTracker::new();
        tracker.measured(2, 100);

        // When measured again with the same token count
        let changed = tracker.measured(3, 100);

        // Then no change is reported
        assert!(!changed);
        assert_eq!(tracker.measured_tokens(), 100);
    }

    // -----------------------------------------------------------------------
    // ContextSizeTracker::reset
    // -----------------------------------------------------------------------

    #[test]
    fn should_clear_all_state_when_reset() {
        // Given a tracker with measured data
        let mut tracker = ContextSizeTracker::new();
        tracker.measured(5, 500);

        // When reset
        tracker.reset();

        // Then all fields are zero
        assert_eq!(tracker.measured_length(), 0);
        assert_eq!(tracker.measured_tokens(), 0);
        assert_eq!(tracker.last_emitted_tokens(), 0);

        // And get() returns all-estimated for a non-empty context
        let context = context_with_n_messages(3);
        let size = tracker.get(&context, None, None);
        assert_eq!(size.measured, 0);
        assert!(size.estimated > 0);
    }

    // -----------------------------------------------------------------------
    // Integration: partial overlap
    // -----------------------------------------------------------------------

    #[test]
    fn should_split_measured_and_estimated_when_range_straddles_boundary() {
        // Given a tracker with 3 measured messages at 200 tokens
        let mut tracker = ContextSizeTracker::new();
        tracker.measured(3, 200);

        // And a context of 5 messages
        let context = context_with_n_messages(5);

        // When getting the full range
        let size = tracker.get(&context, None, None);

        // Then measured = 200 (full prefix), estimated > 0 (messages 3-4)
        assert_eq!(size.measured, 200);
        assert!(size.estimated > 0);
        assert_eq!(size.size, 200 + size.estimated);

        // And when getting only the measured prefix [0, 3)
        let measured_only = tracker.get(&context, Some(0), Some(3));
        assert_eq!(measured_only.measured, 200);
        assert_eq!(measured_only.estimated, 0);

        // And when getting only the unmeasured tail [3, 5)
        let estimated_only = tracker.get(&context, Some(3), Some(5));
        assert_eq!(estimated_only.measured, 0);
        assert!(estimated_only.estimated > 0);
    }
}
