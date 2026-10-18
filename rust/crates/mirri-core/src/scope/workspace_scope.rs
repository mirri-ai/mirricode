//! WorkspaceScope — a project directory context within the app.
//!
//! A `WorkspaceScope` owns the working directory path, workspace-level config,
//! and the set of sessions active in that workspace.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::sync::RwLock;

use crate::error::{EngineError, EngineResult};
use crate::hook_engine::HookEngine;
use crate::skill::{SkillCatalog, SkillSource};

use super::config::{SessionMetadata, SessionScopeConfig, WorkspaceScopeConfig};
use super::ids::{SessionId, WorkspaceId};
use super::session_scope::SessionScope;

/// A workspace — represents a project directory and its active sessions.
///
/// The `cwd` is the canonical working directory for file operations. The
/// `sessions` map holds all active conversation sessions in this workspace.
/// The workspace owns the skills visible to all of its sessions.
pub struct WorkspaceScope {
    /// Unique identifier for this workspace.
    id: WorkspaceId,
    /// Working directory path for this workspace.
    cwd: PathBuf,
    /// Session collection, keyed by session ID.
    sessions: RwLock<HashMap<SessionId, Arc<SessionScope>>>,
    /// Workspace-level configuration.
    config: WorkspaceScopeConfig,
    /// The LIVE project-AGENTS.md trust gate (W8, design 08 §5 D6).
    ///
    /// Seeded from [`WorkspaceScopeConfig::load_project_agents_md`] at
    /// construction; the W8 `workspace.setTrust` face flips it in place
    /// (workspace_trust.rs's runtime refresh) so every session created
    /// AFTER the flip carries the new verdict — the config field stays
    /// the build-time seed, this atomic is the live answer. Sessions
    /// already created hold their own copy of the gate (see
    /// [`SessionScope`]), which the same flip updates per live session.
    load_project_agents_md: AtomicBool,
    /// The workspace's skill catalog (skills are workspace-scoped).
    skills: Arc<SkillCatalog>,
    /// Workspace-scoped external-command hook engine.
    ///
    /// Seeded from [`WorkspaceScopeConfig::hooks`] at construction. The
    /// config hot-reload watcher (see `mirri_cli::watcher`) clears and
    /// re-registers this engine when `config.toml` changes, so sessions
    /// created after a reload inherit the new hooks.
    hook_engine: Arc<HookEngine>,
}

impl WorkspaceScope {
    /// Create a new workspace scope.
    pub fn new(id: WorkspaceId, cwd: PathBuf, config: WorkspaceScopeConfig) -> Self {
        let hook_engine = Arc::new(HookEngine::new());
        hook_engine.register_all(config.hooks.clone());

        // Discover skills from the project-level (`.agents/skills/` and the
        // TS brand dir `.mirri-code/skills/`) and user-level
        // (`~/.agents/skills/`) directories via the replayable default-root
        // discovery. The user-brand dir (`<mirri home>/skills`) is NOT
        // scanned here — see [`Self::discover_default_skill_roots`].
        let skills = Arc::new(SkillCatalog::new());

        let workspace = Self {
            id,
            cwd,
            sessions: RwLock::new(HashMap::new()),
            load_project_agents_md: AtomicBool::new(config.load_project_agents_md),
            config,
            skills,
            hook_engine,
        };
        workspace.discover_default_skill_roots();
        workspace
    }

    /// Replay the default skill-root discovery (B8 R4): the project
    /// `.agents/skills` / `.mirri-code/skills` directories plus
    /// `$HOME/.agents/skills`. [`Self::new`] runs this once at assembly; the
    /// embedded server's config-write resync hook replays it on every built
    /// workspace after a `clear()` so a config change takes effect without a
    /// restart.
    ///
    /// Missing directories are silently skipped by `discover`. The user-brand
    /// dir (`<mirri home>/skills`) is NOT scanned here — the workspace scope
    /// does not know the Mirri home (its resolution is injected at the
    /// composition root); `mirri_embedded::bootstrap::assemble` discovers it
    /// after construction, mirroring the TS scanner's USER_BRAND_DIRS.
    pub fn discover_default_skill_roots(&self) {
        let skills = &self.skills;
        let project_skills_dir = self.cwd.join(".agents/skills");
        if let Err(e) = skills.discover(&project_skills_dir, SkillSource::Project) {
            tracing::warn!(path = %project_skills_dir.display(), error = %e, "project skill discovery failed");
        }
        let project_brand_skills_dir = self.cwd.join(".mirri-code/skills");
        if let Err(e) = skills.discover(&project_brand_skills_dir, SkillSource::Project) {
            tracing::warn!(path = %project_brand_skills_dir.display(), error = %e, "project brand skill discovery failed");
        }
        if let Some(home) = std::env::var_os("HOME") {
            let user_skills_dir = Path::new(&home).join(".agents/skills");
            if let Err(e) = skills.discover(&user_skills_dir, SkillSource::User) {
                tracing::warn!(path = %user_skills_dir.display(), error = %e, "user skill discovery failed");
            }
        }
    }

