//! Conversation undo service — availability computation and execution.
//!
//! Ports the TypeScript undo domain. The service tracks whether the
//! conversation is idle (no turn in progress) and exposes:
//!
//! - [`ConversationUndoService::availability`] — how many turns can be undone
//!   right now, and whether a compaction boundary limits the range.
//! - [`ConversationUndoService::undo`] — execute up to N undos, gated on the
//!   idle flag.
//!
//! The actual history mutation is delegated to [`ContextMemory::undo_count`].

use crate::context::ContextMessage;
use crate::context_memory::ContextMemory;
use crate::message::Role;
use crate::origin::PromptOrigin;

/// Default maximum number of undoable turns.
const DEFAULT_MAX_UNDO_TURNS: usize = 50;

/// Snapshot of how many turns can be undone from the current history tail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoAvailability {
    /// Maximum number of turns undoable from the current history tail,
    /// clamped to the service's `max_undo_turns` limit.
    pub max_turns: usize,
    /// `true` when a compaction summary boundary was encountered while
    /// counting undoable turns. This means older turns exist behind the
    /// boundary and cannot be reached by undo.
    pub stopped_at_compaction: bool,
}

/// Result of an [`ConversationUndoService::undo`] call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoResult {
    /// How many turns were actually undone (0 if not idle or nothing to undo).
    pub turns_undone: usize,
    /// Whether a compaction boundary limits the remaining undo range.
    pub stopped_at_compaction: bool,
}

/// Conversation undo service.
///
/// Gates undo on an idle flag so that undo cannot race with an in-flight
/// turn. Delegates the actual history mutation to [`ContextMemory`].
pub struct ConversationUndoService {
    /// Whether the conversation is idle (no turn in progress).
    is_idle: bool,
    /// Hard cap on how many turns can be undone across the lifetime of the
    /// service, applied to each [`availability`](Self::availability) query.
    max_undo_turns: usize,
}

impl Default for ConversationUndoService {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_UNDO_TURNS)
    }
}

impl ConversationUndoService {
    /// Create a new service with the given maximum undo turns.
    pub fn new(max_undo_turns: usize) -> Self {
        Self {
            is_idle: true,
            max_undo_turns,
        }
    }

    /// Compute how many turns can be undone from the current context memory.
    ///
    /// Walks backward through the history, skipping injection-origin messages,
    /// stopping at a compaction summary boundary. Counts undo anchors (real
    /// user input messages) and clamps the result to `max_undo_turns`.
    pub fn availability(&self, context_memory: &ContextMemory) -> UndoAvailability {
        let history = context_memory.history();
        let mut count = 0usize;
        let mut stopped_at_compaction = false;

        for message in history.iter().rev() {
            if let Some(ref origin) = message.origin {
                if *origin == PromptOrigin::Injection {
                    continue;
                }
                if *origin == PromptOrigin::CompactionSummary {
                    stopped_at_compaction = true;
                    break;
                }
            }

            if is_undo_anchor(message) {
                count += 1;
            }
        }

        let max_turns = count.min(self.max_undo_turns);

        UndoAvailability {
            max_turns,
            stopped_at_compaction,
        }
    }

    /// Undo up to `turns` turns from the context memory.
    ///
    /// - If the service is **not idle**, returns 0 turns undone immediately.
    /// - If `turns` exceeds [`availability`](Self::availability), only the
    ///   available turns are undone.
    /// - If `turns` is 0 or no turns are available, this is a no-op.
    ///
    /// Returns the actual number of turns undone and whether a compaction
    /// boundary limits the remaining undo range.
    pub fn undo(&mut self, context_memory: &mut ContextMemory, turns: usize) -> UndoResult {
        if !self.is_idle {
            return UndoResult {
                turns_undone: 0,
                stopped_at_compaction: false,
            };
        }

        let availability = self.availability(context_memory);
        let to_undo = turns.min(availability.max_turns);

        if to_undo == 0 {
            return UndoResult {
                turns_undone: 0,
                stopped_at_compaction: availability.stopped_at_compaction,
            };
        }

        match context_memory.undo_count(to_undo) {
            Ok(()) => UndoResult {
                turns_undone: to_undo,
                stopped_at_compaction: availability.stopped_at_compaction,
            },
            Err(_) => UndoResult {
                turns_undone: 0,
                stopped_at_compaction: availability.stopped_at_compaction,
            },
        }
    }

