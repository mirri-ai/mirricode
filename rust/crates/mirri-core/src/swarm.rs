//! Swarm mode service.
//!
//! Ports the TypeScript `swarm` domain. [`SwarmModeService`] tracks whether
//! the agent is currently in *swarm mode* — a state in which the agent is
//! expected to use `AgentSwarm` to parallelise work across multiple subagents.
//!
//! The service is intentionally lightweight: it holds the active flag, the
//! trigger that caused entry, and a `reminder_injected` flag that the host
//! uses to avoid re-injecting the same system-reminder twice. The actual
//! permission enforcement (allowing `AgentSwarm` only when swarm mode is
//! active) lives in [`crate::permission`].
//!
//! ## Lifecycle
//!
//! 1. The host calls [`SwarmModeService::enter`] (or
//!    [`SwarmModeService::restore_enter`] when resuming a session) with a
//!    [`SwarmModeTrigger`].
//! 2. Before each turn, the host checks
//!    [`SwarmModeService::should_inject_reminder`]; if `true`, it injects the
//!    text from [`SwarmModeService::build_reminder_text`] and calls
//!    [`SwarmModeService::mark_reminder_injected`].
//! 3. When the agent or the host decides to leave swarm mode,
//!    [`SwarmModeService::exit`] clears the state and the host may inject the
//!    text from [`SwarmModeService::build_exit_reminder`].

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// SwarmModeTrigger
// ---------------------------------------------------------------------------

/// What caused the agent to enter swarm mode.
///
/// Serialized as snake_case to match the wire model used by the TypeScript
/// kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SwarmModeTrigger {
    /// The user or host explicitly requested swarm mode.
    Manual,
    /// A task-related condition triggered swarm mode (e.g. a large task was
    /// detected).
    Task,
    /// A tool-call-related condition triggered swarm mode.
    Tool,
}

impl std::fmt::Display for SwarmModeTrigger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Manual => write!(f, "manual"),
            Self::Task => write!(f, "task"),
            Self::Tool => write!(f, "tool"),
        }
    }
}

// ---------------------------------------------------------------------------
// Reminder texts (TS verbatim)
// ---------------------------------------------------------------------------

/// The enter reminder injected on manual/task-triggered swarm entry
/// (TS `enter-reminder.md`, verbatim baseline). Injected wrapped in a
/// `<system-reminder>` envelope via
/// [`ContextMemory::append_system_reminder`](crate::context_memory::ContextMemory::append_system_reminder).
pub const SWARM_ENTER_REMINDER: &str = r#"## Swarm Mode

You are now in "agent swarm" mode. The user may send tasks that require a large number of parallel subagents.

## Workflow

You do not need to use TodoList to record this workflow.

1. First, you may need to do a small amount of exploratory work before deciding how to divide the task across subagents. You may not need subagents during this exploratory phase.

2. After exploring, if you are convinced no subagent is needed to complete the task, tell the user why and wait for further instructions; otherwise, continue with the appropriate delegation.

3. Once you have enough context, do not handle the main work yourself. Use AgentSwarm with a `prompt_template` containing the `{{item}}` placeholder and an `items` array for the requested or appropriate number of subagents, partitioning the problem so each item gives one subagent a distinct part of the work. Pass `subagent_type` when the whole swarm should use a non-default subagent profile.

## Coordination

- Give each subagent a distinct scope of work.
- Avoid duplicating work across subagents.
- Avoid assigning conflicting changes or responsibilities to different subagents.
- Remember that subagents have your full capabilities. Do not overload their prompts with excessive detail; only describe the necessary background and each subagent's specific task.
- Unless the user explicitly specifies a lower limit, do not try to conserve the number of agents. AgentSwarm supports up to 128 subagents and queues launches automatically, so decompose work as finely as possible while keeping subagent responsibilities non-conflicting; combine tasks only when they are genuinely inseparable. If the subagents only need to read, inspect, or report back without making changes, their scopes may overlap slightly."#;

/// The exit reminder injected on manual/task-triggered swarm exit when the
/// enter reminder is no longer the newest message (TS `exit-reminder.md`,
/// verbatim baseline). Like the enter reminder it rides the
/// `<system-reminder>` envelope.
pub const SWARM_EXIT_REMINDER: &str = r#"## Swarm Mode Ended

Swarm Mode has ended. You are no longer required to follow the Swarm Mode workflow.

The user's next request is likely to be a regular request that does not need AgentSwarm. If the request still benefits from parallel subagents, you may call the AgentSwarm tool, but decide from the new request itself rather than the ended Swarm Mode workflow."#;

// ---------------------------------------------------------------------------
// SwarmModeService
// ---------------------------------------------------------------------------

/// Swarm mode state machine for a single agent.
///
/// Tracks the active flag, the trigger that caused entry, and whether the
/// system-reminder has already been injected. The host is responsible for
/// calling [`Self::should_inject_reminder`] before each turn and acting on the
/// result.
#[derive(Debug, Clone, Default)]
pub struct SwarmModeService {
    /// Whether swarm mode is currently active.
    active: bool,
    /// The trigger that caused entry, if active.
    trigger: Option<SwarmModeTrigger>,
    /// Whether the swarm-mode reminder has already been injected for this
    /// activation. Reset to `false` on every [`Self::enter`] /
    /// [`Self::restore_enter`].
    reminder_injected: bool,
}

