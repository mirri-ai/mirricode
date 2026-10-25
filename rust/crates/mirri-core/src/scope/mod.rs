//! Scope hierarchy — explicit nested structs replacing the TS v2 DI container.
//!
//! The scope tree has four levels, each owning its own state and the
//! collection of child scopes:
//!
//! ```text
//! AppScope
//! └── workspaces: Map<WorkspaceId, Arc<WorkspaceScope>>
//!     └── sessions: Map<SessionId, Arc<SessionScope>>
//!         └── agents: Map<AgentId, Arc<AgentScope>>
//! ```
//!
//! **Design principles:**
//! - Each layer holds only its own state and child references.
//! - Cross-layer sharing uses `Arc<T>`, not DI-container lookup.
//! - Services define contracts via traits; implementations are swappable.
//! - Constructors list all dependencies explicitly; the compiler checks
//!   completeness.
//! - **No DI container** — just explicit nested structs.

pub mod agent_scope;
pub mod app_scope;
pub mod config;
pub mod ids;
pub mod lifecycle;
pub mod session_scope;
pub mod system_prompt;
pub mod workspace_scope;

pub use agent_scope::AgentScope;
pub use app_scope::AppScope;
pub use config::{
    AgentScopeConfig, AppScopeConfig, InteractionSettings, ProviderType, SessionMetadata,
    SessionScopeConfig, TaskLifecycleSettings, UsageInitialState, WorkspaceScopeConfig,
    WorkspaceTrustLevel,
};
pub use ids::{AgentId, SessionId, WorkspaceId};
pub use lifecycle::LifecycleScope;
pub use session_scope::{PendingSkillReminder, SessionMetaHandle, SessionScope, SessionWireLog};
pub use workspace_scope::{
    SkillRootChannel, WorkspaceScope, dedupe_skill_roots, default_skill_root_channels,
};

use std::sync::Arc;

/// The entry point for the entire scope tree.
///
/// Holds an `Arc<AppScope>` so it can be cheaply cloned and shared across
/// async tasks. Created via [`ScopeTree::new`].
#[derive(Debug, Clone)]
pub struct ScopeTree {
    app: Arc<AppScope>,
}

impl ScopeTree {
    /// Create a new scope tree with the given global configuration.
    pub fn new(config: AppScopeConfig) -> Self {
        Self {
            app: Arc::new(AppScope::new(config)),
        }
    }

    /// Access the root `AppScope`.
    pub fn app(&self) -> &Arc<AppScope> {
        &self.app
    }

    /// Consume the tree and return the underlying `Arc<AppScope>`.
    pub fn into_app(self) -> Arc<AppScope> {
        self.app
    }
}