    /// This workspace's unique identifier.
    pub fn id(&self) -> &WorkspaceId {
        &self.id
    }

    /// The working directory path for this workspace.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// Workspace-level configuration.
    pub fn config(&self) -> &WorkspaceScopeConfig {
        &self.config
    }

    /// The live project-AGENTS.md trust gate (W8): whether sessions created
    /// in this workspace from now on inject the project-level AGENTS.md
    /// sections. See [`Self::load_project_agents_md`].
    pub fn load_project_agents_md(&self) -> bool {
        self.load_project_agents_md.load(Ordering::Acquire)
    }

    /// Flip the live project-AGENTS.md trust gate (W8): the
    /// `workspace.setTrust` runtime refresh calls this so the verdict
    /// applies to every session created after the flip without rebuilding
    /// the runtime (sessions already created are updated by the same
    /// refresh through their own gate — see
    /// [`SessionScope::set_load_project_agents_md`]).
    pub fn set_load_project_agents_md(&self, value: bool) {
        self.load_project_agents_md.store(value, Ordering::Release);
    }

    /// Additional directories this workspace may access beyond the root
    /// working directory.
    ///
    /// Declared in [`WorkspaceScopeConfig::extra_directories`] so the engine
    /// knows about them for permission/trust purposes. File tools operate on
    /// absolute paths, so these directories need no further wiring for file
    /// access.
    pub fn extra_directories(&self) -> &[PathBuf] {
        &self.config.extra_directories
    }

    /// The workspace's skill catalog.
    ///
    /// Skills are workspace-scoped: every session in this workspace shares
    /// the same discovered skill set.
    pub fn skills(&self) -> &Arc<SkillCatalog> {
        &self.skills
    }

    /// Shared handle to the workspace-scoped external-command hook engine.
    ///
    /// Sessions inherit the engine's *current* registrations when they are
    /// created, so a config.toml hot-reload (clear + re-register on this
    /// engine) is picked up by every session created afterwards.
    pub fn hook_engine(&self) -> Arc<HookEngine> {
        Arc::clone(&self.hook_engine)
    }

    /// Create and register a new session in this workspace.
    ///
    /// Returns an error if a session with the same ID already exists.
    pub async fn create_session(
        &self,
        id: SessionId,
        metadata: SessionMetadata,
    ) -> EngineResult<Arc<SessionScope>> {
        self.create_session_with_config(id, metadata, SessionScopeConfig::default(), None)
            .await
    }

