//! Prompt lifecycle queue.
//!
//! Mirrors the TypeScript `prompt` domain. A [`PromptQueue`] manages the
//! lifecycle of prompts submitted to an agent: they move through `Pending`
//! → `Running` → a terminal state (`Completed`, `Failed`, `Cancelled`,
//! `Blocked`). Steering and injection allow out-of-band interaction with the
//! queue.

use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::context::ContextMessage;

// ---------------------------------------------------------------------------
// PromptState
// ---------------------------------------------------------------------------

/// The lifecycle state of a prompt in the queue.
///
/// Serialized as snake_case to match the TypeScript wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptState {
    /// Waiting in the queue to be picked up.
    Pending,
    /// Currently being processed by the agent.
    Running,
    /// Redirected by a steer operation (will be re-evaluated).
    Steered,
    /// Finished successfully.
    Completed,
    /// Finished with an error.
    Failed,
    /// Cancelled by the user or system.
    Cancelled,
    /// Blocked waiting for an external resolution (e.g. permission).
    Blocked,
}

impl PromptState {
    /// The canonical wire representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Steered => "steered",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Blocked => "blocked",
        }
    }

    /// Whether this state is terminal (the prompt will not transition further).
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}

impl fmt::Display for PromptState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// PromptCompletionState
// ---------------------------------------------------------------------------

/// The terminal state reported when a prompt completes.
///
/// A subset of [`PromptState`] representing only the states that a prompt
/// can end in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptCompletionState {
    Completed,
    Failed,
    Cancelled,
    Blocked,
}

impl PromptCompletionState {
    /// Convert to the corresponding [`PromptState`].
    pub fn to_prompt_state(self) -> PromptState {
        match self {
            Self::Completed => PromptState::Completed,
            Self::Failed => PromptState::Failed,
            Self::Cancelled => PromptState::Cancelled,
            Self::Blocked => PromptState::Blocked,
        }
    }
}

/// A completion record emitted when the active prompt finishes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptCompletion {
    pub prompt_id: String,
    pub state: PromptCompletionState,
}

// ---------------------------------------------------------------------------
// PromptInput
// ---------------------------------------------------------------------------

/// Input submitted to the prompt queue.
///
/// The `materialize` closure, if present, is invoked at enqueue time to
/// produce the final [`ContextMessage`] stored in the snapshot. This allows
/// lazy generation of the message content (e.g. resolving a skill reference
/// to its full prompt) without forcing the caller to build it eagerly.
pub struct PromptInput {
    /// Optional explicit ID; if `None` the queue generates one.
    pub id: Option<String>,
    /// The message to store in the snapshot (used when `materialize` is
    /// `None`).
    pub message: ContextMessage,
    /// Optional lazy message generator. If present, the result replaces
    /// `message` in the snapshot.
    pub materialize: Option<Box<dyn FnOnce() -> ContextMessage + Send>>,
}

impl PromptInput {
    /// Create a simple input with just a message (no explicit ID, no
    /// materializer).
    pub fn new(message: ContextMessage) -> Self {
        Self {
            id: None,
            message,
            materialize: None,
        }
    }

    /// Set an explicit prompt ID.
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// Set a lazy materializer.
    pub fn with_materialize(
        mut self,
        materialize: Box<dyn FnOnce() -> ContextMessage + Send>,
    ) -> Self {
        self.materialize = Some(materialize);
        self
    }

    /// Consume the input, calling `materialize` if present to produce the
    /// final message.
    fn into_message(self) -> ContextMessage {
        match self.materialize {
            Some(materialize_fn) => materialize_fn(),
            None => self.message,
        }
    }
}

impl fmt::Debug for PromptInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PromptInput")
            .field("id", &self.id)
            .field("message", &self.message)
            .field(
                "materialize",
                &self.materialize.as_ref().map(|_| "<closure>"),
            )
            .finish()
    }
}

// ---------------------------------------------------------------------------
// PromptSnapshot
// ---------------------------------------------------------------------------

/// An immutable snapshot of a prompt at a point in time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptSnapshot {
    /// Unique prompt identifier (e.g. `"prompt-1"`).
    pub id: String,
    /// Identifier of the user message associated with this prompt.
    pub user_message_id: String,
    /// Creation timestamp (milliseconds since Unix epoch).
    pub created_at: u64,
    /// Current lifecycle state.
    pub state: PromptState,
    /// The message payload.
    pub message: ContextMessage,
}

