//! Steer buffering — mid-turn user interjections.
//!
//! When a user sends a message while a turn is active, the message is buffered
//! rather than immediately appended to context. The buffer is flushed at the
//! next `before_step` hook (after compaction) so that the steered content lands
//! in the post-compaction context window.

use crate::context_memory::ContextMemory;
use crate::message::ContentPart;
use crate::origin::PromptOrigin;

/// A buffered steer message waiting to be flushed into context memory.
#[derive(Debug, Clone)]
pub struct BufferedSteer {
    /// The steer content (text, media, etc.).
    pub content: Vec<ContentPart>,
    /// Where the steer originated (typically `PromptOrigin::User`).
    pub origin: PromptOrigin,
}

/// A FIFO buffer of steer messages.
///
/// Steers are appended via [`push`] and flushed into `ContextMemory` via
/// [`flush`]. The buffer is cloned cheaply when the turn's hooks need a
/// reference — it is an `Arc<Mutex<SteerBuffer>>` in practice.
///
/// [`push`]: SteerBuffer::push
/// [`flush`]: SteerBuffer::flush
#[derive(Debug, Clone, Default)]
pub struct SteerBuffer {
    items: Vec<BufferedSteer>,
}

impl SteerBuffer {
    /// Create an empty steer buffer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Push a steer message onto the buffer.
    pub fn push(&mut self, content: Vec<ContentPart>, origin: PromptOrigin) {
        self.items.push(BufferedSteer { content, origin });
    }

    /// Flush all buffered steers into context memory as user messages.
    ///
    /// Returns `true` if at least one steer was flushed, `false` if the buffer
    /// was empty.
    pub fn flush(&mut self, memory: &mut ContextMemory) -> bool {
        if self.items.is_empty() {
            return false;
        }
        let items = std::mem::take(&mut self.items);
        for steer in items {
            memory.append_user_message(steer.content, steer.origin);
        }
        true
    }

    /// Take all buffered steers out, leaving the buffer empty.
    ///
    /// The durable-flush path (T3) snapshots the buffer synchronously,
    /// records the consumption marker (`turn.steer_flushed`) for exactly
    /// this count, and only then appends the steers into context memory —
    /// a steer pushed between the take and the append lands in the next
    /// flush instead of being miscounted.
    pub fn take_items(&mut self) -> Vec<BufferedSteer> {
        std::mem::take(&mut self.items)
    }

    /// Drop every buffered steer (a resume rebuilds the buffer from the
    /// wire log's pending steer state; the clear makes the restore
    /// idempotent when the pre-close buffer still holds its copies).
    pub fn clear(&mut self) {
        self.items.clear();
    }

    /// Whether the buffer is empty.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Number of buffered steers.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Read-only view of the parked steers (the notification driver's
    /// dedup scan: a parked notification counts as scheduled, not
    /// undelivered — SDR-B2-01's ledger input).
    pub fn iter(&self) -> std::slice::Iter<'_, BufferedSteer> {
        self.items.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_become_non_empty_when_steer_is_pushed() {
        let mut buf = SteerBuffer::new();
        assert!(buf.is_empty());
        assert_eq!(buf.len(), 0);

        buf.push(
            vec![ContentPart::Text {
                text: "steer 1".to_string(),
            }],
            PromptOrigin::User,
        );
        assert!(!buf.is_empty());
        assert_eq!(buf.len(), 1);
    }

    #[test]
    fn should_return_false_when_flushing_empty_buffer() {
        let mut buf = SteerBuffer::new();
        let mut memory = ContextMemory::new();
        assert!(!buf.flush(&mut memory));
    }

    #[test]
    fn should_return_true_and_clear_buffer_when_flushing_non_empty_buffer() {
        let mut buf = SteerBuffer::new();
        buf.push(
            vec![ContentPart::Text {
                text: "steer".to_string(),
            }],
            PromptOrigin::User,
        );
        let mut memory = ContextMemory::new();
        assert!(buf.flush(&mut memory));
        assert!(buf.is_empty());
    }

    #[test]
    fn should_flush_all_steers_when_multiple_are_buffered() {
        let mut buf = SteerBuffer::new();
        buf.push(
            vec![ContentPart::Text {
                text: "first".to_string(),
            }],
            PromptOrigin::User,
        );
        buf.push(
            vec![ContentPart::Text {
                text: "second".to_string(),
            }],
            PromptOrigin::User,
        );
        let mut memory = ContextMemory::new();
        assert!(buf.flush(&mut memory));
        assert_eq!(buf.len(), 0);

        // Two user messages should be in history.
        let history = memory.history();
        let user_count = history
            .iter()
            .filter(|m| m.message.role == crate::message::Role::User)
            .count();
        assert_eq!(user_count, 2);
    }

    #[test]
    fn should_preserve_items_when_buffer_is_cloned() {
        let mut buf = SteerBuffer::new();
        buf.push(
            vec![ContentPart::Text {
                text: "steer".to_string(),
            }],
            PromptOrigin::User,
        );
        let cloned = buf.clone();
        assert_eq!(cloned.len(), 1);
        assert_eq!(buf.len(), 1);
    }

    #[test]
    fn should_take_all_items_and_empty_the_buffer_when_take_items_called() {
        // Given a buffer with two steers
        let mut buf = SteerBuffer::new();
        buf.push(
            vec![ContentPart::Text {
                text: "first".to_string(),
            }],
            PromptOrigin::User,
        );
        buf.push(
            vec![ContentPart::Text {
                text: "second".to_string(),
            }],
            PromptOrigin::User,
        );

        // When the flush path takes the items
        let taken = buf.take_items();

        // Then the taken snapshot carries both steers in arrival order and
        // the buffer is empty (a push after the take lands in the next
        // flush, never the taken snapshot)
        assert_eq!(taken.len(), 2);
        assert!(matches!(
            &taken[0].content[0],
            ContentPart::Text { text } if text == "first"
        ));
        assert!(matches!(
            &taken[1].content[0],
            ContentPart::Text { text } if text == "second"
        ));
        assert!(buf.is_empty());
    }

    #[test]
    fn should_drop_every_buffered_steer_when_clear_called() {
        // Given a buffer with a parked steer
        let mut buf = SteerBuffer::new();
        buf.push(
            vec![ContentPart::Text {
                text: "stale".to_string(),
            }],
            PromptOrigin::User,
        );

        // When the resume restore clears the buffer before re-parking the
        // replayed pending steers
        buf.clear();

        // Then the buffer is empty (no duplicated residency)
        assert!(buf.is_empty());
    }

    #[test]
    fn should_expose_the_parked_origins_when_iterating_the_buffer() {
        // Given a buffer with a user steer and a parked notification
        let mut buf = SteerBuffer::new();
        buf.push(
            vec![ContentPart::Text {
                text: "hi".to_string(),
            }],
            PromptOrigin::User,
        );
        buf.push(
            vec![ContentPart::Text {
                text: "n".to_string(),
            }],
            PromptOrigin::BackgroundTask {
                task_id: "t".into(),
                status: "completed".into(),
                notification_id: "task:t:completed".into(),
            },
        );

        // When the notification driver scans the parked entries (a parked
        // notification counts as scheduled, not undelivered)
        let parked: Vec<bool> = buf
            .iter()
            .map(|s| matches!(s.origin, PromptOrigin::BackgroundTask { .. }))
            .collect();

        // Then the read-only view exposes each steer's origin in FIFO order
        assert_eq!(parked, vec![false, true]);
    }
}
