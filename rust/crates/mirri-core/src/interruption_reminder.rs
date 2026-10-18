//! Interruption reminder — tracks user-cancelled turns that need a reminder
//! injected into the conversation.
//!
//! Ports the TypeScript `interruptionReminder` domain. When a turn is
//! cancelled by the user (`target == "active"`, `reason == "user_cancelled"`),
//! the turn ID is recorded as *pending*. A model-visible reminder text is
//! built via [`InterruptionReminder::build_reminder_text`] so the host can
//! inject it as a system-reminder before the next turn. Once the reminder has
//! been delivered to the conversation, [`record_delivered`](Self::record_delivered)
//! clears the pending entry.
//!
//! The pending set is kept sorted and deduplicated to match the TS wire model
//! semantics (`InterruptionReminderModel` stores `readonly number[]`).

/// Tracks turn IDs whose interruption reminder still needs to reach the
/// conversation.
///
/// Pending turn IDs are kept in ascending order with no duplicates, mirroring
/// the TS `InterruptionReminderModel` state.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InterruptionReminder {
    /// Turn IDs awaiting reminder delivery, sorted ascending.
    pending_turn_ids: Vec<usize>,
}

/// The target value that qualifies a cancellation for a reminder.
const CANCEL_TARGET_ACTIVE: &str = "active";

/// The reason value that qualifies a cancellation for a reminder.
const CANCEL_REASON_USER: &str = "user_cancelled";

/// The model-visible reminder text injected after a user interruption.
///
/// Combines the TS wording (partial output is incomplete, conversation
/// continues) with a directive to check whether prior work needs revisiting.
/// Public so the turn-flow write side and its integration tests share the
/// exact wording (the dedup on the memory's last message is text-based).
pub const INTERRUPTION_REMINDER_TEXT: &str = "\
The previous turn was interrupted by the user before completion; \
any partial output shown above is incomplete. \
The user's next message continues the conversation. \
Please check whether any work from the interrupted turn needs to be revisited or completed.";

impl InterruptionReminder {
    /// Create a new, empty reminder tracker.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a turn cancellation.
    ///
    /// Only cancellations with `target == "active"` and `reason ==
    /// "user_cancelled"` qualify for a reminder. The turn ID is inserted into
    /// the pending set in sorted order; duplicates are silently ignored.
    pub fn on_turn_cancelled(&mut self, turn_id: usize, target: &str, reason: &str) {
        if target != CANCEL_TARGET_ACTIVE || reason != CANCEL_REASON_USER {
            return;
        }

        // Binary-search insert to keep the vec sorted and deduplicated.
        match self.pending_turn_ids.binary_search(&turn_id) {
            Ok(_) => {
                // Already pending — no-op (dedup, same reference as TS).
            }
            Err(insert_pos) => {
                self.pending_turn_ids.insert(insert_pos, turn_id);
            }
        }
    }

    /// Mark a turn's reminder as delivered, removing it from the pending set.
    ///
    /// If the turn ID was not pending, this is a silent no-op.
    pub fn record_delivered(&mut self, turn_id: usize) {
        if let Ok(pos) = self.pending_turn_ids.binary_search(&turn_id) {
            self.pending_turn_ids.remove(pos);
        }
    }

    /// View the pending turn IDs (sorted ascending).
    pub fn pending(&self) -> &[usize] {
        &self.pending_turn_ids
    }

    /// Whether any interruption reminders are pending.
    pub fn has_pending(&self) -> bool {
        !self.pending_turn_ids.is_empty()
    }

    /// Build the model-visible reminder text if there are pending
    /// interruptions.
    ///
    /// Returns `None` when the pending set is empty. When non-empty, returns a
    /// system-reminder style message noting that the user interrupted the
    /// previous turn and the assistant should check whether prior work needs
    /// revisiting.
    pub fn build_reminder_text(&self) -> Option<String> {
        if self.pending_turn_ids.is_empty() {
            None
        } else {
            Some(INTERRUPTION_REMINDER_TEXT.to_string())
        }
    }

