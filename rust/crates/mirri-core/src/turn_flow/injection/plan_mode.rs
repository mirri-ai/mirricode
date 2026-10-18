//! Plan-mode reminders: enter/exit guides and periodic sparse refreshes.

use super::*;

// ====================================================================
// PlanModeInjector
// ====================================================================

/// Injects reminders about plan mode state changes and periodic refreshes.
///
/// Tracks `was_active` across steps. On enter/exit transitions, injects the
/// full guide / exit reminder. While plan mode remains active, injects a
/// sparse reminder every 2+ assistant turns or a full refresh every 5+ turns.
pub struct PlanModeInjector {
    manager: Arc<PlanManager>,
    was_active: Mutex<bool>,
}

impl PlanModeInjector {
    /// Create a plan-mode injector backed by the given manager.
    pub fn new(manager: Arc<PlanManager>) -> Self {
        Self {
            manager,
            was_active: Mutex::new(false),
        }
    }
}

const PLAN_FULL_REMINDER: &str = "\
Plan mode is active. You MUST NOT make any edits or otherwise make changes to the system. Only the read-only tools (Read, Grep, Glob) and AskUserQuestion are permitted (AskUserQuestion is denied in auto mode — go straight to ExitPlanMode there); Bash, Write, Edit, and every other tool are denied until the plan is approved. This supersedes any other instructions you have received.

Workflow:
  1. Understand — explore the codebase with Glob, Grep, Read.
  2. Design — converge on the best approach; consider trade-offs but aim for a single recommendation.
  3. Review — re-read key files to verify understanding.
  4. Submit — call ExitPlanMode with the complete plan text in its `plan` parameter.

## Handling multiple approaches
Keep it focused: at most 2-3 meaningfully different approaches. Do NOT pad with minor variations — if one approach is clearly superior, just propose that one.
When the best approach depends on user preferences, constraints, or context you don't have, use AskUserQuestion to clarify first. This helps you write a better, more targeted plan rather than dumping multiple options on the user to sort through.
When you do include multiple approaches, you MUST pass them as the `options` parameter when calling ExitPlanMode, so the user can select which approach to execute at approval time.
NEVER write multiple approaches in the plan and call ExitPlanMode without the `options` parameter — the user will only see the default approval controls with no way to choose a specific approach.

AskUserQuestion is for clarifying missing requirements or user preferences that affect the plan.
Never ask about plan approval via text or AskUserQuestion.
Your turn must end with either AskUserQuestion (to clarify requirements or preferences) or ExitPlanMode (to request plan approval). Do NOT end your turn any other way.
Do NOT use AskUserQuestion to ask about plan approval or reference \"the plan\" — the user cannot see the plan until you call ExitPlanMode.";

const PLAN_SPARSE_REMINDER: &str = "\
Plan mode still active (see full instructions). Only read-only tools (Read, Grep, Glob) and AskUserQuestion are permitted (AskUserQuestion is denied in auto mode — go straight to ExitPlanMode there). Submit the plan by calling ExitPlanMode with the complete plan text in its `plan` parameter; pass alternative approaches via `options` so the user can choose. End turns with AskUserQuestion (for clarifications) or ExitPlanMode (for approval). Never ask about plan approval via text or AskUserQuestion.";

const PLAN_EXIT_REMINDER: &str = "\
Plan mode is no longer active. The read-only and plan-file-only restrictions from plan mode no longer apply. Continue with the approved plan using the normal tool and permission rules.";

impl Injector for PlanModeInjector {
    fn inject(&self, memory: &ContextMemory) -> Option<String> {
        let is_active = self.manager.is_active();

        let mut was_active = self
            .was_active
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let prev = *was_active;

        if !is_active && prev {
            // Exit transition.
            *was_active = false;
            return Some(PLAN_EXIT_REMINDER.to_string());
        }

        if is_active && !prev {
            // Enter transition.
            *was_active = true;
            return Some(PLAN_FULL_REMINDER.to_string());
        }

        if is_active && prev {
            // Still active — count assistant turns since last plan-mode injection.
            let history = memory.history();
            let mut turns_since = 0u32;
            for m in history.iter().rev() {
                if m.origin == Some(PromptOrigin::Injection) && m.message.role == Role::User {
                    let text = m.message.text_content();
                    if text.contains("Plan mode is active")
                        || text.contains("Plan mode still active")
                    {
                        break;
                    }
                }
                if m.message.role == Role::Assistant {
                    turns_since += 1;
                }
            }
            if turns_since >= 5 {
                return Some(PLAN_FULL_REMINDER.to_string());
            }
            if turns_since >= 2 {
                return Some(PLAN_SPARSE_REMINDER.to_string());
            }
            return None;
        }

        // !is_active && !prev — nothing to do.
        None
    }
}