impl SwarmModeService {
    /// Create a new service with swarm mode inactive.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether swarm mode is currently active.
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Enter swarm mode.
    ///
    /// Sets `active` to `true`, records `trigger`, and marks the reminder as
    /// not-yet-injected (i.e. pending) so the host will inject it before the
    /// next turn.
    pub fn enter(&mut self, trigger: SwarmModeTrigger) {
        self.active = true;
        self.trigger = Some(trigger);
        self.reminder_injected = false;
        tracing::debug!(trigger = %trigger, "swarm mode entered");
    }

    /// Restore swarm mode on session resume.
    ///
    /// Functionally identical to [`Self::enter`] but intended for the resume
    /// path where the host is rehydrating persisted state and does not want to
    /// dispatch "mode entered" events a second time.
    pub fn restore_enter(&mut self, trigger: SwarmModeTrigger) {
        self.active = true;
        self.trigger = Some(trigger);
        self.reminder_injected = false;
        tracing::debug!(trigger = %trigger, "swarm mode restored");
    }

    /// Exit swarm mode.
    ///
    /// Sets `active` to `false` and clears the trigger. The
    /// `reminder_injected` flag is also cleared so a subsequent
    /// [`Self::enter`] starts fresh.
    pub fn exit(&mut self) {
        self.active = false;
        self.trigger = None;
        self.reminder_injected = false;
        tracing::debug!("swarm mode exited");
    }

    /// The trigger that caused entry, if swarm mode is active.
    pub fn trigger(&self) -> Option<&SwarmModeTrigger> {
        self.trigger.as_ref()
    }

    /// Whether the host should inject the swarm-mode reminder before the next
    /// turn.
    ///
    /// Returns `true` only when swarm mode is active and the reminder has not
    /// yet been injected.
    pub fn should_inject_reminder(&self) -> bool {
        self.active && !self.reminder_injected
    }

    /// Mark the swarm-mode reminder as injected.
    ///
    /// After this call, [`Self::should_inject_reminder`] returns `false` until
    /// the next [`Self::enter`] / [`Self::restore_enter`].
    pub fn mark_reminder_injected(&mut self) {
        self.reminder_injected = true;
    }

    /// Build the system-reminder text telling the agent it is in swarm mode.
    ///
    /// Returns `Some(text)` when swarm mode is active and the reminder has not
    /// yet been injected; `None` otherwise. The text instructs the agent to
    /// use `AgentSwarm` for parallelisable work.
    pub fn build_reminder_text(&self) -> Option<String> {
        if !self.should_inject_reminder() {
            return None;
        }

        let trigger = self
            .trigger
            .map_or_else(|| "unknown".to_string(), |t| t.to_string());
        Some(format!(
            "You are currently in swarm mode (trigger: {trigger}). \
             When you have multiple independent tasks that can run in parallel, \
             use the AgentSwarm tool to dispatch them concurrently rather than \
             executing them sequentially. This improves throughput and reduces \
             wall-clock time. Exit swarm mode only when the remaining work is \
             strictly sequential or when all parallel tasks have completed."
        ))
    }

