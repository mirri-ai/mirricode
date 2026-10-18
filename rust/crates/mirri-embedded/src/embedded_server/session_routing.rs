//! The session-routing seam (W3, design `docs/specs/rust-core-design/
//! 08-multi-workspace.md` §3 / D7): every session-id-addressing dispatch
//! surface of the [`EmbeddedServer`] resolves the owning workspace's
//! manager through here instead of holding the default workspace's manager.
//!
//! Resolution (registry [`WorkspaceRegistry::manager_for`]):
//!
//! 1. the **active-session table** (session-id → workspace-id, noted on
//!    create/resume/restore, forgotten on close/archive);
//! 2. the **session index** (a persisted session's owning workspace, lazily
//!    built on a hit);
//! 3. the **built-runtime bucket probe** — a `state.json` that physically
//!    lives in a built runtime's own bucket belongs to that workspace (the
//!    only source for engines without an index, and a cold-miss fallback);
//! 4. `NotFound` — never a silent default.
//!
//! The free `*_via` functions take `&WorkspaceRegistry` so spawned tasks
//! (the cron driver, overlay connect tasks) capture the registry `Arc`
//! instead of a single manager; the [`EmbeddedServer`] methods are the
//! dispatch-surface face. A resolution that turns out to hold a live session
//! backfills the active table, so a session created outside the dispatch
//! surface (a test path, a host API) becomes routable from its first touch.

use std::sync::Arc;

use mirri_core::{SessionArtifacts, SessionId, SessionManager, SessionScope};
use mirri_transport::adapter::EngineError;

use super::{EmbeddedServer, not_found, session_error};
use crate::workspace_registry::{ManagerResolution, WorkspaceRegistry};

/// The standard not-found error of the seam — the same message the
/// dispatch-surface handlers produced before the seam existed.
pub(super) fn session_not_found(session_id: &str) -> EngineError {
    not_found(format!("session not found: {session_id}"))
}

/// The not-found error the lifecycle handlers (`session.close` / `archive` /
/// `resume` / `restore`) produce when no workspace owns the id — the same
/// mapping a manager-level `SessionError::NotFound` produced before the
/// seam (code + `session.not_found` domain data).
pub(super) fn lifecycle_session_not_found(session_id: &str) -> EngineError {
    session_error(mirri_core::SessionError::NotFound(session_id.to_string()))
}

/// The not-found error the resume family (`session.resume` / `restore` /
/// `reload`) produces when no workspace owns the id — the same
/// `session.init_failed` domain code the manager-level resume kept, with an
/// honest message: the failure is the ROUTING (no workspace claims the id),
/// not a missing wire log at some default path.
pub(super) fn resume_session_not_found(session_id: &str) -> EngineError {
    session_error(mirri_core::SessionError::ResumeFailed(format!(
        "no workspace owns session {session_id}: it is not active, not in the session index, \
         and not in a built workspace bucket — if it came from the legacy TS engine, run \
         `mirri migrate` and restart the server"
    )))
}

/// The owning workspace's manager + registry id for a session (any level —
/// active, index, or bucket probe; the owning runtime is built on demand).
/// `pub(super)`: also the follow driver's manager seam (it resolves the
/// session and the manager through the same registry pass).
pub(super) fn resolve_manager_via(
    registry: &WorkspaceRegistry,
    session_id: &str,
) -> Result<(Arc<SessionManager>, String), EngineError> {
    match registry.manager_for(session_id) {
        ManagerResolution::Active {
            workspace_id,
            manager,
        }
        | ManagerResolution::Restored {
            workspace_id,
            manager,
        } => Ok((manager, workspace_id)),
        ManagerResolution::NotFound => Err(session_not_found(session_id)),
    }
}

/// The owning manager like [`resolve_manager_via`], but with the lifecycle
/// handlers' not-found mapping (see [`lifecycle_session_not_found`]).
pub(super) fn resolve_lifecycle_manager_via(
    registry: &WorkspaceRegistry,
    session_id: &str,
) -> Result<(Arc<SessionManager>, String), EngineError> {
    resolve_manager_via(registry, session_id).map_err(|_| lifecycle_session_not_found(session_id))
}