    /// Create a new session with explicit config.
    ///
    /// The workspace's configured hooks ([`WorkspaceScopeConfig::hooks`]) are
    /// inherited by the session: they are prepended to the session config's
    /// own `hooks` before the [`SessionScope`] registers them, so workspace
    /// hooks fire before session-specific ones.
    ///
    /// `tasks_dir` (normally `<session_dir>/tasks`) roots on-disk
    /// persistence for the session's background tasks; `None` keeps the
    /// session's [`TaskManager`] purely in memory.
    pub async fn create_session_with_config(
        &self,
        id: SessionId,
        metadata: SessionMetadata,
        mut config: SessionScopeConfig,
        tasks_dir: Option<PathBuf>,
    ) -> EngineResult<Arc<SessionScope>> {
        let mut sessions = self.sessions.write().await;
        if sessions.contains_key(&id) {
            return Err(EngineError::RequestInvalid(format!(
                "session '{}' already exists in workspace '{}'",
                id, self.id
            )));
        }
        // Inherit the workspace's external command hooks. The hooks come
        // from the workspace's live hook engine rather than the static
        // `WorkspaceScopeConfig`, so sessions created after a config.toml
        // hot-reload inherit the reloaded hooks.
        config.hooks = self
            .hook_engine
            .list()
            .into_iter()
            .chain(config.hooks)
            .collect();
        // Inherit the workspace's trust gate for the project-level
        // AGENTS.md sections (W7, design 08 §5 D6): the workspace owns the
        // trust decision, so every session created in it carries the same
        // verdict — a caller-supplied session config cannot re-open an
        // untrusted workspace's project instructions. W8: the LIVE gate is
        // read (not the frozen config seed), so a trust flip applies to
        // sessions created after it without a runtime rebuild.
        config.load_project_agents_md = self.load_project_agents_md();
        let session = Arc::new(SessionScope::new(
            id.clone(),
            metadata,
            config,
            Arc::clone(&self.skills),
            Some(self.cwd.clone()),
            tasks_dir,
        ));
        sessions.insert(id, Arc::clone(&session));
        Ok(session)
    }

    /// Retrieve a session by ID.
    pub async fn get_session(&self, id: &SessionId) -> Option<Arc<SessionScope>> {
        self.sessions.read().await.get(id).cloned()
    }

    /// Remove a session from this workspace.
    pub async fn remove_session(&self, id: &SessionId) -> Option<Arc<SessionScope>> {
        self.sessions.write().await.remove(id)
    }

    /// List all session IDs in this workspace.
    pub async fn list_sessions(&self) -> Vec<SessionId> {
        self.sessions.read().await.keys().cloned().collect()
    }
}