    /// Set whether the conversation is idle.
    ///
    /// When `false`, [`undo`](Self::undo) is a no-op. Callers should set
    /// this to `false` before starting a turn and `true` when the turn
    /// completes.
    pub fn set_idle(&mut self, idle: bool) {
        self.is_idle = idle;
    }

    /// Whether the conversation is idle (undo is permitted).
    pub fn is_idle(&self) -> bool {
        self.is_idle
    }
}

/// Whether a message is an undo anchor (real user input).
///
/// Mirrors the private `is_undo_anchor` in `context_memory/mod.rs`: a message
/// with [`Role::User`] and either no origin or a real-user-input origin.
fn is_undo_anchor(message: &ContextMessage) -> bool {
    if message.message.role != Role::User {
        return false;
    }
    match &message.origin {
        None => true,
        Some(origin) => origin.is_real_user_input(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Message;

    /// Helper: create a user message with the given origin.
    fn user_msg(text: &str, origin: PromptOrigin) -> ContextMessage {
        ContextMessage {
            message: Message::user(text),
            origin: Some(origin),
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        }
    }

    /// Helper: create an assistant message (no origin).
    fn assistant_msg(text: &str) -> ContextMessage {
        ContextMessage {
            message: Message::assistant(text),
            origin: None,
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        }
    }

    /// Helper: create a compaction-summary user message.
    fn compaction_summary_msg(text: &str) -> ContextMessage {
        ContextMessage {
            message: Message::user(text),
            origin: Some(PromptOrigin::CompactionSummary),
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        }
    }

    /// Helper: create an injection (system-reminder) user message.
    fn injection_msg(text: &str) -> ContextMessage {
        ContextMessage {
            message: Message::user(text),
            origin: Some(PromptOrigin::Injection),
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        }
    }

    #[test]
    fn should_return_zero_availability_when_history_is_empty() {
        // Given an empty context memory
        let service = ConversationUndoService::default();
        let memory = ContextMemory::new();

        // When checking availability
        let availability = service.availability(&memory);

        // Then max_turns is 0 and no compaction boundary
        assert_eq!(availability.max_turns, 0);
        assert!(!availability.stopped_at_compaction);
    }

    #[test]
    fn should_count_undoable_turns_when_history_has_user_messages() {
        // Given a memory with two user-assistant exchanges
        let mut memory = ContextMemory::new();
        memory.append_message(user_msg("Hello", PromptOrigin::User));
        memory.append_message(assistant_msg("Hi there"));
        memory.append_message(user_msg("How are you?", PromptOrigin::User));
        memory.append_message(assistant_msg("I'm good"));

        // When checking availability
        let service = ConversationUndoService::default();
        let availability = service.availability(&memory);

        // Then two turns can be undone
        assert_eq!(availability.max_turns, 2);
        assert!(!availability.stopped_at_compaction);
    }

    #[test]
    fn should_stop_at_compaction_boundary_when_history_contains_compaction_summary() {
        // Given a memory with a compaction summary between user messages
        let mut memory = ContextMemory::new();
        memory.append_message(user_msg("Old message", PromptOrigin::User));
        memory.append_message(compaction_summary_msg("Compaction summary"));
        memory.append_message(user_msg("New message", PromptOrigin::User));

        // When checking availability
        let service = ConversationUndoService::default();
        let availability = service.availability(&memory);

        // Then only one turn can be undone (the one after compaction)
        assert_eq!(availability.max_turns, 1);
        assert!(availability.stopped_at_compaction);
    }

    #[test]
    fn should_skip_injection_messages_when_counting_availability() {
        // Given a memory with injection messages between user messages
        let mut memory = ContextMemory::new();
        memory.append_message(user_msg("Hello", PromptOrigin::User));
        memory.append_message(injection_msg("System reminder"));
        memory.append_message(user_msg("Second message", PromptOrigin::User));

        // When checking availability
        let service = ConversationUndoService::default();
        let availability = service.availability(&memory);

        // Then both user messages are counted (injection skipped)
        assert_eq!(availability.max_turns, 2);
        assert!(!availability.stopped_at_compaction);
    }

    #[test]
    fn should_limit_availability_to_max_undo_turns_when_history_exceeds_the_limit() {
        // Given a service with max_undo_turns = 1 and a memory with 3 user msgs
        let mut memory = ContextMemory::new();
        memory.append_message(user_msg("First", PromptOrigin::User));
        memory.append_message(user_msg("Second", PromptOrigin::User));
        memory.append_message(user_msg("Third", PromptOrigin::User));

        // When checking availability with max_undo_turns = 1
        let service = ConversationUndoService::new(1);
        let availability = service.availability(&memory);

        // Then availability is capped at 1
        assert_eq!(availability.max_turns, 1);
        assert!(!availability.stopped_at_compaction);
    }

    #[test]
    fn should_return_zero_turns_undone_when_not_idle() {
        // Given a non-idle service with one available turn
        let mut memory = ContextMemory::new();
        memory.append_message(user_msg("Hello", PromptOrigin::User));
        let mut service = ConversationUndoService::default();
        service.set_idle(false);

        // When undoing 1 turn
        let result = service.undo(&mut memory, 1);

        // Then zero turns are undone and history is unchanged
        assert_eq!(result.turns_undone, 0);
        assert_eq!(memory.history().len(), 1);
    }

    #[test]
    fn should_undo_requested_turns_when_idle() {
        // Given an idle service with two user-assistant exchanges
        let mut memory = ContextMemory::new();
        memory.append_message(user_msg("First", PromptOrigin::User));
        memory.append_message(assistant_msg("Response 1"));
        memory.append_message(user_msg("Second", PromptOrigin::User));
        memory.append_message(assistant_msg("Response 2"));
        let mut service = ConversationUndoService::default();

        // When undoing 1 turn
        let result = service.undo(&mut memory, 1);

        // Then one turn is undone (second user message and response removed)
        assert_eq!(result.turns_undone, 1);
        assert_eq!(memory.history().len(), 2);
    }

    #[test]
    fn should_undo_fewer_turns_when_request_exceeds_availability() {
        // Given an idle service with only 1 available turn
        let mut memory = ContextMemory::new();
        memory.append_message(user_msg("Only message", PromptOrigin::User));
        let mut service = ConversationUndoService::default();

        // When undoing 5 turns
        let result = service.undo(&mut memory, 5);

        // Then only 1 turn is undone
        assert_eq!(result.turns_undone, 1);
        assert!(memory.history().is_empty());
    }

    #[test]
    fn should_undo_nothing_when_request_is_zero() {
        // Given an idle service with available turns
        let mut memory = ContextMemory::new();
        memory.append_message(user_msg("Hello", PromptOrigin::User));
        let mut service = ConversationUndoService::default();

        // When undoing 0 turns
        let result = service.undo(&mut memory, 0);

        // Then nothing is undone
        assert_eq!(result.turns_undone, 0);
        assert_eq!(memory.history().len(), 1);
    }

    #[test]
    fn should_set_and_get_idle_state_when_toggled() {
        // Given a new service (idle by default)
        let mut service = ConversationUndoService::default();

        // When toggling idle state
        assert!(service.is_idle());
        service.set_idle(false);
        assert!(!service.is_idle());
        service.set_idle(true);

        // Then idle is true again
        assert!(service.is_idle());
    }
}
