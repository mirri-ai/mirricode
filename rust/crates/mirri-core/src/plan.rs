//! Plan mode management service.
//!
//! Mirrors the TypeScript `plan` domain: while plan mode is active the agent
//! may only read and plan (writes are gated by the permission system's
//! plan-mode guard). The manager tracks whether plan mode is active, the
//! current plan text, the plan file path once one is written, and a revision
//! history for auditing how the plan evolved.

use std::path::PathBuf;
use std::sync::Mutex;

use crate::error::{EngineError, EngineResult};

/// The full state of plan mode for one session.
#[derive(Debug, Clone, Default)]
pub struct PlanState {
    /// Whether plan mode is currently active.
    pub active: bool,
    /// The current plan text (from `enter`).
    pub plan_text: Option<String>,
    /// Path of the plan file, once one has been written (e.g. by
    /// `ExitPlanMode`).
    pub plan_file_path: Option<PathBuf>,
    /// Ordered history of recorded plan revisions.
    pub revisions: Vec<String>,
}

/// Session-scoped plan mode manager.
///
/// All access goes through an internal `Mutex`, so a single
/// `Arc<PlanManager>` can be shared across agents and async tasks.
#[derive(Debug, Default)]
pub struct PlanManager {
    state: Mutex<PlanState>,
    /// The wire-contract plan id minted by the enter write side
    /// (`plan_mode.enter`'s `id`), so the ExitPlanMode revision op keys on
    /// the same plan. `None` until an enter write side stamps it (and for
    /// pre-T6 logs, whose enter ops carry no id the manager ever saw).
    plan_id: Mutex<Option<String>>,
}

impl PlanManager {
    /// Create a manager with plan mode inactive.
    pub fn new() -> Self {
        Self::default()
    }

    /// Enter plan mode with the given plan text.
    ///
    /// Returns an error if plan mode is already active — entering twice in a
    /// row indicates a caller bug and would silently discard the first plan.
    pub fn enter(&self, plan_text: String) -> EngineResult<()> {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.active {
            return Err(EngineError::RequestInvalid(
                "plan mode is already active; exit or cancel before entering again".to_string(),
            ));
        }
        *guard = PlanState {
            active: true,
            plan_text: Some(plan_text),
            // A fresh plan has no file yet; any prior file path belongs to a
            // cancelled or exited plan.
            plan_file_path: None,
            revisions: Vec::new(),
        };
        // A fresh plan-mode session has no wire id until the enter write
        // side stamps one.
        *self
            .plan_id
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        tracing::debug!("plan mode entered");
        Ok(())
    }

    /// Exit plan mode, keeping the plan text and file path for reference.
    ///
    /// "Exit" means "the plan is accepted and execution begins" — the plan
    /// stays readable via [`PlanManager::get_plan`].
    pub fn exit(&self) {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.active = false;
        tracing::debug!("plan mode exited");
    }

    /// Cancel plan mode, discarding the plan text, file path, and revisions.
    pub fn cancel(&self) {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = PlanState::default();
        *self
            .plan_id
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        tracing::debug!("plan mode cancelled");
    }

    /// Whether plan mode is currently active.
    pub fn is_active(&self) -> bool {
        let guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.active
    }

    /// The current plan text, if plan mode has been entered.
    pub fn get_plan(&self) -> Option<String> {
        let guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.plan_text.clone()
    }

    /// Update the plan text while plan mode is active.
    ///
    /// The model submits the plan through `ExitPlanMode`'s `plan`
    /// parameter; that tool lands the text here before recording the
    /// revision. Errors when plan mode is inactive — updating a plan
    /// that is not under review is a caller bug.
    pub fn set_plan_text(&self, plan_text: String) -> EngineResult<()> {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !guard.active {
            return Err(EngineError::RequestInvalid(
                "plan mode is not active; there is no plan to update".to_string(),
            ));
        }
        guard.plan_text = Some(plan_text);
        Ok(())
    }

    /// Record a revision of the plan.
    ///
    /// Revisions are appended in order and retained until the plan is
    /// cancelled or a new plan is entered.
    pub fn record_revision(&self, revision: String) {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.revisions.push(revision);
        tracing::debug!(count = guard.revisions.len(), "plan revision recorded");
    }

    /// How many revisions have been recorded for the current plan — the
    /// version number the next `plan.revision` op mints (count + 1).
    pub fn revision_count(&self) -> u64 {
        let guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.revisions.len() as u64
    }

    /// Stamp the wire-contract plan id minted by the enter write side, so
    /// the revision op keys on the same plan (`plan_mode.enter.id`).
    pub fn set_plan_id(&self, id: String) {
        *self
            .plan_id
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(id);
    }