/// Resolve the live session owning `session_id` (live-only: a persisted but
/// not-live session resolves to an error — the transport layer's
/// restore-on-demand hook resumes it, not the seam). A live hit backfills
/// the active table (see the module doc).
pub(super) async fn resolve_session_via(
    registry: &WorkspaceRegistry,
    session_id: &str,
) -> Result<Arc<SessionScope>, EngineError> {
    let (manager, workspace_id) = resolve_manager_via(registry, session_id)?;
    let session = manager
        .get_session(&SessionId::new(session_id))
        .await
        .ok_or_else(|| session_not_found(session_id))?;
    registry.note_active(session_id, &workspace_id);
    Ok(session)
}

/// The live session's artifacts (gates / event bus) — the same live-only
/// semantics `SessionManager::session_artifacts` has (artifacts are
/// installed for live sessions only).
pub(super) fn resolve_artifacts_via(
    registry: &WorkspaceRegistry,
    session_id: &str,
) -> Result<SessionArtifacts, EngineError> {
    let (manager, _) = resolve_manager_via(registry, session_id)?;
    manager
        .session_artifacts(&SessionId::new(session_id))
        .ok_or_else(|| session_not_found(session_id))
}

/// The owning workspace of a session for the index write-through: the
/// routing resolution plus the workspace's registered root (the row's
/// `cwd` fallback). `None` when no workspace owns the id — the write
/// through skips rather than attributing the row to the default.
pub(super) struct WorkspaceOwner {
    pub workspace_id: String,
    pub root: std::path::PathBuf,
    pub manager: Arc<SessionManager>,
}

pub(super) fn workspace_owner_via(
    registry: &WorkspaceRegistry,
    session_id: &str,
) -> Option<WorkspaceOwner> {
    let (manager, workspace_id) = match registry.manager_for(session_id) {
        ManagerResolution::Active {
            workspace_id,
            manager,
        }
        | ManagerResolution::Restored {
            workspace_id,
            manager,
        } => (manager, workspace_id),
        ManagerResolution::NotFound => return None,
    };
    let root = registry
        .registration(&workspace_id)
        .map(|registration| registration.root)
        .unwrap_or_else(|| manager.workspace().cwd().to_path_buf());
    Some(WorkspaceOwner {
        workspace_id,
        root,
        manager,
    })
}

impl EmbeddedServer {
    /// Resolve the live session owning `session_id` through the routing seam
    /// (active table → index → bucket probe → not-found; never the default
    /// workspace silently). The replacement for every dispatch site that
    /// used to call `self.manager.get_session(&SessionId::new(..))`.
    pub(super) async fn resolve_session(
        &self,
        session_id: &str,
    ) -> Result<Arc<SessionScope>, EngineError> {
        resolve_session_via(&self.registry, session_id).await
    }

    /// The owning workspace's manager for a session (live or persisted) —
    /// the lifecycle surfaces (`session.resume`/`close`/`archive`/`fork`)
    /// operate on it. Errors with the lifecycle not-found mapping.
    pub(super) fn resolve_session_owner(
        &self,
        session_id: &str,
    ) -> Result<(Arc<SessionManager>, String), EngineError> {
        resolve_lifecycle_manager_via(&self.registry, session_id)
    }

    /// The owning workspace's manager, the plain not-found mapping.
    pub(super) fn resolve_session_manager(
        &self,
        session_id: &str,
    ) -> Result<Arc<SessionManager>, EngineError> {
        resolve_manager_via(&self.registry, session_id).map(|(manager, _)| manager)
    }

    /// The live session's artifacts through the seam — the replacement for
    /// every dispatch site that used to call
    /// `self.manager.session_artifacts(&SessionId::new(..))`.
    pub(super) fn resolve_session_artifacts(
        &self,
        session_id: &str,
    ) -> Result<SessionArtifacts, EngineError> {
        resolve_artifacts_via(&self.registry, session_id)
    }
}
