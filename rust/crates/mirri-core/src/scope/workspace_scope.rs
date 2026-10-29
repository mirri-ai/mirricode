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
use crate::session::lexically_resolve;
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

    /// Replay the default skill-root discovery (B8 R4, ADR-011): the
    /// construction-time three-channel default-root list (project
    /// `.agents/skills`, project `.mirri-code/skills`, user
    /// `$HOME/.agents/skills`), built via [`default_skill_root_channels`]
    /// and deduped by normalized path via [`dedupe_skill_roots`] — the SAME
    /// shared discovery definition the construction-time first scan in
    /// [`Self::new`] runs through. [`Self::new`] calls this once at
    /// assembly; the embedded server's replay path (`apply_skill_discovery`)
    /// carries the FULL four-channel list (the three channels plus the user
    /// brand dir `<mirri home>/skills`) through the same
    /// `default_skill_root_channels` + `dedupe_skill_roots` pair, so both
    /// entry points share one discovery definition.
    ///
    /// Missing directories are silently skipped by `discover`. The user-brand
    /// dir (`<mirri home>/skills`) is NOT scanned here — the workspace scope
    /// does not know the Mirri home (its resolution is injected at the
    /// composition root); `mirri_embedded::bootstrap::assemble` discovers it
    /// after construction, mirroring the TS scanner's USER_BRAND_DIRS.
    pub fn discover_default_skill_roots(&self) {
        self.discover_default_skill_roots_into(&self.skills);
    }

    /// The scratch-build form of [`Self::discover_default_skill_roots`]
    /// (ADR-010 decision 3): the same default-root channels replayed onto
    /// a CALLER-SUPPLIED catalog instead of the workspace's own — the
    /// atomic-rescan orchestration builds the fresh table off to the side
    /// and swaps it in, so this writes the scratch catalog the rescan
    /// assembles. The workspace's own call delegates here with its live
    /// catalog; the discovery sequence (and its HOME-read) is identical.
    pub fn discover_default_skill_roots_into(&self, skills: &SkillCatalog) {
        for channel in dedupe_skill_roots(default_skill_root_channels(&self.cwd)) {
            if let Err(e) = skills.discover(&channel.dir, channel.source) {
                tracing::warn!(path = %channel.dir.display(), error = %e, "skill discovery failed");
            }
        }
    }

    /// The default skill-root candidates this workspace scans — the
    /// discovery-channel paths [`Self::discover_default_skill_roots`]
    /// iterates, as an ordered `(path, source)` sequence: the project
    /// `.agents/skills` and `.mirri-code/skills` directories plus the
    /// user's `$HOME/.agents/skills` (when HOME is set). The user-brand
    /// dir (`<mirri home>/skills`) is NOT part of this sequence — the
    /// workspace scope does not know the Mirri home (its resolution is
    /// injected at the composition root). Superseded as the listen-set
    /// source by [`Self::skill_root_paths`] (which rides the shared
    /// channel list); retained for its original callers.
    #[allow(dead_code)]
    fn default_skill_root_paths(&self) -> Vec<(PathBuf, SkillSource)> {
        let mut candidates = vec![
            (self.cwd.join(".agents/skills"), SkillSource::Project),
            (self.cwd.join(".mirri-code/skills"), SkillSource::Project),
        ];
        if let Some(home) = std::env::var_os("HOME") {
            candidates.push((
                Path::new(&home).join(".agents/skills"),
                SkillSource::User,
            ));
        }
        candidates
    }

    /// Every skill root directory this workspace's catalog can be
    /// re-discovered from — the watcher's listen-set source (ADR-010
    /// decision 1). See [`Self::skill_root_paths`] for the channel
    /// assembly and dedupe semantics.
    pub fn skill_root_paths(&self, mirri_home: Option<&Path>) -> Vec<PathBuf> {
        // The ADR-011 channel list (later-wins dedupe) is the single source
        // for BOTH faces — discovery replays it, and this listen-set
        // derivation rides it too; the user-brand channel joins here (the
        // scope does not know the Mirri home, the caller injects it the same
        // way `apply_skill_discovery` does). Candidates that do not exist
        // on disk are INCLUDED — the watcher must listen for their creation
        // (its parent-directory fallback handles the gap).
        let mut channels = default_skill_root_channels(&self.cwd);
        if let Some(mirri_home) = mirri_home {
            channels.push(SkillRootChannel {
                source: SkillSource::User,
                dir: mirri_home.join("skills"),
            });
        }
        dedupe_skill_roots(channels)
            .into_iter()
            .map(|channel| channel.dir)
            .collect()
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

/// One default skill-root discovery channel: the directory to scan and
/// the source tier the scan attributes (ADR-011).
pub struct SkillRootChannel {
    pub source: SkillSource,
    pub dir: PathBuf,
}

/// The construction-time default-root channel list (ADR-011): the project
/// `.agents/skills`, the project brand dir, and the user `~/.agents/skills`.
/// The user brand dir is NOT here — the workspace scope does not know the
/// Mirri home; the assembly layer's replay carries it (see
/// `apply_skill_discovery`).
pub fn default_skill_root_channels(cwd: &Path) -> Vec<SkillRootChannel> {
    let mut channels = vec![
        SkillRootChannel {
            source: SkillSource::Project,
            dir: cwd.join(".agents/skills"),
        },
        SkillRootChannel {
            source: SkillSource::Project,
            dir: cwd.join(".mirri-code/skills"),
        },
    ];
    if let Some(home) = std::env::var_os("HOME") {
        channels.push(SkillRootChannel {
            source: SkillSource::User,
            dir: Path::new(&home).join(".agents/skills"),
        });
    }
    channels
}

/// Normalize one skill-root path for identity comparison (ADR-011):
/// lexical resolution first (never touches the fs), then symlink
/// resolution via `canonicalize` — a failure (nonexistent dir, perm) falls
/// back to the lexical form so the root still participates in discovery.
fn normalize_skill_root(dir: &Path) -> PathBuf {
    let lexical = lexically_resolve(dir);
    std::fs::canonicalize(&lexical).unwrap_or(lexical) // zero-io-allow: watch-set dedupe needs symlink identity; read-only metadata probe, see F-T13-2
}

/// Dedupe the ordered channel list by normalized path (ADR-011): same
/// physical directory scanned once, by the LATER channel in the sequence
/// (the later-wins override winner — catalog content unchanged, only the
/// duplicate scan and its per-skill shadow warnings disappear). Channels
/// kept in original order minus the earlier duplicates. ⚠️ NOT
/// insert-if-absent: the first channel is NOT the keeper — the accident
/// scenario has Project first and User later, and User must win.
///
/// Precondition on callers: each key appears at most twice (the default
/// root lists never repeat a directory more than once per collision
/// pair). With a key appearing 3+ times interleaved with other keys
/// (`[A, B, A]`), the LAST occurrence wins and is emitted at its own
/// position (`[B, A]`) — the earlier occurrences are removed, which
/// reorders A behind B relative to a strict "drop earlier duplicates in
/// place" reading. The default-root call sites never construct such a
/// sequence.
pub fn dedupe_skill_roots(channels: Vec<SkillRootChannel>) -> Vec<SkillRootChannel> {
    // Key -> index of the LAST channel with that key (later wins, ADR-011).
    let mut last_by_key: std::collections::HashMap<PathBuf, usize> =
        std::collections::HashMap::new();
    for (i, channel) in channels.iter().enumerate() {
        last_by_key.insert(normalize_skill_root(&channel.dir), i);
    }
    // Emit in original order, skipping any channel that is not its key's
    // last holder.
    let mut keep = vec![false; channels.len()];
    for &i in last_by_key.values() {
        keep[i] = true;
    }
    channels
        .into_iter()
        .zip(keep)
        .filter(|(_, k)| *k)
        .map(|(c, _)| c)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WorkspaceTrustLevel;
    use crate::hook_engine::{HookDef, HookEventType};
    use crate::permission::PermissionMode;

    /// Serializes every test that mutates or reads the `HOME` environment
    /// variable: a plain `#[test]` race between two HOME-mutating tests
    /// leaks one test's fake home into another's workspace construction
    /// (the parallel-test hazard the fake-home convention documents).
    ///
    /// There is exactly ONE lock object for this: a second guard function
    /// with its own `static` would let a test holding one run concurrently
    /// with a test holding the other, which is the same race wearing a
    /// different name. Every HOME pin — empty tempdir or populated home —
    /// takes this lock and holds it across the whole pin window.
    fn home_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

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
        // HOME pin: serialize via the crate-wide lock (every HOME reader
        // takes it, so an empty pin is not lock-free either).
        let _guard = home_lock();
        let saved_home = std::env::var_os("HOME");
        // SAFETY: This test only verifies that a workspace with no skills
        // produces an empty catalog. Setting HOME to a temp dir is the only
        // way to prevent discovering the real user's skills.
        unsafe {
            std::env::set_var("HOME", fake_home.path());
        }

        // When a workspace is created
        let workspace = WorkspaceScope::new(
            WorkspaceId::new("ws-no-skills"),
            tmp.path().to_path_buf(),
            WorkspaceScopeConfig::default(),
        );

        // Restore HOME (an originally-unset HOME must not leak the pin)
        match saved_home {
            Some(home) => unsafe {
                std::env::set_var("HOME", home);
            },
            None => unsafe {
                std::env::remove_var("HOME");
            },
        }

        // Then the skill catalog is empty (but not in an error state)
        assert!(workspace.skills().list().is_empty());
    }

    #[test]
    fn should_rediscover_default_roots_when_replayed_after_a_clear() {
        let tmp = tempfile::tempdir().expect("temp dir");
        // Pin HOME to an empty tempdir so only the project-root skill counts
        // (mirrors the fake-home convention of the empty-catalog test above).
        // HOME pin: serialize via the crate-wide lock.
        let _guard = home_lock();
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
        match saved_home {
            Some(home) => unsafe {
                std::env::set_var("HOME", home);
            },
            None => unsafe {
                std::env::remove_var("HOME");
            },
        }

        // Then the same skill set is rediscovered
        assert_eq!(discovered_at_birth, 1);
        assert!(rediscovered, "the skill must be rediscovered after replay");
        assert_eq!(rediscovered_len, discovered_at_birth);
    }

    // Given a workspace whose project skill dir and user skill dir resolve
    // to the same physical directory (workspace root == $HOME), when the
    // default roots are discovered, then the directory is scanned once as
    // the user channel — no skill carries the project label.
    // NOTE: this test is a zero-change REGRESSION GUARD (current code also
    // yields source == User via later-wins) — do not "fix" the assertion if
    // it never went red; the red/green switch lives in Task 2's warning
    // capture test.
    #[test]
    fn should_discover_shared_dir_once_as_user_when_workspace_root_is_home() {
        let home = tempfile::tempdir().expect("home tempdir");
        let skills_dir = home.path().join(".agents/skills/dup-skill");
        std::fs::create_dir_all(&skills_dir).expect("create skill dir");
        std::fs::write(skills_dir.join("SKILL.md"), "# Dup\n").expect("write SKILL.md");
        // brand dir absent — fine, discovery skips missing dirs

        // Non-empty HOME pin: hold the crate lock across the whole window.
        let _guard = home_lock();
        let previous_home = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", home.path()) };
        // When a workspace is created with cwd == $HOME (the collision case)
        let workspace = WorkspaceScope::new(
            WorkspaceId::new("ws-home-collision"),
            home.path().to_path_buf(),
            WorkspaceScopeConfig::default(),
        );
        match previous_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }

        // Then the skill is present exactly once, labeled User
        let skill = workspace
            .skills()
            .get("dup-skill")
            .expect("skill discovered");
        assert_eq!(skill.source, SkillSource::User);
    }

    // Given a workspace reached through a symlink into the user's skill
    // territory, when the default roots are discovered, then the symlinked
    // project channel is recognized as the same physical dir and skipped
    // (zero-change regression guard — see the home-collision test's note).
    #[cfg(unix)]
    #[test]
    fn should_skip_project_channel_when_symlink_resolves_to_user_channel_dir() {
        let home = tempfile::tempdir().expect("home tempdir");
        let skills_dir = home.path().join(".agents/skills/link-skill");
        std::fs::create_dir_all(&skills_dir).expect("create skill dir");
        std::fs::write(skills_dir.join("SKILL.md"), "# Link\n").expect("write SKILL.md");

        let link_parent = tempfile::tempdir().expect("link parent tempdir");
        let link = link_parent.path().join("linked-ws");
        std::os::unix::fs::symlink(home.path(), &link).expect("symlink");

        let _guard = home_lock();
        let previous_home = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", home.path()) };
        let workspace = WorkspaceScope::new(
            WorkspaceId::new("ws-symlink"),
            link,
            WorkspaceScopeConfig::default(),
        );
        match previous_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }

        let skill = workspace
            .skills()
            .get("link-skill")
            .expect("skill discovered");
        assert_eq!(skill.source, SkillSource::User);
    }

    #[test]
    fn should_keep_the_later_channel_when_two_roots_normalize_to_the_same_path() {
        // Given two channels whose dirs normalize to the same path, the
        // later (User) one after the earlier (Project) one
        let channels = vec![
            SkillRootChannel {
                source: SkillSource::Project,
                dir: PathBuf::from("/tmp/./ws/.agents/skills"),
            },
            SkillRootChannel {
                source: SkillSource::User,
                dir: PathBuf::from("/tmp/ws/.agents/skills"),
            },
        ];

        // When the channel list is deduped
        let deduped = dedupe_skill_roots(channels);

        // Then only the later channel survives, keeping its position
        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].source, SkillSource::User);
    }

    #[test]
    fn should_keep_nonexistent_roots_verbatim_when_normalization_fails() {
        // Given two nonexistent roots that only coincide lexically (via `..`)
        let channels = vec![
            SkillRootChannel {
                source: SkillSource::Project,
                dir: PathBuf::from("/definitely/not/here"),
            },
            SkillRootChannel {
                source: SkillSource::User,
                dir: PathBuf::from("/definitely/not/here/../here"),
            },
        ];

        // When the channel list is deduped (canonicalize fails on the
        // nonexistent dirs — the lexical fallback still dedupes)
        let deduped = dedupe_skill_roots(channels);

        // Then they collapse to one entry, the later one
        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].source, SkillSource::User);
    }

    /// Given a workspace with a cwd and a HOME, when `skill_root_paths` is
    /// called with a mirri home, then all four candidate roots are present:
    /// the project `.agents/skills`, the project brand `.mirri-code/skills`,
    /// the user `~/.agents/skills`, and the user-brand `<mirri home>/skills`
    /// — including candidates that do not exist on disk (the watcher must
    /// listen for their creation).
    #[test]
    fn should_return_all_four_candidate_roots_when_collecting_skill_root_paths() {
        let _guard = home_lock();
        let tmp = tempfile::tempdir().expect("temp dir");
        let fake_home = tempfile::tempdir().expect("fake home");
        let mirri_home = tempfile::tempdir().expect("mirri home");
        let saved_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", fake_home.path());
        }

        let workspace = WorkspaceScope::new(
            WorkspaceId::new("ws-roots"),
            tmp.path().to_path_buf(),
            WorkspaceScopeConfig::default(),
        );
        let roots = workspace.skill_root_paths(Some(mirri_home.path()));
        if let Some(home) = saved_home {
            unsafe {
                std::env::set_var("HOME", home);
            }
        }

        let expected = vec![
            tmp.path().join(".agents/skills"),
            tmp.path().join(".mirri-code/skills"),
            fake_home.path().join(".agents/skills"),
            mirri_home.path().join("skills"),
        ];
        assert_eq!(
            roots, expected,
            "the four channels in discovery order, absent candidates included"
        );
    }

    /// Given a workspace whose cwd IS the HOME dir (the server-default-
    /// workspace-is-home scenario, ADR-011's collision case), when
    /// `skill_root_paths` is called with a mirri home inside that same root,
    /// then the normalized-duplicate paths are deduplicated (the later
    /// channel in the sequence wins — the user channel shadows the project
    /// channel of the same physical directory), while distinct directories
    /// under the same root stay separate.
    #[test]
    fn should_deduplicate_when_two_channels_resolve_to_the_same_directory() {
        let _guard = home_lock();
        let tmp = tempfile::tempdir().expect("temp dir");
        let saved_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", tmp.path());
        }

        let workspace = WorkspaceScope::new(
            WorkspaceId::new("ws-dedupe"),
            tmp.path().to_path_buf(),
            WorkspaceScopeConfig::default(),
        );
        let roots = workspace.skill_root_paths(Some(tmp.path()));
        if let Some(home) = saved_home {
            unsafe {
                std::env::set_var("HOME", home);
            }
        }

        // The project `.agents/skills` and the user `~/.agents/skills`
        // resolve to the SAME directory (cwd == HOME); the dedupe keeps
        // ONE entry — the later (user) channel, at its own position
        // (ADR-011 later-wins semantics; for the listen-set face both
        // spellings point at the same physical directory, so which one
        // survives is immaterial to what gets watched).
        let agents_entries = roots
            .iter()
            .filter(|p| p.ends_with(".agents/skills"))
            .count();
        assert_eq!(
            agents_entries, 1,
            "same-directory channels collapse to one entry: {roots:?}"
        );
        // The project brand dir and the user-brand dir are DISTINCT
        // directories under the same root (`.mirri-code/skills` vs
        // `skills`) — they do not collide and both stay; the surviving
        // `.agents/skills` entry sits at the later channel's position
        // (after the brand dir), per the later-wins emission order.
        assert_eq!(
            roots,
            vec![
                tmp.path().join(".mirri-code/skills"),
                tmp.path().join(".agents/skills"),
                tmp.path().join("skills"),
            ],
            "three distinct roots after dedupe, in channel order"
        );
    }

    /// Given a HOME that is unset, when `skill_root_paths` is called, then
    /// the user channel yields no root (no HOME → no `~/.agents/skills`)
    /// and the mirri-home channel still yields its path.
    #[test]
    fn should_skip_the_user_channel_when_home_is_unset() {
        let _guard = home_lock();
        let tmp = tempfile::tempdir().expect("temp dir");
        let mirri_home = tempfile::tempdir().expect("mirri home");
        let saved_home = std::env::var_os("HOME");
        unsafe {
            std::env::remove_var("HOME");
        }

        let workspace = WorkspaceScope::new(
            WorkspaceId::new("ws-no-home"),
            tmp.path().to_path_buf(),
            WorkspaceScopeConfig::default(),
        );
        let roots = workspace.skill_root_paths(Some(mirri_home.path()));
        if let Some(home) = saved_home {
            unsafe {
                std::env::set_var("HOME", home);
            }
        }

        assert_eq!(
            roots,
            vec![
                tmp.path().join(".agents/skills"),
                tmp.path().join(".mirri-code/skills"),
                mirri_home.path().join("skills"),
            ]
        );
    }
}