    /// The stamped wire-contract plan id, when an enter write side stamped
    /// one (or a resume restored it from the replayed `plan_mode.enter`).
    pub fn plan_id(&self) -> Option<String> {
        self.plan_id
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Restore the plan text without activating plan mode — the post-exit
    /// resume arm (F12): the session was closed after an approved plan
    /// exited, so the plan is no longer active but its text stays readable
    /// through [`PlanManager::get_plan`] (the resume summary and
    /// `plan.status` reference surfaces).
    pub fn restore_plan_text(&self, plan_text: String) {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.active = false;
        guard.plan_text = Some(plan_text);
        tracing::debug!("plan text restored after resume (plan mode inactive)");
    }

    /// Set the path of the plan file.
    ///
    /// Used by the host once the plan has been persisted to disk (the
    /// `ExitPlanMode` flow).
    pub fn set_plan_file_path(&self, path: PathBuf) {
        let mut guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.plan_file_path = Some(path);
        tracing::debug!("plan file path set");
    }

    /// The path of the plan file, if one has been set.
    pub fn get_plan_file_path(&self) -> Option<PathBuf> {
        let guard = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.plan_file_path.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_start_with_plan_mode_inactive_when_manager_is_new() {
        let manager = PlanManager::new();
        assert!(!manager.is_active());
        assert!(manager.get_plan().is_none());
    }

    #[test]
    fn should_activate_plan_with_text_when_entering_from_inactive_mode() {
        let manager = PlanManager::new();
        manager.enter("Step 1: explore".to_string()).unwrap();
        assert!(manager.is_active());
        assert_eq!(manager.get_plan().as_deref(), Some("Step 1: explore"));
    }

    #[test]
    fn should_update_plan_text_when_plan_mode_is_active() {
        let manager = PlanManager::new();
        manager.enter(String::new()).unwrap();

        manager
            .set_plan_text("Step 1: do the thing\nStep 2: verify".to_string())
            .unwrap();

        assert_eq!(
            manager.get_plan().as_deref(),
            Some("Step 1: do the thing\nStep 2: verify")
        );
    }

    #[test]
    fn should_error_when_setting_plan_text_while_plan_mode_is_inactive() {
        let manager = PlanManager::new();

        let error = manager.set_plan_text("Step 1".to_string()).unwrap_err();

        assert!(error.to_string().contains("plan mode is not active"));
    }

    #[test]
    fn should_reject_entering_again_when_plan_mode_is_active() {
        let manager = PlanManager::new();
        manager.enter("First plan".to_string()).unwrap();
        let error = manager.enter("Second plan".to_string()).unwrap_err();
        assert!(error.to_string().contains("plan mode is already active"));
        // The original plan is left untouched.
        assert_eq!(manager.get_plan().as_deref(), Some("First plan"));
    }

    #[test]
    fn should_deactivate_but_keep_plan_when_exiting_active_plan() {
        let manager = PlanManager::new();
        manager.enter("Keep me".to_string()).unwrap();
        manager.exit();
        assert!(!manager.is_active());
        assert_eq!(manager.get_plan().as_deref(), Some("Keep me"));
    }

    #[test]
    fn should_discard_plan_when_cancelling_active_plan() {
        let manager = PlanManager::new();
        manager.enter("Discard me".to_string()).unwrap();
        manager.record_revision("revision 1".to_string());
        manager.cancel();
        assert!(!manager.is_active());
        assert!(manager.get_plan().is_none());
    }

    #[test]
    fn should_reset_revisions_when_entering_new_plan() {
        let manager = PlanManager::new();
        manager.enter("Plan A".to_string()).unwrap();
        manager.record_revision("A v1".to_string());
        manager.exit();
        manager.enter("Plan B".to_string()).unwrap();
        assert_eq!(manager.get_plan().as_deref(), Some("Plan B"));
    }

    #[test]
    fn should_store_plan_file_path_when_set() {
        let manager = PlanManager::new();
        manager.set_plan_file_path(PathBuf::from("/tmp/plan.md"));
        let guard = manager.state.lock().unwrap();
        assert_eq!(
            guard.plan_file_path.as_deref(),
            Some(std::path::Path::new("/tmp/plan.md"))
        );
    }

    #[test]
    fn should_report_the_next_version_when_revisions_are_recorded() {
        let manager = PlanManager::new();
        manager.enter("Plan".to_string()).unwrap();
        assert_eq!(manager.revision_count(), 0);
        manager.record_revision("v1".to_string());
        manager.record_revision("v2".to_string());
        assert_eq!(
            manager.revision_count(),
            2,
            "the next revision op mints version 3 (count + 1)"
        );
    }

    #[test]
    fn should_stamp_and_clear_the_wire_plan_id_when_set() {
        let manager = PlanManager::new();
        assert_eq!(manager.plan_id(), None);

        manager.set_plan_id("plan-abc".to_string());
        assert_eq!(manager.plan_id().as_deref(), Some("plan-abc"));

        // A fresh enter resets the stamped id (a new plan-mode session).
        manager.enter("Fresh plan".to_string()).unwrap();
        assert_eq!(manager.plan_id(), None);

        // And a cancel discards it too.
        manager.set_plan_id("plan-def".to_string());
        manager.cancel();
        assert_eq!(manager.plan_id(), None);
    }

    #[test]
    fn should_restore_plan_text_without_activating_when_restore_plan_text_called() {
        let manager = PlanManager::new();

        manager.restore_plan_text("1. The approved plan".to_string());

        assert!(
            !manager.is_active(),
            "restore must not re-activate plan mode"
        );
        assert_eq!(manager.get_plan().as_deref(), Some("1. The approved plan"));
    }
}