impl PromptSnapshot {
    /// Whether this snapshot is in a terminal state.
    pub fn is_terminal(&self) -> bool {
        self.state.is_terminal()
    }
}

// ---------------------------------------------------------------------------
// PromptQueueSnapshot
// ---------------------------------------------------------------------------

/// A read-only view of the current queue state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptQueueSnapshot {
    /// The currently active prompt, if any.
    pub active: Option<PromptSnapshot>,
    /// Pending prompts in FIFO order.
    pub pending: Vec<PromptSnapshot>,
}

// ---------------------------------------------------------------------------
// PromptQueue
// ---------------------------------------------------------------------------

/// The prompt lifecycle queue.
///
/// At most one prompt is `active` (currently being processed) at a time.
/// Pending prompts are processed in FIFO order via [`next`](Self::next).
pub struct PromptQueue {
    /// The currently active prompt (state `Running`).
    pub active: Option<PromptSnapshot>,
    /// Pending prompts in FIFO order.
    pub pending: Vec<PromptSnapshot>,
    /// Monotonic counter for ID generation.
    pub counter: u64,
}

impl PromptQueue {
    /// Create an empty queue.
    pub fn new() -> Self {
        Self {
            active: None,
            pending: Vec::new(),
            counter: 0,
        }
    }

    /// Generate a unique prompt ID (`"prompt-{counter}"`).
    pub fn generate_id(&mut self) -> String {
        self.counter += 1;
        format!("prompt-{}", self.counter)
    }

    /// Current wall-clock timestamp in milliseconds since the Unix epoch.
    fn now_millis() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// Enqueue a new prompt.
    ///
    /// If `input.id` is `None` a new ID is generated. The `materialize`
    /// closure (if present) is called to produce the final message. The
    /// snapshot is added to `pending` with state `Pending` and returned.
    pub fn enqueue(&mut self, mut input: PromptInput) -> PromptSnapshot {
        let id = input.id.take().unwrap_or_else(|| self.generate_id());
        let message = input.into_message();
        let created_at = Self::now_millis();

        let snapshot = PromptSnapshot {
            id: id.clone(),
            user_message_id: id,
            created_at,
            state: PromptState::Pending,
            message,
        };

        self.pending.push(snapshot.clone());
        snapshot
    }

    /// Snapshot the current queue state.
    pub fn list(&self) -> PromptQueueSnapshot {
        PromptQueueSnapshot {
            active: self.active.clone(),
            pending: self.pending.clone(),
        }
    }

    /// Pop the head of `pending` and set it as `active` with state `Running`.
    ///
    /// Returns `None` if the queue is empty or a prompt is already active.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<PromptSnapshot> {
        if self.active.is_some() {
            return None;
        }
        let mut snapshot = self.pending.first()?.clone();
        self.pending.remove(0);
        snapshot.state = PromptState::Running;
        self.active = Some(snapshot.clone());
        Some(snapshot)
    }

    /// Mark the active prompt as completed with the given terminal state.
    ///
    /// The active slot is cleared so [`next`](Self::next) can pick up the
    /// next pending prompt. Does nothing if there is no active prompt.
    pub fn complete_active(&mut self, state: PromptCompletionState) {
        if let Some(mut active) = self.active.take() {
            active.state = state.to_prompt_state();
        }
    }

    /// Abort a prompt by ID.
    ///
    /// If the ID matches a pending prompt it is removed from the queue. If it
    /// matches the active prompt the active is marked as `Cancelled` and
    /// cleared. Returns `true` if a prompt was found and aborted.
    pub fn abort(&mut self, prompt_id: &str) -> bool {
        // Check pending first.
        if let Some(pos) = self.pending.iter().position(|s| s.id == prompt_id) {
            self.pending.remove(pos);
            return true;
        }
        // Check active.
        if let Some(active) = &self.active
            && active.id == prompt_id
        {
            self.complete_active(PromptCompletionState::Cancelled);
            return true;
        }
        false
    }

    /// Mark prompts as steered.
    ///
    /// For each ID in `prompt_ids`, if the prompt is found in `pending` or is
    /// the active prompt, its state is set to `Steered`. Returns the
    /// snapshots of the prompts that were steered.
    pub fn steer(&mut self, prompt_ids: &[String]) -> Vec<PromptSnapshot> {
        let mut steered = Vec::new();
        for id in prompt_ids {
            // Check pending.
            if let Some(pos) = self.pending.iter().position(|s| &s.id == id) {
                self.pending[pos].state = PromptState::Steered;
                steered.push(self.pending[pos].clone());
                continue;
            }
            // Check active.
            if let Some(active) = &mut self.active
                && active.id == *id
            {
                active.state = PromptState::Steered;
                steered.push(active.clone());
            }
        }
        steered
    }

    /// Clear all pending and active prompts.
    pub fn clear(&mut self) {
        self.pending.clear();
        self.active = None;
    }

    /// Inject a high-priority prompt that bypasses the normal queue.
    ///
    /// The prompt is inserted at the front of `pending` so it will be the
    /// next prompt picked up by [`next`](Self::next).
    pub fn inject(&mut self, message: ContextMessage) -> PromptSnapshot {
        let id = self.generate_id();
        let created_at = Self::now_millis();

        let snapshot = PromptSnapshot {
            id: id.clone(),
            user_message_id: id,
            created_at,
            state: PromptState::Pending,
            message,
        };

        self.pending.insert(0, snapshot.clone());
        snapshot
    }
}

