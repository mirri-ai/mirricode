//! AppScope — the root scope, owning global configuration and all workspaces.
//!
//! `AppScope` is the top of the scope tree. It holds global config and the
//! set of active workspaces. Phase 3 services (session index, event bus,
//! plugin manager, MCP registry) will be added as trait-object placeholders
//! in later phases.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::error::{EngineError, EngineResult};

use super::config::{AppScopeConfig, WorkspaceScopeConfig};
use super::ids::WorkspaceId;
use super::workspace_scope::WorkspaceScope;

/// The root scope — owns global config and the workspace collection.
///
/// In Phase 2, only the config and workspaces map are present. Phase 3 will
/// add `session_index`, `event_bus`, `plugin_manager`, and `mcp_registry` as
/// trait-object placeholders.
pub struct AppScope {
    /// Global configuration.
    config: AppScopeConfig,
    /// Active workspaces, keyed by workspace ID.
    workspaces: RwLock<HashMap<WorkspaceId, Arc<WorkspaceScope>>>,
}

impl AppScope {
    /// Create a new root scope with the given configuration.
    pub fn new(config: AppScopeConfig) -> Self {
        Self {
            config,
            workspaces: RwLock::new(HashMap::new()),
        }
    }

    /// Global configuration.
    pub fn config(&self) -> &AppScopeConfig {
        &self.config
    }

    /// Create and register a new workspace with default config.
    ///
    /// Returns an error if a workspace with the same ID already exists.
    pub async fn create_workspace(
        &self,
        id: WorkspaceId,
        cwd: PathBuf,
    ) -> EngineResult<Arc<WorkspaceScope>> {
        self.create_workspace_with_config(id, cwd, WorkspaceScopeConfig::default())
            .await
    }

    /// Create a new workspace with explicit config.
    pub async fn create_workspace_with_config(
        &self,
        id: WorkspaceId,
        cwd: PathBuf,
        config: WorkspaceScopeConfig,
    ) -> EngineResult<Arc<WorkspaceScope>> {
        let mut workspaces = self.workspaces.write().await;
        if workspaces.contains_key(&id) {
            return Err(EngineError::RequestInvalid(format!(
                "workspace '{}' already exists",
                id
            )));
        }
        let ws = Arc::new(WorkspaceScope::new(id.clone(), cwd, config));
        workspaces.insert(id, Arc::clone(&ws));
        Ok(ws)
    }

    /// Retrieve a workspace by ID.
    pub async fn get_workspace(&self, id: &WorkspaceId) -> Option<Arc<WorkspaceScope>> {
        self.workspaces.read().await.get(id).cloned()
    }

    /// Remove a workspace from the app.
    pub async fn remove_workspace(&self, id: &WorkspaceId) -> Option<Arc<WorkspaceScope>> {
        self.workspaces.write().await.remove(id)
    }

    /// List all workspace IDs.
    pub async fn list_workspaces(&self) -> Vec<WorkspaceId> {
        self.workspaces.read().await.keys().cloned().collect()
    }
}

impl std::fmt::Debug for AppScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppScope")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}
