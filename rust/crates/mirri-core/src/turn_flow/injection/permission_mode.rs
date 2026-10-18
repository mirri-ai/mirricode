//! Permission-mode transition reminders (auto-mode enter/exit).

use super::*;

// ====================================================================
// PermissionModeInjector
// ====================================================================

/// Injects a reminder when the permission mode is or was `Auto`.
///
/// Tracks `last_mode` across steps. On enter/exit transitions involving
/// `Auto` mode, injects the enter or exit reminder. While `Auto` mode
/// remains active (no transition), no reminder is produced.
pub struct PermissionModeInjector {
    manager: Arc<PermissionManager>,
    last_mode: Mutex<Option<PermissionMode>>,
}

impl PermissionModeInjector {
    /// Create a permission-mode injector backed by the given manager.
    pub fn new(manager: Arc<PermissionManager>) -> Self {
        Self {
            manager,
            last_mode: Mutex::new(None),
        }
    }
}

const AUTO_MODE_ENTER_REMINDER: &str = "\
Auto permission mode is active. Tool approvals will be handled automatically while this mode remains enabled.
  - Continue normally without pausing for approval prompts.
  - Do NOT call AskUserQuestion while auto mode is active. Make a reasonable decision and continue without asking the user.
  - ExitPlanMode is also approved automatically, without the user reviewing the plan. An auto-approved plan is NOT a signal from the user to start executing — follow the user's original instructions on whether to proceed.";

const AUTO_MODE_EXIT_REMINDER: &str = "\
Auto permission mode is no longer active. Tool approvals and permission checks are back to the current mode.
  - Continue normally, but expect approval prompts or denials when a tool requires them.";

impl Injector for PermissionModeInjector {
    fn inject(&self, _memory: &ContextMemory) -> Option<String> {
        let current_mode = self.manager.mode();
        let mut last = self
            .last_mode
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let previous = *last;
        *last = Some(current_mode);

        match (previous, current_mode) {
            (None, PermissionMode::Auto) => Some(AUTO_MODE_ENTER_REMINDER.to_string()),
            (Some(PermissionMode::Auto), PermissionMode::Auto) => None, // already in auto
            (Some(PermissionMode::Auto), _) => Some(AUTO_MODE_EXIT_REMINDER.to_string()),
            (_, PermissionMode::Auto) => Some(AUTO_MODE_ENTER_REMINDER.to_string()),
            _ => None,
        }
    }
}