    /// Build the system-reminder text for when swarm mode exits.
    ///
    /// Returns `Some(text)` when swarm mode is not active (i.e. after
    /// [`Self::exit`] has been called); `None` when swarm mode is still
    /// active. The text informs the agent that it should return to normal
    /// single-agent operation.
    pub fn build_exit_reminder(&self) -> Option<String> {
        if self.active {
            return None;
        }

        Some(
            "Swarm mode has exited. \
             You are now operating in normal single-agent mode. \
             Do not use the AgentSwarm tool unless swarm mode is explicitly \
             re-entered. Continue with any remaining work sequentially."
                .to_string(),
        )
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // enter / is_active / trigger
    // -----------------------------------------------------------------------

    #[test]
    fn should_be_inactive_when_newly_created() {
        // Given a fresh service
        let service = SwarmModeService::new();

        // Then swarm mode is not active
        assert!(!service.is_active());
        assert!(service.trigger().is_none());
    }

    #[test]
    fn should_activate_when_entering_with_manual_trigger() {
        // Given a fresh service
        let mut service = SwarmModeService::new();

        // When entering with manual trigger
        service.enter(SwarmModeTrigger::Manual);

        // Then swarm mode is active with the correct trigger
        assert!(service.is_active());
        assert_eq!(service.trigger(), Some(&SwarmModeTrigger::Manual));
    }

    #[test]
    fn should_activate_when_entering_with_task_trigger() {
        // Given a fresh service
        let mut service = SwarmModeService::new();

        // When entering with task trigger
        service.enter(SwarmModeTrigger::Task);

        // Then swarm mode is active with the correct trigger
        assert!(service.is_active());
        assert_eq!(service.trigger(), Some(&SwarmModeTrigger::Task));
    }

    // -----------------------------------------------------------------------
    // exit
    // -----------------------------------------------------------------------

    #[test]
    fn should_deactivate_when_exiting() {
        // Given an active service
        let mut service = SwarmModeService::new();
        service.enter(SwarmModeTrigger::Manual);
        assert!(service.is_active());

        // When exiting
        service.exit();

        // Then swarm mode is inactive and trigger is cleared
        assert!(!service.is_active());
        assert!(service.trigger().is_none());
    }

    // -----------------------------------------------------------------------
    // restore_enter
    // -----------------------------------------------------------------------

    #[test]
    fn should_activate_when_restore_enter_called() {
        // Given a fresh service
        let mut service = SwarmModeService::new();

        // When restoring enter with tool trigger
        service.restore_enter(SwarmModeTrigger::Tool);

        // Then swarm mode is active with the correct trigger
        assert!(service.is_active());
        assert_eq!(service.trigger(), Some(&SwarmModeTrigger::Tool));
    }

    #[test]
    fn should_mark_reminder_pending_when_restore_enter_called() {
        // Given an active service where the reminder was already injected
        let mut service = SwarmModeService::new();
        service.enter(SwarmModeTrigger::Manual);
        service.mark_reminder_injected();
        assert!(!service.should_inject_reminder());

        // When restoring enter
        service.restore_enter(SwarmModeTrigger::Manual);

        // Then the reminder is pending again
        assert!(service.should_inject_reminder());
    }

    // -----------------------------------------------------------------------
    // should_inject_reminder / mark_reminder_injected
    // -----------------------------------------------------------------------

    #[test]
    fn should_inject_reminder_when_active_and_not_yet_injected() {
        // Given an active service
        let mut service = SwarmModeService::new();
        service.enter(SwarmModeTrigger::Manual);

        // Then the reminder should be injected
        assert!(service.should_inject_reminder());
    }

    #[test]
    fn should_not_inject_reminder_when_inactive() {
        // Given an inactive service
        let service = SwarmModeService::new();

        // Then the reminder should not be injected
        assert!(!service.should_inject_reminder());
    }

    #[test]
    fn should_not_inject_reminder_when_already_injected() {
        // Given an active service
        let mut service = SwarmModeService::new();
        service.enter(SwarmModeTrigger::Manual);

        // When the reminder is marked as injected
        service.mark_reminder_injected();

        // Then the reminder should not be injected again
        assert!(!service.should_inject_reminder());
    }

    // -----------------------------------------------------------------------
    // build_reminder_text
    // -----------------------------------------------------------------------

    #[test]
    fn should_return_reminder_text_when_active_and_not_injected() {
        // Given an active service
        let mut service = SwarmModeService::new();
        service.enter(SwarmModeTrigger::Manual);

        // When building the reminder text
        let text = service.build_reminder_text();

        // Then a non-empty string is returned mentioning AgentSwarm
        let text = text.expect("should return Some when active and not injected");
        assert!(text.contains("swarm mode"));
        assert!(text.contains("AgentSwarm"));
    }

    #[test]
    fn should_return_none_when_building_reminder_and_inactive() {
        // Given an inactive service
        let service = SwarmModeService::new();

        // When building the reminder text
        let text = service.build_reminder_text();

        // Then None is returned
        assert!(text.is_none());
    }

    // -----------------------------------------------------------------------
    // build_exit_reminder
    // -----------------------------------------------------------------------

    #[test]
    fn should_return_exit_reminder_when_not_active() {
        // Given an inactive service
        let service = SwarmModeService::new();

        // When building the exit reminder
        let text = service.build_exit_reminder();

        // Then a non-empty string is returned
        let text = text.expect("should return Some when not active");
        assert!(text.contains("exited"));
        assert!(text.contains("single-agent"));
    }

    #[test]
    fn should_return_none_when_building_exit_reminder_and_still_active() {
        // Given an active service
        let mut service = SwarmModeService::new();
        service.enter(SwarmModeTrigger::Manual);

        // When building the exit reminder
        let text = service.build_exit_reminder();

        // Then None is returned
        assert!(text.is_none());
    }

    // -----------------------------------------------------------------------
    // Display
    // -----------------------------------------------------------------------

    #[test]
    fn should_render_trigger_as_lowercase_string_when_displayed() {
        // Given triggers
        // When displaying them
        // Then they render as lowercase strings
        assert_eq!(SwarmModeTrigger::Manual.to_string(), "manual");
        assert_eq!(SwarmModeTrigger::Task.to_string(), "task");
        assert_eq!(SwarmModeTrigger::Tool.to_string(), "tool");
    }

    // -----------------------------------------------------------------------
    // Serde round-trip
    // -----------------------------------------------------------------------

    #[test]
    fn should_round_trip_when_serializing_and_deserializing_trigger() {
        // Given a trigger
        let trigger = SwarmModeTrigger::Tool;

        // When serializing and deserializing
        let json = serde_json::to_string(&trigger).unwrap();
        let deserialized: SwarmModeTrigger = serde_json::from_str(&json).unwrap();

        // Then the deserialized value equals the original
        assert_eq!(trigger, deserialized);
    }
}