impl std::fmt::Debug for WorkspaceScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkspaceScope")
            .field("id", &self.id)
            .field("cwd", &self.cwd)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WorkspaceTrustLevel;
    use crate::hook_engine::{HookDef, HookEventType};
    use crate::permission::PermissionMode;

    fn make_workspace(config: WorkspaceScopeConfig) -> WorkspaceScope {
        WorkspaceScope::new(
            WorkspaceId::new("ws-test"),
            std::path::PathBuf::from("/tmp"),
            config,
        )
    }

    #[tokio::test]
    async fn should_inherit_workspace_hooks_when_creating_session_with_config() {
        // Given a workspace configured with external command hooks and a
        // session config with its own hooks
        let workspace_config = WorkspaceScopeConfig {
            hooks: vec![HookDef::new(HookEventType::SessionStart, "echo ws")],
            ..WorkspaceScopeConfig::default()
        };
        let workspace = make_workspace(workspace_config);

        let session_config = SessionScopeConfig {
            hooks: vec![HookDef::new(HookEventType::SessionEnd, "echo session")],
            ..SessionScopeConfig::default()
        };

        // When a session is created in the workspace
        let session = workspace
            .create_session_with_config(
                SessionId::new("session-1"),
                SessionMetadata::default(),
                session_config,
                None,
            )
            .await
            .expect("session should be created");

        // Then the workspace hooks are registered first, followed by the
        // session's own hooks
        let registered = session.hook_engine().list();
        assert_eq!(registered.len(), 2);
        assert_eq!(registered[0].event, HookEventType::SessionStart);
        assert_eq!(registered[1].event, HookEventType::SessionEnd);
    }

    #[tokio::test]
    async fn should_inherit_workspace_hooks_when_creating_session_with_default_config() {
        // Given a workspace configured with external command hooks
        let workspace_config = WorkspaceScopeConfig {
            hooks: vec![HookDef::new(HookEventType::Stop, "echo stop")],
            ..WorkspaceScopeConfig::default()
        };
        let workspace = make_workspace(workspace_config);

        // When a session is created with the default session config
        let session = workspace
            .create_session(SessionId::new("session-1"), SessionMetadata::default())
            .await
            .expect("session should be created");

        // Then the workspace hooks are still inherited
        let registered = session.hook_engine().list();
        assert_eq!(registered.len(), 1);
        assert_eq!(registered[0].event, HookEventType::Stop);
    }

    #[test]
    fn should_default_to_empty_hooks_when_creating_workspace_scope_config() {
        // Given the default workspace config
        let config = WorkspaceScopeConfig {
            trust_level: WorkspaceTrustLevel::Trusted,
            permission_mode: PermissionMode::default(),
            ..Default::default()
        };

        // When inspected
        // Then it carries no hooks
        assert!(config.hooks.is_empty());
    }

    #[test]
    fn should_expose_configured_extra_directories() {
        // Given a workspace configured with an extra directory outside cwd
        let configured_extra_directories = vec![PathBuf::from("/shared/assets")];
        let workspace = make_workspace(WorkspaceScopeConfig {
            extra_directories: configured_extra_directories.clone(),
            ..WorkspaceScopeConfig::default()
        });

        // Then the workspace exposes them verbatim
        assert_eq!(
            workspace.extra_directories(),
            configured_extra_directories.as_slice()
        );
    }

    // Given a workspace whose config closes the project AGENTS.md gate,
    // when a session is created in it, then the session inherits the closed
    // gate — and a caller-supplied session config cannot re-open it (the
    // workspace owns the trust decision, W7 design 08 §5 D6).
    #[tokio::test]
    async fn should_inherit_the_workspace_agents_md_gate_when_creating_a_session() {
        // Given a workspace with the gate closed
        let workspace = make_workspace(WorkspaceScopeConfig {
            load_project_agents_md: false,
            ..WorkspaceScopeConfig::default()
        });

        // When a session is created with a session config that (wrongly)
        // tries to re-open the gate
        let session = workspace
            .create_session_with_config(
                SessionId::new("session-1"),
                SessionMetadata::default(),
                SessionScopeConfig {
                    load_project_agents_md: true,
                    ..SessionScopeConfig::default()
                },
                None,
            )
            .await
            .expect("session should be created");

        // Then the session's inherited config carries the workspace's
        // verdict, not the caller's
        assert!(
            !session.config().load_project_agents_md,
            "the workspace's trust gate must win over the session config"
        );

        // And a workspace with the gate open keeps it open.
        let trusted_workspace = make_workspace(WorkspaceScopeConfig::default());
        let trusted_session = trusted_workspace
            .create_session(SessionId::new("session-2"), SessionMetadata::default())
            .await
            .expect("session should be created");
        assert!(
            trusted_session.config().load_project_agents_md,
            "the default workspace config keeps the historical gate-open behavior"
        );
    }

    #[tokio::test]
    async fn should_inherit_reloaded_hooks_when_creating_session_after_hook_engine_reload() {
        // Given a workspace whose hook engine is hot-reloaded (clear +
        // re-register, mirroring the config.toml watcher's reload sequence)
        // after creation
        let workspace_config = WorkspaceScopeConfig {
            hooks: vec![HookDef::new(HookEventType::SessionStart, "echo original")],
            ..WorkspaceScopeConfig::default()
        };
        let workspace = make_workspace(workspace_config);
        workspace.hook_engine().clear();
        workspace.hook_engine().register_all(vec![HookDef::new(
            HookEventType::SessionEnd,
            "echo reloaded",
        )]);

        // When a session is created after the reload
        let session = workspace
            .create_session(SessionId::new("session-1"), SessionMetadata::default())
            .await
            .expect("session should be created");

        // Then the session inherits the reloaded hooks, not the originals
        let registered = session.hook_engine().list();
        assert_eq!(registered.len(), 1);
        assert_eq!(registered[0].event, HookEventType::SessionEnd);
        assert_eq!(registered[0].command, "echo reloaded");
    }

    #[test]
    fn should_discover_project_skills_when_workspace_created_with_skills_dir() {
        // Given a temp directory with a `.agents/skills/` containing a skill
        let tmp = tempfile::tempdir().expect("temp dir");
        let skills_dir = tmp.path().join(".agents/skills/my-skill");
        std::fs::create_dir_all(&skills_dir).expect("create skill dir");
        std::fs::write(
            skills_dir.join("SKILL.md"),
            "# My Skill\nA test skill for discovery.",
        )
        .expect("write SKILL.md");

        // When a workspace is created with that cwd
        let workspace = WorkspaceScope::new(
            WorkspaceId::new("ws-skill-test"),
            tmp.path().to_path_buf(),
            WorkspaceScopeConfig::default(),
        );

        // Then the skill catalog contains the discovered skill
        let catalog = workspace.skills();
        let names: Vec<String> = catalog.list().into_iter().map(|s| s.name).collect();
        assert!(
            names.contains(&"my-skill".to_string()),
            "expected 'my-skill' in discovered skills, got {:?}",
            names
        );
    }

    #[test]
    fn should_discover_project_brand_skills_when_workspace_created_with_mirri_code_skills_dir() {
        // Given a temp directory with a `.mirri-code/skills/` (the TS
        // PROJECT_BRAND_DIRS parity) containing a skill
        let tmp = tempfile::tempdir().expect("temp dir");
        let skills_dir = tmp.path().join(".mirri-code/skills/brand-skill");
        std::fs::create_dir_all(&skills_dir).expect("create skill dir");
        std::fs::write(
            skills_dir.join("SKILL.md"),
            "# Brand Skill\nA project brand skill.",
        )
        .expect("write SKILL.md");

        // When a workspace is created with that cwd
        let workspace = WorkspaceScope::new(
            WorkspaceId::new("ws-brand-skill-test"),
            tmp.path().to_path_buf(),
            WorkspaceScopeConfig::default(),
        );

        // Then the skill catalog contains the discovered skill (as a
        // project skill — the brand dir rides the same source tier)
        let catalog = workspace.skills();
        let skill = catalog
            .get("brand-skill")
            .expect("brand skill must be discovered from .mirri-code/skills");
        assert_eq!(skill.source, SkillSource::Project);
    }

    #[test]
    fn should_have_empty_skill_catalog_when_no_skills_dir_exists() {
        // Given a temp directory with no `.agents/skills/` and a HOME pointing
        // to another temp dir that also has no skills.
        let tmp = tempfile::tempdir().expect("temp dir");
        let fake_home = tempfile::tempdir().expect("fake home temp dir");
        let saved_home = std::env::var_os("HOME");
        // SAFETY: This test only verifies that a workspace with no skills
        // produces an empty catalog. Setting HOME to a temp dir is the only
        // way to prevent discovering the real user's skills. The test runs
        // in parallel with others, but no sibling test in this binary reads
        // HOME except the skill discovery test which uses its own temp dir.
        unsafe {
            std::env::set_var("HOME", fake_home.path());
        }

        // When a workspace is created
        let workspace = WorkspaceScope::new(
            WorkspaceId::new("ws-no-skills"),
            tmp.path().to_path_buf(),
            WorkspaceScopeConfig::default(),
        );

        // Restore HOME
        if let Some(home) = saved_home {
            unsafe {
                std::env::set_var("HOME", home);
            }
        }

        // Then the skill catalog is empty (but not in an error state)
        assert!(workspace.skills().list().is_empty());
    }

    #[test]
    fn should_rediscover_default_roots_when_replayed_after_a_clear() {
        let tmp = tempfile::tempdir().expect("temp dir");
        // Pin HOME to an empty tempdir so only the project-root skill counts
        // (mirrors the fake-home convention of the empty-catalog test above).
        let fake_home = tempfile::tempdir().expect("fake home temp dir");
        let saved_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", fake_home.path());
        }
        let skills_dir = tmp.path().join(".agents/skills/re-greet");
        std::fs::create_dir_all(&skills_dir).expect("create skill dir");
        std::fs::write(skills_dir.join("SKILL.md"), "# Re\n").expect("write SKILL.md");
        let workspace = WorkspaceScope::new(
            WorkspaceId::new("ws-rescan"),
            tmp.path().to_path_buf(),
            WorkspaceScopeConfig::default(),
        );
        let discovered_at_birth = workspace.skills().list().len();

        // When the catalog is cleared and the default roots are replayed,
        // restore HOME first so a failing assert below cannot leak the
        // fake home into sibling tests.
        workspace.skills().clear();
        workspace.discover_default_skill_roots();
        let rediscovered = workspace.skills().get("re-greet").is_some();
        let rediscovered_len = workspace.skills().list().len();
        if let Some(home) = saved_home {
            unsafe {
                std::env::set_var("HOME", home);
            }
        }

        // Then the same skill set is rediscovered
        assert_eq!(discovered_at_birth, 1);
        assert!(rediscovered, "the skill must be rediscovered after replay");
        assert_eq!(rediscovered_len, discovered_at_birth);
    }
}
