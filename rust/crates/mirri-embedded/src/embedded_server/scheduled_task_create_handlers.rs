//! The embedded host's `ScheduledTaskCreator` (ADR-022): the single
//! place a workspace scheduled task is created on behalf of an agent —
//! AFTER the engine's one tool approval was granted. No gate entries, no
//! pending map, no notice injection: approve means create, in the very
//! same tool call.

use std::sync::Arc;

use mirri_core::cron::{CronCreateOptions, TaskOrigin, TaskScope, WorkspaceCronManager};
use mirri_core::tools::create_scheduled_task::{ScheduledTaskCreator, ScheduledTaskDraft};

pub(crate) struct RegistryScheduledTaskCreator {
    /// The server's registry (the workspace runtime resolution).
    registry: Arc<crate::workspace_registry::WorkspaceRegistry>,
}

impl RegistryScheduledTaskCreator {
    pub(crate) fn new(registry: Arc<crate::workspace_registry::WorkspaceRegistry>) -> Self {
        Self { registry }
    }
}

impl ScheduledTaskCreator for RegistryScheduledTaskCreator {
    fn create(
        &self,
        draft: ScheduledTaskDraft,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + '_>>
    {
        Box::pin(async move {
            // A14: build on demand — the draft's workspace is a REGISTERED
            // row whose runtime may not be built yet.
            match self.registry.ensure_built(&draft.workspace_id) {
                Some(Ok(runtime)) => {
                    let cron = runtime
                        .cron()
                        .cloned()
                        .ok_or_else(|| "workspace store unavailable".to_string())?;
                    create_task_from_draft(&cron, &draft).await
                }
                Some(Err(error)) => Err(format!("building workspace failed: {error}")),
                None => Err(format!("workspace not found: {}", draft.workspace_id)),
            }
        })
    }
}

/// Create the workspace task from the approved draft (origin =
/// `AgentApproved` — the creation happened through the user's approval).
async fn create_task_from_draft(
    cron: &Arc<WorkspaceCronManager>,
    draft: &ScheduledTaskDraft,
) -> Result<String, String> {
    cron.add(
        draft.cron.clone(),
        draft.prompt.clone(),
        draft.recurring,
        CronCreateOptions {
            timezone: draft.timezone.clone(),
            max_fires: draft.max_fires,
            origin: TaskOrigin::AgentApproved,
            cron_lineage: false,
            scope: TaskScope::Workspace(draft.workspace_id.clone()),
            permission_posture: Some(draft.suggested_posture),
            permission_mode: draft.permission_mode,
            concurrency: draft.concurrency,
            title: draft.title.clone(),
            model: draft.model.clone(),
            effort: draft
                .effort
                .as_deref()
                .and_then(|effort| match effort {
                    "low" => Some(mirri_core::profile::ThinkingEffort::Low),
                    "medium" => Some(mirri_core::profile::ThinkingEffort::Medium),
                    "high" => Some(mirri_core::profile::ThinkingEffort::High),
                    _ => None,
                }),
            ..CronCreateOptions::default()
        },
    )
    .await
    .map_err(|error| error.to_string())
}