impl Default for PromptQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for PromptQueue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PromptQueue")
            .field("active", &self.active.is_some())
            .field("pending_count", &self.pending.len())
            .field("counter", &self.counter)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Placeholder context for system prompt templates
// ---------------------------------------------------------------------------

/// Context values for `{{placeholder}}` substitution in system prompt templates.
///
/// Mirrors the TypeScript `PlaceholderContext`. Only `workspace`, `platform`,
/// and `date` are supported. Unknown tokens are left verbatim. Expansion is a
/// single pass — values containing `{{...}}` are emitted as-is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlaceholderContext {
    /// The current workspace root path.
    pub workspace: Option<String>,
    /// The current platform identifier (`"macos"`, `"linux"`, `"windows"`).
    pub platform: Option<String>,
    /// The current date in `YYYY-MM-DD` format.
    pub date: Option<String>,
}

/// Replace `{{workspace}}` / `{{platform}}` / `{{date}}` placeholders in a
/// system prompt template.
///
/// Single-pass: substituted values are not re-scanned. Unknown tokens and
/// unclosed `{{` sequences are left verbatim.
pub fn replace_placeholders(template: &str, ctx: &PlaceholderContext) -> String {
    let mut result = String::with_capacity(template.len());
    let mut rest = template;

    while let Some(open) = rest.find("{{") {
        result.push_str(&rest[..open]);

        let after_open = &rest[open + 2..];
        if let Some(close) = after_open.find("}}") {
            let key = after_open[..close].trim();
            let value = match key {
                "workspace" => ctx.workspace.as_deref(),
                "platform" => ctx.platform.as_deref(),
                "date" => ctx.date.as_deref(),
                _ => None,
            };
            match value {
                Some(v) => result.push_str(v),
                None => {
                    result.push_str("{{");
                    result.push_str(after_open[..close + 2].trim_end());
                }
            }
            rest = &after_open[close + 2..];
        } else {
            // Unclosed {{ — emit the remaining text verbatim (including {{)
            result.push_str(&rest[open..]);
            return result;
        }
    }

    result.push_str(rest);
    result
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Message;

    /// Helper: create a simple user ContextMessage.
    fn user_msg(text: &str) -> ContextMessage {
        ContextMessage::from_message(Message::user(text))
    }

    // -- PromptState ---------------------------------------------------------

    #[test]
    fn should_use_snake_case_when_prompt_state_is_serialized() {
        let json = serde_json::to_string(&PromptState::Running).unwrap();
        assert_eq!(json, "\"running\"");

        let json = serde_json::to_string(&PromptState::Cancelled).unwrap();
        assert_eq!(json, "\"cancelled\"");
    }

    #[test]
    fn should_use_snake_case_when_prompt_state_is_displayed() {
        assert_eq!(PromptState::Pending.to_string(), "pending");
        assert_eq!(PromptState::Completed.to_string(), "completed");
        assert_eq!(PromptState::Blocked.to_string(), "blocked");
    }

    #[test]
    fn should_return_true_for_terminal_states_when_is_terminal_is_checked() {
        assert!(PromptState::Completed.is_terminal());
        assert!(PromptState::Failed.is_terminal());
        assert!(PromptState::Cancelled.is_terminal());
        assert!(!PromptState::Pending.is_terminal());
        assert!(!PromptState::Running.is_terminal());
        assert!(!PromptState::Blocked.is_terminal());
    }

    // -- PromptQueue::enqueue / generate_id ---------------------------------

    #[test]
    fn should_enqueue_prompt_as_pending_when_queue_is_new() {
        let mut queue = PromptQueue::new();
        let snapshot = queue.enqueue(PromptInput::new(user_msg("hello")));

        assert_eq!(snapshot.state, PromptState::Pending);
        assert_eq!(snapshot.id, "prompt-1");
        assert_eq!(queue.pending.len(), 1);
        assert!(queue.active.is_none());
    }

    #[test]
    fn should_generate_unique_ids_when_enqueueing_multiple_prompts() {
        let mut queue = PromptQueue::new();
        let s1 = queue.enqueue(PromptInput::new(user_msg("one")));
        let s2 = queue.enqueue(PromptInput::new(user_msg("two")));
        let s3 = queue.enqueue(PromptInput::new(user_msg("three")));

        assert_eq!(s1.id, "prompt-1");
        assert_eq!(s2.id, "prompt-2");
        assert_eq!(s3.id, "prompt-3");
    }

    #[test]
    fn should_use_provided_id_when_enqueueing_input_with_explicit_id() {
        let mut queue = PromptQueue::new();
        let snapshot = queue.enqueue(PromptInput::new(user_msg("custom")).with_id("my-prompt-42"));
        assert_eq!(snapshot.id, "my-prompt-42");
    }

    // -- PromptQueue::next ---------------------------------------------------

    #[test]
    fn should_promote_pending_prompt_to_running_when_next_is_called() {
        let mut queue = PromptQueue::new();
        queue.enqueue(PromptInput::new(user_msg("first")));
        queue.enqueue(PromptInput::new(user_msg("second")));

        let active = queue.next().expect("should return first prompt");
        assert_eq!(active.id, "prompt-1");
        assert_eq!(active.state, PromptState::Running);
        assert!(queue.active.is_some());
        assert_eq!(queue.pending.len(), 1);
    }

    #[test]
    fn should_return_none_when_next_is_called_while_prompt_is_active() {
        let mut queue = PromptQueue::new();
        queue.enqueue(PromptInput::new(user_msg("only")));
        queue.next();

        let result = queue.next();
        assert!(result.is_none(), "should not return while active is set");
    }

    #[test]
    fn should_return_none_when_next_is_called_on_empty_queue() {
        let mut queue = PromptQueue::new();
        assert!(queue.next().is_none());
    }

    // -- PromptQueue::complete_active ---------------------------------------

    #[test]
    fn should_clear_active_slot_when_completing_a_prompt() {
        let mut queue = PromptQueue::new();
        queue.enqueue(PromptInput::new(user_msg("task")));
        queue.next();

        queue.complete_active(PromptCompletionState::Completed);
        assert!(
            queue.active.is_none(),
            "active should be cleared after completion"
        );
    }

    #[test]
    fn should_pick_up_next_pending_prompt_when_active_is_completed() {
        let mut queue = PromptQueue::new();
        queue.enqueue(PromptInput::new(user_msg("first")));
        queue.enqueue(PromptInput::new(user_msg("second")));

        queue.next();
        queue.complete_active(PromptCompletionState::Completed);

        let second = queue.next().expect("should pick up second prompt");
        assert_eq!(second.id, "prompt-2");
    }

    // -- PromptQueue::abort --------------------------------------------------

    #[test]
    fn should_remove_pending_prompt_from_queue_when_aborting_it() {
        let mut queue = PromptQueue::new();
        queue.enqueue(PromptInput::new(user_msg("first")));
        queue.enqueue(PromptInput::new(user_msg("second")));

        let aborted = queue.abort("prompt-1");
        assert!(aborted);
        assert_eq!(queue.pending.len(), 1);
        assert_eq!(queue.pending[0].id, "prompt-2");
    }

    #[test]
    fn should_cancel_and_clear_active_prompt_when_aborting_it() {
        let mut queue = PromptQueue::new();
        queue.enqueue(PromptInput::new(user_msg("active")));
        queue.next();

        let aborted = queue.abort("prompt-1");
        assert!(aborted);
        assert!(
            queue.active.is_none(),
            "active should be cleared after abort"
        );
    }

    #[test]
    fn should_return_false_when_aborting_nonexistent_prompt() {
        let mut queue = PromptQueue::new();
        queue.enqueue(PromptInput::new(user_msg("only")));

        let aborted = queue.abort("nonexistent");
        assert!(!aborted);
        assert_eq!(queue.pending.len(), 1);
    }

    // -- PromptQueue::steer --------------------------------------------------

    #[test]
    fn should_mark_pending_prompts_as_steered_when_steer_is_called() {
        let mut queue = PromptQueue::new();
        queue.enqueue(PromptInput::new(user_msg("first")));
        queue.enqueue(PromptInput::new(user_msg("second")));
        queue.enqueue(PromptInput::new(user_msg("third")));

        let steered = queue.steer(&["prompt-1".to_string(), "prompt-3".to_string()]);

        assert_eq!(steered.len(), 2);
        assert!(steered.iter().all(|s| s.state == PromptState::Steered));
        assert_eq!(queue.pending[0].state, PromptState::Steered);
        assert_eq!(queue.pending[1].state, PromptState::Pending);
        assert_eq!(queue.pending[2].state, PromptState::Steered);
    }

    #[test]
    fn should_mark_active_prompt_as_steered_when_steering_it() {
        let mut queue = PromptQueue::new();
        queue.enqueue(PromptInput::new(user_msg("active")));
        queue.next();

        let steered = queue.steer(&["prompt-1".to_string()]);
        assert_eq!(steered.len(), 1);
        assert_eq!(queue.active.as_ref().unwrap().state, PromptState::Steered);
    }

    // -- PromptQueue::clear --------------------------------------------------

    #[test]
    fn should_become_empty_when_clearing_populated_queue() {
        let mut queue = PromptQueue::new();
        queue.enqueue(PromptInput::new(user_msg("first")));
        queue.enqueue(PromptInput::new(user_msg("second")));
        queue.next();

        queue.clear();
        assert!(queue.pending.is_empty());
        assert!(queue.active.is_none());
    }

    // -- PromptQueue::inject -------------------------------------------------

    #[test]
    fn should_insert_injected_prompt_at_front_when_injecting() {
        let mut queue = PromptQueue::new();
        queue.enqueue(PromptInput::new(user_msg("normal")));

        let injected = queue.inject(user_msg("urgent"));
        assert_eq!(injected.state, PromptState::Pending);
        assert_eq!(queue.pending[0].id, injected.id);
        assert_eq!(queue.pending[1].id, "prompt-1");
    }

    #[test]
    fn should_pick_up_injected_prompt_first_when_next_is_called() {
        let mut queue = PromptQueue::new();
        queue.enqueue(PromptInput::new(user_msg("normal")));
        let injected = queue.inject(user_msg("urgent"));

        let active = queue.next().expect("should return injected prompt");
        assert_eq!(active.id, injected.id);
    }

    // -- PromptQueue::list ---------------------------------------------------

    #[test]
    fn should_return_queue_snapshot_when_listing_populated_queue() {
        let mut queue = PromptQueue::new();
        queue.enqueue(PromptInput::new(user_msg("first")));
        queue.enqueue(PromptInput::new(user_msg("second")));
        queue.next();

        let snapshot = queue.list();
        assert!(snapshot.active.is_some());
        assert_eq!(snapshot.pending.len(), 1);
    }

    // -- PromptInput::materialize -------------------------------------------

    #[test]
    fn should_generate_message_via_materialize_closure_when_enqueueing() {
        let mut queue = PromptQueue::new();
        let input = PromptInput::new(user_msg("placeholder"))
            .with_materialize(Box::new(|| user_msg("materialized content")));

        let snapshot = queue.enqueue(input);
        let text = snapshot.message.message.text_content();
        assert_eq!(text, "materialized content");
    }

    // -- PromptCompletionState ----------------------------------------------

    #[test]
    fn should_map_completion_state_to_prompt_state_when_converted() {
        assert_eq!(
            PromptCompletionState::Completed.to_prompt_state(),
            PromptState::Completed
        );
        assert_eq!(
            PromptCompletionState::Blocked.to_prompt_state(),
            PromptState::Blocked
        );
    }
}