    /// Clear all pending interruption reminders.
    ///
    /// Called when the session is reset or when all reminders have been
    /// reconciled.
    pub fn reset(&mut self) {
        self.pending_turn_ids.clear();
    }
}

/// Deliver the interruption reminder for pending cancelled turns after a
/// wire restore (TS `reconcilePendingReminders`): append once (dedup
/// against the newest message), then record the delivery op per pending
/// turn so the next replay starts from a drained set.
///
/// The restore counterpart of the turn flow's write side (Task 4): a crash
/// between the durable `turn.cancel` write and the reminder delivery leaves
/// `interruption_pending` non-empty in the replayed state, and the
/// reconstructed memory misses the reminder. The append dedups the same way
/// the live write side does — when the memory's newest message is already
/// this reminder (an Injection-origin `<system-reminder>` with the
/// interruption text), the append is skipped but every pending turn's
/// `interruptionReminder.recorded` marker is still recorded. `wire` being
/// `None` (no recorder installed) still delivers into memory but records
/// nothing.
pub async fn deliver_pending_interruption_reminders(
    memory: &std::sync::Arc<std::sync::Mutex<crate::context_memory::ContextMemory>>,
    wire: Option<&dyn crate::loop_engine::WireOpRecorder>,
    pending: &[u64],
) {
    use crate::origin::PromptOrigin;

    let already_delivered = {
        let guard = memory.lock().unwrap_or_else(|e| e.into_inner());
        guard.history().last().is_some_and(|m| {
            m.origin == Some(PromptOrigin::Injection)
                && m.message.text_content()
                    == crate::system_reminder_wire::system_reminder_text(INTERRUPTION_REMINDER_TEXT)
        })
    };
    if !already_delivered {
        memory
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .append_system_reminder(INTERRUPTION_REMINDER_TEXT.to_string());
        if let Some(wire) = wire {
            wire.record_op(crate::system_reminder_wire::system_reminder_op(
                INTERRUPTION_REMINDER_TEXT,
            ))
            .await;
        }
    }
    if let Some(wire) = wire {
        for turn_id in pending {
            wire.record_op(mirri_proto::WireOp::InterruptionReminderRecorded { turn_id: *turn_id })
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // on_turn_cancelled — qualification
    // -----------------------------------------------------------------------

    #[test]
    fn should_add_turn_when_target_is_active_and_reason_is_user_cancelled() {
        // Given a fresh reminder tracker
        let mut reminder = InterruptionReminder::new();

        // When a turn is cancelled by the user
        reminder.on_turn_cancelled(3, "active", "user_cancelled");

        // Then the turn ID is pending
        assert_eq!(reminder.pending(), &[3]);
        assert!(reminder.has_pending());
    }

    #[test]
    fn should_ignore_cancellation_when_target_is_not_active() {
        // Given a fresh reminder tracker
        let mut reminder = InterruptionReminder::new();

        // When a turn is cancelled with a non-active target
        reminder.on_turn_cancelled(1, "background", "user_cancelled");

        // Then nothing is pending
        assert!(reminder.pending().is_empty());
        assert!(!reminder.has_pending());
    }

    #[test]
    fn should_ignore_cancellation_when_reason_is_not_user_cancelled() {
        // Given a fresh reminder tracker
        let mut reminder = InterruptionReminder::new();

        // When a turn is cancelled with a non-user reason
        reminder.on_turn_cancelled(1, "active", "timeout");

        // Then nothing is pending
        assert!(reminder.pending().is_empty());
    }

    // -----------------------------------------------------------------------
    // on_turn_cancelled — dedup & sorting
    // -----------------------------------------------------------------------

    #[test]
    fn should_avoid_duplicates_when_same_turn_cancelled_twice() {
        // Given a tracker with turn 2 already pending
        let mut reminder = InterruptionReminder::new();
        reminder.on_turn_cancelled(2, "active", "user_cancelled");

        // When the same turn is cancelled again
        reminder.on_turn_cancelled(2, "active", "user_cancelled");

        // Then only one entry exists
        assert_eq!(reminder.pending(), &[2]);
    }

    #[test]
    fn should_keep_pending_turns_sorted_when_added_out_of_order() {
        // Given a fresh tracker
        let mut reminder = InterruptionReminder::new();

        // When turns are cancelled in non-sorted order
        reminder.on_turn_cancelled(5, "active", "user_cancelled");
        reminder.on_turn_cancelled(1, "active", "user_cancelled");
        reminder.on_turn_cancelled(3, "active", "user_cancelled");

        // Then pending IDs are sorted ascending
        assert_eq!(reminder.pending(), &[1, 3, 5]);
    }

    // -----------------------------------------------------------------------
    // record_delivered
    // -----------------------------------------------------------------------

    #[test]
    fn should_remove_turn_from_pending_when_delivered() {
        // Given a tracker with turns 1, 3, 5 pending
        let mut reminder = InterruptionReminder::new();
        reminder.on_turn_cancelled(1, "active", "user_cancelled");
        reminder.on_turn_cancelled(3, "active", "user_cancelled");
        reminder.on_turn_cancelled(5, "active", "user_cancelled");

        // When turn 3's reminder is delivered
        reminder.record_delivered(3);

        // Then only turns 1 and 5 remain
        assert_eq!(reminder.pending(), &[1, 5]);
    }

    #[test]
    fn should_be_silent_noop_when_delivering_unknown_turn() {
        // Given a tracker with turn 1 pending
        let mut reminder = InterruptionReminder::new();
        reminder.on_turn_cancelled(1, "active", "user_cancelled");

        // When delivering a turn that was never pending
        reminder.record_delivered(99);

        // Then the pending set is unchanged
        assert_eq!(reminder.pending(), &[1]);
    }

    // -----------------------------------------------------------------------
    // build_reminder_text
    // -----------------------------------------------------------------------

    #[test]
    fn should_return_reminder_text_when_pending_exists() {
        // Given a tracker with a pending interruption
        let mut reminder = InterruptionReminder::new();
        reminder.on_turn_cancelled(1, "active", "user_cancelled");

        // When building the reminder text
        let text = reminder.build_reminder_text();

        // Then a non-empty string is returned
        let text = text.expect("should return Some when pending exists");
        assert!(text.contains("interrupted"));
        assert!(text.contains("incomplete"));
        assert!(text.contains("revisited"));
    }

    #[test]
    fn should_return_none_when_no_pending_exists() {
        // Given a fresh tracker with no pending interruptions
        let reminder = InterruptionReminder::new();

        // When building the reminder text
        let text = reminder.build_reminder_text();

        // Then None is returned
        assert!(text.is_none());
    }

    // -----------------------------------------------------------------------
    // reset
    // -----------------------------------------------------------------------

    #[test]
    fn should_clear_all_pending_when_reset() {
        // Given a tracker with multiple pending interruptions
        let mut reminder = InterruptionReminder::new();
        reminder.on_turn_cancelled(1, "active", "user_cancelled");
        reminder.on_turn_cancelled(2, "active", "user_cancelled");

        // When reset
        reminder.reset();

        // Then no pending interruptions remain
        assert!(!reminder.has_pending());
        assert!(reminder.pending().is_empty());
        assert!(reminder.build_reminder_text().is_none());
    }

    // -----------------------------------------------------------------------
    // Empty / default state
    // -----------------------------------------------------------------------

    #[test]
    fn should_have_no_pending_when_newly_created() {
        // Given a new tracker
        let reminder = InterruptionReminder::new();

        // Then it has no pending interruptions
        assert!(!reminder.has_pending());
        assert!(reminder.pending().is_empty());
    }
}
