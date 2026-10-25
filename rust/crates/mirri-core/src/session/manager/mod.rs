//! Session lifecycle management — create, resume, fork, close, list.
//!
//! [`SessionManager`] is the top-level orchestrator for session lifecycles
//! within a workspace. It wraps a [`WorkspaceScope`] and delegates session
//! storage to it, adding:
//!
//! - Automatic session ID generation (UUID v4).
//! - Session persistence integration (wire log path computation, state
//!   save/load via [`SessionPersistence`]).
//! - Session lifecycle hooks (firing [`SessionLifecycleHooks`] on create
//!   and close).
//! - Fork support (creating a new session from an existing agent's context).
//! - Resume support (framework for replaying `wire.jsonl` to reconstruct
//!   session state).
//! - Event plumbing: each session owns its own [`EventBus`] broadcast channel
//!   plus [`ApprovalGate`] / [`QuestionGate`] sets, bundled as
//!   [`SessionArtifacts`] — fresh on create/resume, drained on close through
//!   [`SessionShutdown`]. No event or gate request ever crosses a session
//!   boundary.
//!
//! ## Resume
//!
//! The resume flow reconstructs a closed session's state by replaying its
//! wire log: it loads session metadata from `state.json`, folds every
//! `wire.jsonl` record after the `snapshot.json` checkpoint (if one exists)
//! through the [`mirri_proto::reduce_with_limits`] reducer, rebuilds the main
//! agent's
//! [`ContextMemory`] from the resulting [`mirri_proto::AgentState`], and
//! registers a fresh [`SessionScope`] in the workspace. A snapshot written
//! at session close lets resume skip the already-folded prefix of the log.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use chrono::Utc;
use mirri_os::{DynFileSystem, LocalFs};
use mirri_persistence::{
    AppendLogStore, JsonlAppendLog, SnapshotStore, migrate_log_file, migrate_records_in_memory,
};
use mirri_proto::{AgentState, WireOp};
use serde::Serialize;
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::blob::AgentBlobService;
use crate::config::ServerConfig;
use crate::context::ContextMessage;
use crate::cron::CronManager;
use crate::goal::{GoalBudget, GoalStatus};
use crate::hook_engine::HookEventType;
use crate::permission::{PermissionMode, PermissionRule};
use crate::scope::{
    AgentId, AgentScope, AgentScopeConfig, InteractionSettings, SessionId, SessionMetadata,
    SessionScope, SessionScopeConfig, TaskLifecycleSettings, UsageInitialState, WorkspaceScope,
};

use super::agent_lifecycle::AgentLifecycle;
use super::artifacts::SessionArtifacts;
use super::error::{SessionError, SessionResult};
use super::events::{SessionEvent, uuid_from_id};
use super::persistence::SessionPersistence;
use super::replay::{reconstruct_context_memory, reconstruct_context_memory_with_blobs};
use super::shutdown::SessionShutdown;
use super::workdir_key::encode_work_dir_key;

/// Options of the session fork pipeline (F2, design 12 §3).
///
/// `turn_id` anchors the persisted fork at a specific completed turn
/// (a `turn.ended` turn id); `None` anchors at the source's most recent
/// completed turn. See [`SessionManager::fork_session_with_options`].
#[derive(Debug, Clone, Default)]
pub struct ForkOptions {
    /// The `turn.ended` turn id the fork rewinds to. `None` = the most
    /// recent completed turn (an active turn's content never carries).
    pub turn_id: Option<u64>,
}

/// Why a session is being closed.
#[derive(Debug, Clone)]
pub enum CloseReason {
    /// The session exited normally (agent loop completed).
    Exit,
    /// The session closed due to an error.
    Error(String),
    /// The user explicitly closed the session.
    UserInitiated,
    /// The user archived the session (closed, but kept for a later restore).
    UserClosed,
}

/// How a session was created.
#[derive(Debug, Clone)]
pub enum SessionSource {
    /// Fresh session started at app launch.
    Startup,
    /// Session resumed from a persisted wire log.
    Resume { session_id: String },
    /// Session forked from another session's agent.
    Fork {
        source_session_id: String,
        source_agent_id: String,
    },
}

/// Lifecycle status of a session, as reported by [`SessionManager::list_all_sessions`].
///
/// A session is `Running` when it is live in the workspace (in-memory), and
/// `Closed` when it exists only on disk (persisted but not resumed). The TS
/// client uses the same distinction to decide whether to show a session as
/// active or archived.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionStatus {
    /// The session is live in the workspace (in-memory).
    Running,
    /// The session exists only on disk (persisted but not resumed).
    Closed,
}

/// Serializable summary of a session, returned by [`SessionManager::list_sessions`]
/// and [`SessionManager::list_all_sessions`].
#[derive(Debug, Clone, Serialize)]
pub struct SessionInfo {
    /// The session's unique identifier.
    pub id: String,
    /// Human-readable session title.
    pub title: String,
    /// When the session was created (UTC).
    pub created_at: chrono::DateTime<Utc>,
    /// When the session was last updated (UTC).
    pub updated_at: chrono::DateTime<Utc>,
    /// Number of agents currently in the session.
    pub agent_count: usize,
    /// Free-form tags associated with the session.
    pub tags: Vec<String>,
    /// Whether the session is archived (the `state.json` flag, TS parity).
    /// Live sessions carry their in-memory metadata flag (false unless the
    /// session was resumed from an archived state without `session.restore`);
    /// persisted rows carry the `state.json` value. Default listings exclude
    /// archived persisted sessions (see
    /// [`SessionManager::list_all_sessions`]).
    pub archived: bool,
    /// When the session was archived (`archivedAt` in `state.json`, the
    /// retention domain's §3.1 stamp), when recorded — `None` for live
    /// sessions, restored sessions, and legacy states predating the field.
    /// Live sessions do not track the stamp in memory (only the archive
    /// lifecycle writes it), so their value is `None`.
    pub archived_at: Option<chrono::DateTime<Utc>>,
    /// Whether the session's wire log is gzip-compressed on disk — the
    /// `state.json` **cached** flag (ADR-007: the disk shape is the truth
    /// source; the scanner reconciles with it). Always `false` for live
    /// sessions (ADR-004: a working session is never compressed).
    pub compressed: bool,
    /// When the wire log was compressed (`compressedAt`), when recorded.
    pub compressed_at: Option<chrono::DateTime<Utc>>,
    /// The wire log's pre-compression byte size (`preCompressedSize`),
    /// when recorded — the "savings" computation's basis.
    pub pre_compressed_size: Option<u64>,
    /// Lifecycle status of the session.
    ///
    /// `Running` for sessions that are live in the workspace, `Closed` for
    /// sessions that exist only on disk. Always `Running` when returned by
    /// [`SessionManager::list_sessions`] (which only lists active sessions).
    #[serde(default = "default_session_status")]
    pub status: SessionStatus,
    /// When the session was last "used" (event-delivery ADR-003 / SDR-014):
    /// bumped by a user send or a `session.touch`, never by turn settles.
    /// Live sessions carry the scope metadata's value; persisted rows carry
    /// the `state.json` value; **closed index rows are always `None`** (the
    /// SQLite index has no such column) — readers fall back to `updated_at`.
    pub last_used_at: Option<chrono::DateTime<Utc>>,
}

#[allow(dead_code)]
fn default_session_status() -> SessionStatus {
    SessionStatus::Running
}

/// The agent ID given to the single main agent recreated on resume.
///
/// Deterministic so that resuming the same session twice yields the same
/// agent ID (unlike `AgentLifecycle`'s UUID-per-create IDs).
pub const MAIN_AGENT_ID: &str = "main";

/// Manages the complete lifecycle of sessions within a workspace.
///
/// Each [`SessionManager`] is bound to one [`WorkspaceScope`] and owns a
/// [`SessionPersistence`] for on-disk state management. Session storage is
/// delegated to the workspace scope; the manager adds ID generation,
/// persistence, hooks, and lifecycle orchestration.
///
/// The manager also owns the per-session event plumbing: every session gets
/// its own [`SessionArtifacts`] — one [`EventBus`] (a broadcast channel for
/// [`SessionEvent`]s) and one set of [`ApprovalGate`] / [`QuestionGate`]s —
/// created fresh when the session is created or resumed, and drained through
/// [`SessionShutdown`] when the session closes. Because each session owns its
/// artifacts, events and gate requests never leak across sessions.
/// The replayed pending-steer pairs of a session (the F2 cache element): the
/// blob-rehydrated `(input, origin)` pairs a resume re-parks into an agent's
/// steer buffer.
type PendingSteerPairs = Vec<(
    Vec<crate::message::ContentPart>,
    crate::origin::PromptOrigin,
)>;

pub struct SessionManager {
    workspace: Arc<WorkspaceScope>,
    persistence: SessionPersistence,
    /// Injected filesystem provider shared with the persistence layer
    /// (zero-IO T13b; snapshot cleanup and session-dir removal route
    /// through it).
    io: Arc<dyn DynFileSystem>,
    /// Per-session event plumbing (event bus + approval/question gates),
    /// keyed by session ID. A live session always has exactly one entry; the
    /// entry is inserted on create/resume and removed on close.
    artifacts: RwLock<HashMap<SessionId, SessionArtifacts>>,
    /// The injected sync filesystem shared with the journal-version probe
    /// (zero-IO 契约 6): `probe_journal_version` runs on sync fence sites
    /// (the mid-turn guard closure, the pre-record write fence), so it
    /// needs the sync trait, not the async `DynFileSystem`.
    sync_io: Arc<dyn mirri_os::SyncFs>,
    /// The replayed pending-steer buffer of a just-resumed session — the
    /// F2 single-fold seam (ADR-017 "单一折叠结果"): [`resume_session_with_scope`]
    /// already folds the main log once and, on `land_resume_tail`, derives the
    /// still-parked steers (`turn.steer` minus `turn.steer_flushed`), blob-
    /// rehydrated through the session's `AgentBlobService` — the exact work the
    /// read face
    /// [`SessionManager::replayed_pending_steers`](crate::session::manager::restore::SessionManager::replayed_pending_steers)
    /// used to repeat with a SECOND full-log replay. The embedded resume
    /// assembly calls that face right after the resume returns; the short-lived
    /// result is cached here so the second replay (and its full-log parse)
    /// disappears entirely. Cleared when the session closes (a close→resume
    /// re-derives a fresh pair set), so the cache never serves a stale buffer
    /// past the session's death.
    pending_steers: RwLock<HashMap<SessionId, PendingSteerPairs>>,
    /// The per-CHILD pending-steer entries of a just-resumed session — the
    /// D2 single-scan seam: `restore_subagents` already reads every child
    /// journal for its turn-id ceiling scan, and derives each child's
    /// still-parked steer entries (`turn.steer` minus `turn.steer_flushed`)
    /// in the same pass. The embedded resume assembly's per-child read face
    /// [`SessionManager::replayed_pending_agent_steers`] takes the cached
    /// entries instead of re-reading and re-folding the whole journal —
    /// an N-child resume parsed N journals once, not twice. Cleared when
    /// the session closes, same lifetime as `pending_steers`.
    pending_agent_steers: RwLock<HashMap<(SessionId, String), Vec<serde_json::Value>>>,
    /// Sessions closed in this process (any close reason). A closed session
    /// must not silently resurrect on read paths within the same server run,
    /// while a restart (fresh manager, empty set) deliberately reopens
    /// persisted sessions on demand (restart survival). Cleared when the
    /// session is explicitly created or resumed again.
    closed_ids: std::sync::Mutex<std::collections::HashSet<String>>,
    /// The engine's live server-config snapshot, when a composition root
    /// wired one in. The close-time snapshot derives its fold's capacity
    /// limits (and the mirri-server restore handler its resume limits) from
    /// the `[loop_control]` keys here — the same shared `Arc` the hot-reload
    /// watcher swaps, so a reloaded config reaches both folds. `None` (a
    /// bare manager, e.g. in tests) keeps the default constants
    /// (10 000 / 1 000).
    server_config: Option<Arc<RwLock<ServerConfig>>>,
}

/// Build the [`SessionScopeConfig`] a server assembly derives from a
/// [`ServerConfig`] (B1 T4 — the embedded `session.create` /
/// `session.resume` assembly entry).
///
/// The task-lifecycle block maps the typed `[task]` / `[subagent]` sections:
/// the resolved per-subagent timeout (2h default, `0` = unbounded →
/// `None`), the background-task capacity, the kill-grace window,
/// `keepAliveOnExit`, and the `[subagent]` `maxNestingDepth` nesting
/// ceiling (B4 T5, 5 by default, `0` = unlimited). Everything else keeps
/// the [`SessionScopeConfig::default`] values — the embedder layers hooks and
/// trust flags on top through its own assembly.
pub fn session_scope_config_from(server_config: &ServerConfig) -> SessionScopeConfig {
    let task = server_config.resolve_task_config();
    let timeout_ms = server_config.resolve_subagent_timeout().as_millis() as u64;
    // The `[subagent]` `maxNestingDepth` (B4 T5 / SDR-B4-03, review SF-4):
    // the depth-guard refusal message names this knob, so it must actually
    // be reachable — `0` = unlimited, absent keeps the default of 5.
    let max_nesting_depth = server_config.resolve_subagent_max_nesting_depth();
    SessionScopeConfig {
        max_nesting_depth,
        task_lifecycle: TaskLifecycleSettings {
            subagent_timeout: if timeout_ms == 0 {
                None
            } else {
                Some(std::time::Duration::from_millis(timeout_ms))
            },
            // The `[subagent]` `maxSwarmItems` (SDR-012): the resolved
            // per-call AgentSwarm item cap (3 by default, `0` = unlimited);
            // the tool's env override is resolved per call, on top of this.
            max_swarm_items: Some(server_config.resolve_subagent_max_swarm_items()),
            max_running_tasks: task.max_running_tasks.unwrap_or(0) as usize,
            kill_grace: server_config.resolve_kill_grace(),
            keep_alive_on_exit: task.keep_alive_on_exit.unwrap_or(false),
            // The `[task]` `heartbeatIntervalMs`: the liveness window for
            // background-task staleness. `0` disables staleness; absent
            // keeps the 5-minute default.
            heartbeat_interval: match task.heartbeat_interval_ms {
                Some(0) => None,
                Some(ms) => Some(std::time::Duration::from_millis(ms)),
                None => Some(std::time::Duration::from_secs(300)),
            },
        },
        // D-34: the user-level `[[permission.rules]]` ride the session from
        // construction — every agent the session creates is stamped with
        // them (see SessionScope::create_agent).
        user_permission_rules: server_config
            .permission
            .as_ref()
            .and_then(|permission| permission.rules.clone())
            .unwrap_or_default(),
        // ADR-008 / D-4: the `[interaction]` question-channel wait ceiling,
        // a construction-time seed for new sessions (the
        // `default_permission_mode` precedent).
        interaction: InteractionSettings {
            question_channel_wait: server_config.resolve_question_channel_wait(),
        },
        // SDR-004 b: `default_permission_mode` is a seed for new sessions'
        // initial posture (and the UI's initial display) — parsed with the
        // same tolerant fallback the resume path uses for replayed modes.
        permission_mode: server_config
            .default_permission_mode
            .as_deref()
            .map(crate::session::manager::restore::parse_permission_mode)
            .unwrap_or_default(),
        ..SessionScopeConfig::default()
    }
}

impl SessionManager {
    /// Create a new session manager bound to the given workspace.
    ///
    /// The persistence directory is derived from the workspace's `cwd`:
    /// `<cwd>/.mirri/v3_sessions/` — the rust engine's own storage root
    /// (ADR-006-R2), physically separate from the TS engine's
    /// `<cwd>/.mirri/sessions` so cross-engine session sharing is
    /// impossible.
    pub fn new(workspace: Arc<WorkspaceScope>) -> Self {
        let sessions_dir = workspace.cwd().join(".mirri").join("v3_sessions");
        Self::with_persistence_dir(workspace, sessions_dir, Arc::new(LocalFs::new()))
    }

    /// Create a new session manager with an explicit sessions directory and
    /// filesystem provider.
    ///
    /// Use this when the sessions directory should not be derived from the
    /// workspace `cwd` (e.g. in tests or custom deployments). The `io`
    /// provider backs the persistence layer's directory enumeration, stat,
    /// and removal operations (zero-IO T13b; composition roots inject,
    /// `SessionManager::new` defaults to `LocalFs`).
    ///
    /// The persistence layer buckets every session under the TS-parity
    /// workspace key derived from the workspace `cwd`
    /// (`encode_work_dir_key`, see [`crate::session::workdir_key`]), so
    /// rust-written sessions share the TS kernel's per-workspace layout.
    pub fn with_persistence_dir(
        workspace: Arc<WorkspaceScope>,
        sessions_dir: PathBuf,
        io: Arc<dyn DynFileSystem>,
    ) -> Self {
        let workspace_key = encode_work_dir_key(workspace.cwd());
        Self {
            workspace,
            persistence: SessionPersistence::new(sessions_dir, Arc::clone(&io))
                .with_workspace_key(workspace_key),
            io,
            sync_io: Arc::new(mirri_os::LocalSyncFs::new()),
            artifacts: RwLock::new(HashMap::new()),
            pending_steers: RwLock::new(HashMap::new()),
            pending_agent_steers: RwLock::new(HashMap::new()),
            closed_ids: std::sync::Mutex::new(std::collections::HashSet::new()),
            server_config: None,
        }
    }

    /// Attach the engine's live server-config snapshot — the composition
    /// root's wiring, so the close-time snapshot and the mirri-server
    /// restore handler fold under the same `[loop_control]` capacity caps
    /// the session's live turns ran with (and a hot-reload swap reaches
    /// them). The shared `Arc` is stored, not copied, mirroring how every
    /// other config reader holds the snapshot.
    pub fn with_server_config(mut self, server_config: Arc<RwLock<ServerConfig>>) -> Self {
        self.server_config = Some(server_config);
        self
    }

    /// Resume-time capacity limits derived from the wired server-config
    /// snapshot's `[loop_control]` keys — the caps the mirri-server restore
    /// handler and the close-time snapshot fold under. `None` means no
    /// snapshot is wired (a bare manager); the fold then keeps the default
    /// constants (10 000 / 1 000), the historical behavior.
    pub fn replay_limits(&self) -> Option<mirri_proto::ReplayLimits> {
        let server_config = self.server_config.as_ref()?;
        let guard = server_config.read().unwrap_or_else(|e| e.into_inner());
        Some(guard.replay_limits())
    }

    /// The resume base config's model — the wired server-config snapshot's
    /// `default_model` alias (spec 11 R9): the no-record resume fallback
    /// chain equals the fresh-create chain (config default model → loud
    /// error), so the transport-owned `session.restore` handler seeds this
    /// into its resume base exactly the way the embedded host does.
    /// `None` (a bare manager without a wired snapshot) keeps the empty
    /// base — the resume then fails loudly with the same actionable error
    /// as any other unresolvable case.
    pub fn resume_default_model(&self) -> Option<String> {
        let server_config = self.server_config.as_ref()?;
        let guard = server_config.read().unwrap_or_else(|e| e.into_inner());
        guard.default_model.clone()
    }

    /// Access the underlying workspace scope.
    pub fn workspace(&self) -> &Arc<WorkspaceScope> {
        &self.workspace
    }

    /// Push a new user-level rule set into every live session of this
    /// workspace and every live agent of those sessions (D-34). The three
    /// writers — config load at session create/resume, the
    /// `approval.addRule` / `approval.removeRule` CRUD pipeline, and the
    /// config.toml hot-reload — all land here, which makes the push
    /// idempotent (a repeated same-value push is a harmless no-op in
    /// effect). Persisted-but-not-live sessions pick the rules up when they
    /// are next created or resumed.
    pub async fn apply_user_permission_rules(&self, rules: Vec<PermissionRule>) {
        for session_id in self.workspace.list_sessions().await {
            if let Some(session) = self.get_session(&session_id).await {
                session.set_user_permission_rules(rules.clone()).await;
            }
        }
    }

    /// Access the persistence layer.
    pub fn persistence(&self) -> &SessionPersistence {
        &self.persistence
    }

    /// Access the injected sync filesystem (zero-IO 契约 6): the sync-side
    /// journal-version probe (`probe_journal_version`) routes its stat
    /// through this, for callers on sync fence sites.
    pub fn sync_io(&self) -> &Arc<dyn mirri_os::SyncFs> {
        &self.sync_io
    }

    /// Access a session's event plumbing (event bus + approval/question
    /// gates).
    ///
    /// Returns `None` when the session has no artifacts installed, i.e. when
    /// it is not (or no longer) live. Each session owns a distinct artifact
    /// set, so consumers addressing a session must resolve it by ID through
    /// this method rather than through a manager-wide gate or bus.
    pub fn session_artifacts(&self, session_id: &SessionId) -> Option<SessionArtifacts> {
        self.artifacts
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(session_id)
            .cloned()
    }

    /// The replayed pending-steer pairs of a just-resumed session (F2), if
    /// the single main-log fold already derived them.
    pub fn take_pending_steers(&self, session_id: &SessionId) -> Option<PendingSteerPairs> {
        self.pending_steers
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id)
    }

    /// Cache the just-resumed session's replayed pending-steer pairs (the
    /// F2 single-fold writer side, called by `land_resume_tail`).
    pub fn put_pending_steers(&self, session_id: &SessionId, steers: PendingSteerPairs) {
        self.pending_steers
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session_id.clone(), steers);
    }

    /// Install the session's lazy subagent hydration seam (ADR-006/ADR-017) —
    /// called by the resume path once the session and its main agent exist, so
    /// a registered-but-dormant child is reconstructed from its journal on
    /// first use instead of at resume. The closure captures ONLY this manager's
    /// persistence layer (never the session or the manager itself — no
    /// lifetime cycle), so it is safe to hand to the session's
    /// [`SessionScope::install_subagent_hydrator`].
    pub fn install_subagent_hydrator(&self, session: &Arc<SessionScope>) {
        let persistence = self.persistence.clone();
        let hydrator: crate::scope::session_scope::SubagentHydrator =
            std::sync::Arc::new(move |host_session: &SessionScope, agent_id: &str| {
                let persistence = persistence.clone();
                let agent_id = agent_id.to_string();
                Box::pin(async move {
                    match restore::hydrate_subagent_from_journal(
                        host_session,
                        &persistence,
                        &agent_id,
                    )
                    .await
                    {
                        Ok(()) => true,
                        Err(error) => {
                            tracing::warn!(
                                session_id = %host_session.id(),
                                agent_id,
                                %error,
                                "lazy subagent hydration failed; the child stays dormant"
                            );
                            false
                        }
                    }
                })
                    as std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>
            });
        session.install_subagent_hydrator(Some(hydrator));
    }

    /// Install fresh event plumbing for a newly created or resumed session.
    ///
    /// The new session starts with an empty event bus and empty gates; no
    /// request or event from any earlier session can reach it. Lifecycle
    /// announcements are left to the caller, which wires its relay (or
    /// otherwise subscribes) before publishing.
    fn install_fresh_artifacts(
        &self,
        session_id: &SessionId,
        session: &Arc<SessionScope>,
    ) -> SessionArtifacts {
        // Alias the session's reverse-RPC slot into the artifacts (the late
        // subscribe fix): the transport installs the callback into
        // `SessionArtifacts::reverse_rpc_callback` at subscribe, and the
        // session's agents' `AskUserQuestion` tools resolve
        // `SessionScope::reverse_rpc_slot` at call time. One shared handle
        // means both views observe the same install — an agent created
        // BEFORE the first subscribe sees the callback the moment it lands.
        let artifacts = session.adopt_reverse_rpc_slot();
        // Metadata write-through driver (the mid-turn persistence gap): every
        // `SessionMetaUpdated` announcement (model title, user rename, tags)
        // rewrites `state.json` immediately instead of waiting for the turn
        // to settle — a process death mid-turn must not lose a title the UI
        // already displayed. The turn-settle write-through stays as the
        // recency backstop; both writes are full-snapshot overwrites, so they
        // never conflict.
        //
        // The task holds only a weak session reference: a strong one would
        // pin the scope (and its event bus) past close, so the receiver
        // would never observe `Closed` and the session would leak. When the
        // scope is gone the weak upgrade fails and the task exits. Best-
        // effort by design (ADR-009 semantics): a save failure logs and
        // never blocks the announcement. A lagged receiver heals by writing
        // the full current snapshot — the save is a snapshot overwrite, not a
        // delta, so a missed announcement is covered by the next one or by
        // the settle backstop. A lazily created, not-yet-materialized
        // session is skipped (SDR-004 — a never-conversed session leaves
        // nothing on disk).
        if tokio::runtime::Handle::try_current().is_ok() {
            let mut receiver = artifacts.event_bus.subscribe();
            let persistence = self.persistence.clone();
            let session_id = session_id.clone();
            let weak_session = Arc::downgrade(session);
            tokio::spawn(async move {
                loop {
                    match receiver.recv().await {
                        Ok(SessionEvent::SessionMetaUpdated { .. }) => {}
                        Err(broadcast::error::RecvError::Lagged(_)) => {}
                        Err(broadcast::error::RecvError::Closed) => break,
                        Ok(_) => continue,
                    }
                    let Some(session) = weak_session.upgrade() else {
                        break;
                    };
                    if !session.wire_log_installed().await {
                        continue;
                    }
                    let metadata = session.metadata().await;
                    if let Err(error) = persistence.save_state(&session_id, &metadata).await {
                        tracing::warn!(
                            session_id = %session_id,
                            %error,
                            "metadata write-through failed; the change stays in memory"
                        );
                    }
                }
            });
        }
        self.artifacts
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session_id.clone(), artifacts.clone());
        artifacts
    }

    /// Create a new session with auto-generated UUID ID.
    ///
    /// The session is registered in the workspace and its metadata is
    /// persisted to `state.json`. Returns the newly created [`SessionScope`].
    /// Fresh per-session event plumbing is installed: the new session starts
    /// with its own empty event bus and gates, isolated from every other
    /// session (see [`SessionManager::session_artifacts`]).
    pub async fn create_session(
        &self,
        metadata: SessionMetadata,
        config: SessionScopeConfig,
    ) -> SessionResult<Arc<SessionScope>> {
        let session_id = SessionId::new(format!("session-{}", Uuid::new_v4()));
        self.create_session_inner(session_id, metadata, config, Some("startup"), false)
            .await
    }

    /// Create a new session with **no physical persistence** (SDR-004 lazy
    /// physical creation): no session directory, no `state.json`, no
    /// `wire.jsonl` — the session exists in memory only, and every wire op
    /// recorded between creation and the first user message is buffered.
    /// [`Self::materialize_session`] (driven by the first `message.send`)
    /// writes the state, opens the wire log, and flushes the buffer — from
    /// that point on the session is byte-identical to an eagerly created one.
    ///
    /// A session that is never conversed leaves **nothing** on disk (the
    /// process exit is the discard).
    pub async fn create_session_lazy(
        &self,
        metadata: SessionMetadata,
        config: SessionScopeConfig,
    ) -> SessionResult<Arc<SessionScope>> {
        let session_id = SessionId::new(format!("session-{}", Uuid::new_v4()));
        self.create_session_inner(session_id, metadata, config, Some("startup"), true)
            .await
    }

    /// Create a new session with a specific ID.
    ///
    /// Returns [`SessionError::AlreadyExists`] if a session with the given ID
    /// already exists in the workspace. Fresh per-session event plumbing is
    /// installed (see [`SessionManager::create_session`]).
    pub async fn create_session_with_id(
        &self,
        session_id: SessionId,
        metadata: SessionMetadata,
        config: SessionScopeConfig,
    ) -> SessionResult<Arc<SessionScope>> {
        self.create_session_inner(session_id, metadata, config, Some("startup"), false)
            .await
    }

    /// Shared session-creation path for [`create_session`](Self::create_session)
    /// and [`create_session_with_id`](Self::create_session_with_id).
    ///
    /// `session_start_source` is the value carried in the `SessionStart`
    /// external hook's `{ source }` input; `None` skips the hook (used by
    /// forks, mirroring the TS behavior of excluding `'fork'` sessions from
    /// `SessionStart`).
    ///
    /// `lazy` selects the SDR-04 deferred-persistence path (see
    /// [`Self::create_session_lazy`]); the default `false` keeps the eager
    /// create-persist-at-once behaviour every existing caller relies on.
    async fn create_session_inner(
        &self,
        session_id: SessionId,
        mut metadata: SessionMetadata,
        config: SessionScopeConfig,
        session_start_source: Option<&str>,
        lazy: bool,
    ) -> SessionResult<Arc<SessionScope>> {
        // Creating (or re-creating under a fixed id) clears any close
        // record for this session — it is live again.
        self.closed_ids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id.as_str());

        // Record the session's workDir (TS parity): the workspace cwd the
        // session is rooted at, persisted into state.json so listings and
        // the legacy-layout migration can attribute the session to its
        // workspace bucket. Seeded only when the caller did not set one.
        if metadata.work_dir.is_none() {
            metadata.work_dir = Some(self.workspace.cwd().to_string_lossy().into_owned());
        }

        // The session's background tasks persist under `<session_dir>/tasks/`
        // (per-task JSON + append-only output logs), mirroring the TS
        // kernel's session-level task layout.
        let tasks_dir = self.persistence.session_dir(&session_id).join("tasks");
        let session = self
            .workspace
            .create_session_with_config(
                session_id.clone(),
                metadata.clone(),
                config,
                Some(tasks_dir),
            )
            .await
            .map_err(|e| SessionError::AlreadyExists(e.to_string()))?;

        if lazy {
            // SDR-004 lazy physical creation: no state write, no wire log —
            // the slot defers, buffering every op recorded from here until
            // [`Self::materialize_session`] runs (the first user message).
            // A never-conversed session therefore leaves no directory and
            // no files behind when the process exits.
            session.defer_wire_log().await;
        } else {
            // Persist session state.
            self.persistence.save_state(&session_id, &metadata).await?;

            // Open the session's wire log for live recording, so every op
            // produced during the session's life (turn prompts, messages,
            // loop events, usage) is durable — `session.resume` replays this
            // file. A session that cannot persist must not start silent:
            // fail the create rather than run without a log.
            let wire_log_path = self.persistence.wire_log_path(&session_id);
            let wire_log = JsonlAppendLog::create(&wire_log_path).await.map_err(|e| {
                SessionError::WireLog(format!(
                    "failed to open {} for writing: {e}",
                    wire_log_path.display()
                ))
            })?;
            session.install_wire_log(wire_log).await;
        }
        // First record: the metadata envelope, marking the log's protocol
        // version and birth. TS kernel parity — every `wire.jsonl` opens
        // with a `metadata` line (the reducer ignores it; migration detects
        // the log's version from it). On the lazy path this lands in the
        // deferred buffer and flushes at materialization.
        session
            .record_op(WireOp::Metadata {
                protocol_version: Some(mirri_persistence::CURRENT_VERSION.to_string()),
                created_at: Some(Utc::now().timestamp_millis() as u64),
            })
            .await;

        // Install fresh per-session event plumbing; the new session starts
        // with no pending approval or question requests and its own bus.
        // Wire the session's task manager onto the fresh bus so background
        // task lifecycle events reach the backends' event projection (the
        // same seam the embedded layer uses for the turn flow's bus).
        let artifacts = self.install_fresh_artifacts(&session_id, &session);
        session
            .tasks()
            .set_event_bus(Arc::clone(&artifacts.event_bus));
        // Wire the session's cron manager onto the same fresh bus so fired
        // cron tasks publish their `cron.fired` events (the same seam the
        // task manager uses).
        session
            .cron()
            .set_event_bus(Arc::clone(&artifacts.event_bus));

        // Fire the SessionStart external-command hook (TS input shape:
        // `{ source }`, source is `'startup' | 'resume'`). Fire-and-forget: a
        // block decision does not prevent session creation.
        if let Some(source) = session_start_source {
            let _ = fire_session_start_hook(&session, source).await;
        }

        Ok(session)
    }
}

impl SessionManager {
    /// Create a snapshot of the current agent state for fast resume.
    ///
    /// The snapshot records how many wire ops have been folded into `state`;
    /// a later resume loads the snapshot and replays only the ops after this
    /// checkpoint. Written atomically via
    /// [`mirri_persistence::SnapshotStore`], so a crash never leaves a
    /// partially written snapshot.
    pub async fn create_snapshot(
        &self,
        session_id: &SessionId,
        state: &AgentState,
        op_count: usize,
    ) -> SessionResult<()> {
        let snapshot_path = self.persistence.snapshot_path(session_id);
        SnapshotStore::write(&snapshot_path, state, op_count as u64)
            .await
            .map_err(|e| {
                SessionError::Snapshot(format!("failed to write {}: {e}", snapshot_path.display()))
            })?;
        tracing::info!(
            session_id = %session_id,
            snapshot = %snapshot_path.display(),
            op_count,
            "state snapshot written"
        );
        Ok(())
    }

    /// Write a state snapshot for a session being closed so a later resume
    /// can start from a checkpoint.
    ///
    /// Replays the session's wire log and snapshots the final state. The
    /// fold runs under the capacity limits derived from the wired
    /// server-config snapshot (`[loop_control]`, via
    /// [`Self::replay_limits`]) — the same caps the resume path threads, so
    /// a close-time checkpoint and a later resume agree on which appends
    /// the capacity checks rejected. A manager without a wired snapshot
    /// folds under the default constants (10 000 / 1 000), the historical
    /// behavior. When the log is missing or empty, no snapshot is written
    /// — and a stale snapshot from an earlier close is removed so it
    /// cannot be mistaken for current state.
    async fn snapshot_session_on_close(&self, session_id: &SessionId) -> SessionResult<()> {
        let wire_log_path = self.persistence.wire_log_path(session_id);
        if !wire_log_path.exists() {
            // zero-io-allow: baseline Path IO, see F-T13-2
            return Ok(());
        }
        let replay = self
            .replay_agent_state(session_id, self.replay_limits())
            .await?;
        let (state, op_count) = (replay.state, replay.op_count);
        if op_count == 0 {
            let snapshot_path = self.persistence.snapshot_path(session_id);
            if snapshot_path.exists() {
                // zero-io-allow: baseline Path IO, see F-T13-2
                self.io.remove_file(&snapshot_path).await.map_err(|e| {
                    SessionError::Snapshot(format!(
                        "failed to remove stale {}: {e}",
                        snapshot_path.display()
                    ))
                })?;
            }
            return Ok(());
        }
        self.create_snapshot(session_id, &state, op_count).await
    }

    /// Fork a session from an existing agent's context (F2 pipeline).
    ///
    /// Creates a new session with auto-generated ID, copies the source
    /// agent's context memory into a new agent in the new session, and
    /// persists the new session's state. The persisted history follows
    /// the fork pipeline: the source session's main wire log is read,
    /// truncated to the last completed turn (the default) or an explicit
    /// [`ForkOptions::turn_id`] anchor, filtered through the fork table
    /// (`fork_pipeline`), and written verbatim into the new session's
    /// `wire.jsonl` — the source log is never touched (append-only).
    ///
    /// The source session and agent are not modified.
    pub async fn fork_session(
        &self,
        source_session: &Arc<SessionScope>,
        source_agent_id: &AgentId,
        new_metadata: SessionMetadata,
    ) -> SessionResult<Arc<SessionScope>> {
        self.fork_session_with_options(
            source_session.id(),
            ForkOptions::default(),
            source_agent_id,
            new_metadata,
        )
        .await
    }

    /// [`Self::fork_session`] with the F2 fork options.
    ///
    /// The source session is addressed by ID (the pipeline reads its
    /// persisted wire log; `source_agent_id` keeps its historical role —
    /// the live agent whose config seeds the fork's agent, and the
    /// in-memory deep-copy source). See [`ForkOptions`] for the anchor
    /// semantics.
    pub async fn fork_session_with_options(
        &self,
        source_session_id: &SessionId,
        options: ForkOptions,
        source_agent_id: &AgentId,
        new_metadata: SessionMetadata,
    ) -> SessionResult<Arc<SessionScope>> {
        let source_session = self
            .workspace
            .get_session(source_session_id)
            .await
            .ok_or_else(|| SessionError::NotFound(source_session_id.to_string()))?;

        // Read the source session's main wire log ops and run them through
        // the fork pipeline (truncate → filter). The read is the same
        // in-memory migration chain every replay uses (restore.rs), so
        // logs written by older protocols fork equivalently. A source
        // without a readable log forks with an empty history (the source
        // never conversed — or was never materialized).
        let forked_ops = self
            .read_source_ops_for_fork(source_session_id, options.turn_id)
            .await?;

        // Get the source agent — resolved EARLY, before the target session
        // is minted, so a bad agent id never leaves a half-landed fork to
        // roll back (fail-fast; nothing exists to clean up yet).
        let source_agent = source_session
            .get_agent(source_agent_id)
            .await
            .ok_or_else(|| SessionError::AgentNotFound(source_agent_id.to_string()))?;

        // F3: mint the fork's lineage (v2 `state.json` `forkedFrom`) —
        // set BEFORE the create so the eager state write carries it (the
        // caller-provided value wins: the pipe never overwrites what the
        // embedded layer already derived).
        let mut new_metadata = new_metadata;
        if new_metadata.forked_from.is_none() {
            new_metadata.forked_from = Some(source_session_id.as_str().to_string());
        }
        // Cache-key lineage: the fork routes its provider prompt-cache key at
        // the fork chain's ROOT — a nested fork must keep hitting the same
        // cache shard the original session warmed. The source's persisted
        // cache root wins; a source without one (fresh or legacy) is itself
        // the root. Caller-supplied values are never overwritten here: the
        // embedded layer may have derived lineage already. A source whose
        // state.json cannot be loaded leaves the fork WITHOUT a cache root —
        // no fallback mint on an unreadable source (the honest state is
        // "unknown"; a wrong root would silently miss the cache anyway).
        if new_metadata.cache_root_id.is_none() {
            new_metadata.cache_root_id = self
                .persistence
                .load_state(source_session_id)
                .await
                .ok()
                .map(|source| {
                    crate::session::fork_pipeline::resolve_fork_cache_root(
                        source.cache_root_id.as_deref(),
                        source_session_id.as_str(),
                    )
                });
        }
        // F3: merge the fork's custom dict — the v2 `forkCustomMetadata`
        // semantics: the SOURCE's persisted `custom` is the base, the
        // caller-supplied dict overrides, and the `goal` key is dropped
        // from both sides (a fork's goal lifecycle restarts fresh — the
        // same rule the wire-side filter applies to `goal.update`).
        let source_custom = self
            .persistence
            .load_state(source_session_id)
            .await
            .ok()
            .and_then(|source| source.custom);
        new_metadata.custom = crate::session::fork_pipeline::merge_fork_custom(
            source_custom.as_ref(),
            new_metadata.custom.as_ref(),
        );

        // Create the new session. No `SessionStart` hook is fired for forks —
        // the TS hook system excludes `'fork'` from its `SessionCreateSource`.
        // A fork carries the source session's conversation history, so it is
        // "conversed" by construction: the eager (persisted) path. D-34: the
        // fork inherits the SOURCE session's current user-level rules — a
        // user deny rule must not silently vanish across a fork.
        //
        // The rest of the source's scope config inherits too (the same
        // posture the resume path applies via `resume_session_with_scope`):
        // task-lifecycle knobs (`[task]` heartbeat window, capacity, kill
        // grace, subagent timeout) must survive a fork, or the fork's
        // stall-watchdog plumbing degrades to the defaults — a fork from a
        // session running a tightened `heartbeatIntervalMs` would fall back
        // to the 5-minute window, and the fork's background-task heartbeat
        // ticker would read as silent (starving the turn's watchdog feed).
        //
        // `hooks` is the ONE field that must NOT inherit: the source's
        // persisted `hooks` already contains the workspace's live hooks
        // (pre-chained at create time by `create_session_with_config`), and
        // the fork's own create passes through the same pre-chaining — an
        // inherited list would double-register every workspace hook
        // (`register_all` is a plain extend). Emptying it here lets the
        // fork chain the workspace's CURRENT live hooks exactly once (the
        // resume path's posture: `session_scope_config_from` builds the
        // config with no hooks and lets the workspace link them in).
        let fork_scope_config = SessionScopeConfig {
            user_permission_rules: source_session.user_permission_rules().await,
            hooks: Vec::new(),
            ..source_session.config().clone()
        };
        // Atomic landing (F4, design §3.5): every step after the target
        // session is minted is fallible, so the remainder of this method
        // runs under a rollback guard — on ANY error the target is removed
        // from the workspace map, its artifacts drained, and its directory
        // deleted, leaving zero residue. The guard disarms itself right
        // before the success return.
        match self
            .land_fork_pipeline(
                source_session,
                source_agent,
                &forked_ops,
                new_metadata,
                fork_scope_config,
            )
            .await
        {
            Ok((new_session, appended_ops)) => {
                tracing::info!(
                    source_session_id = %source_session_id,
                    new_session_id = %new_session.id(),
                    anchor = ?options.turn_id,
                    appended_ops,
                    "session forked"
                );
                Ok(new_session)
            }
            Err(error) => {
                tracing::warn!(
                    source_session_id = %source_session_id,
                    %error,
                    "fork pipeline failed; rolling the target session back"
                );
                Err(error)
            }
        }
    }

    /// Land the forked session after its directory was minted: publish the
    /// fork event on the source bus, replay the truncated history into the
    /// new wire log, and rebuild the live agent from the same stream.
    ///
    /// This is the fallible tail of [`Self::fork_session_with_options`]; the
    /// caller wraps it in the atomic-landing rollback, so any error here
    /// tears the target down to zero residue (see
    /// [`Self::rollback_failed_fork`]).
    #[allow(clippy::too_many_arguments)]
    async fn land_fork_pipeline(
        &self,
        source_session: Arc<SessionScope>,
        source_agent: Arc<AgentScope>,
        forked_ops: &[WireOp],
        new_metadata: SessionMetadata,
        fork_scope_config: SessionScopeConfig,
    ) -> SessionResult<(Arc<SessionScope>, usize)> {
        let new_session = self
            .create_session_inner(
                SessionId::new(format!("session-{}", Uuid::new_v4())),
                new_metadata,
                fork_scope_config,
                None,
                false,
            )
            .await?;

        // From here on, every failure must roll the target back. The guard
        // covers create_session_inner's own tail too — the workspace insert
        // succeeded but a persistence step can still fail past it.
        let result = self
            .land_fork_tail(&source_session, &source_agent, forked_ops, &new_session)
            .await;

        match result {
            Ok(appended_ops) => Ok((new_session, appended_ops)),
            Err(error) => {
                // Atomic landing: the target session is torn down —
                // workspace registration, artifacts, and directory — so a
                // failed fork leaves no half-built session behind (design
                // §3.5). Best-effort cleanup steps log and continue: the
                // residue of a partial rollback is an orphaned directory at
                // worst, never a live-but-broken session.
                self.rollback_failed_fork(new_session.id()).await;
                Err(error)
            }
        }
    }

    /// Replay the forked history into the landed target and rebuild its
    /// live agent — the steps between session creation and fork success.
    async fn land_fork_tail(
        &self,
        source_session: &Arc<SessionScope>,
        source_agent: &Arc<AgentScope>,
        forked_ops: &[WireOp],
        new_session: &Arc<SessionScope>,
    ) -> SessionResult<usize> {
        // Publish on the source session's bus: it is the only session with
        // existing subscribers at fork time (the new session's bus was just
        // created and no client can have subscribed to it yet).
        if let Some(source_artifacts) = self.session_artifacts(source_session.id()) {
            source_artifacts
                .event_bus
                .publish(SessionEvent::SessionForked {
                    source_session_id: uuid_from_id(source_session.id().as_str()),
                    new_session_id: uuid_from_id(new_session.id().as_str()),
                });
        }

        // Pipeline the forked history into the new session's wire log.
        // The eager create already recorded a `metadata` header
        // (`create_session_inner` always opens the log with one), so the
        // stream's own header — when present — is dropped instead of
        // duplicated; the fork's header carries the CURRENT protocol
        // version, which is what a freshly created log should advertise.
        // F5: per-member journals (agents/<id>/wire.jsonl) are out of
        // scope here — the agents registry to enumerate them (state.json
        // `agents` field) lands with F5; until then the forked session
        // rehydrates children from the replayed `subagent.registered`
        // entries the same way a resume does.
        let mut appended_ops = 0usize;
        // Bulk-write the forked history: one batched append (single
        // fsync) instead of one fsync per op — a fork replays thousands
        // of ops and the per-op durability barrier dominated its runtime
        // (measured ~3.5ms/op on local SSD). The eager create already
        // recorded a `metadata` header, so the stream's own header — when
        // present — is dropped instead of duplicated.
        let mut batch: Vec<WireOp> = Vec::with_capacity(forked_ops.len());
        for op in forked_ops {
            if appended_ops == 0 && matches!(op, WireOp::Metadata { .. }) {
                continue; // replaced by the create-time header
            }
            batch.push(op.clone());
            appended_ops += 1;
        }
        // appended_ops == 0 is the legal empty-history fork: the new log
        // holds exactly the create-time metadata header.
        if !batch.is_empty() {
            let wire_log_path = self.persistence.wire_log_path(new_session.id());
            // The batch goes through the session's OWN installed writer —
            // one handle owns the file. A second, append-mode handle here
            // would desynchronize the installed writer's fd offset and the
            // session's next live op would overwrite the forked history
            // (the mid-file corruption the wire-log reader reports as
            // `Corrupted`).
            new_session.record_ops(&batch).await.map_err(|e| {
                SessionError::WireLog(format!(
                    "failed to batch-append the fork history to {}: {e}",
                    wire_log_path.display()
                ))
            })?;
        }

        // F5: entity disposition — the source's remaining forkable
        // entities land in the target now: per-member journals (each
        // truncated to the member's OWN latest completed turn — member
        // turn ids are independent of the main log's), the blobs
        // directory (full copy), and the TS agents manifest (carried
        // verbatim into the minted state.json). Every step is fallible
        // under the same atomic-landing rollback as the steps above.
        self.copy_fork_entities(source_session.id(), forked_ops, new_session)
            .await?;

        // Seed the fork's turn-id allocator past the truncated history's
        // maximum (the same floor a later resume would re-derive from this
        // log): the forked ops carry the source's `turn.ended` ids verbatim,
        // so a fresh counter starting at 1 would re-mint ids the fork's own
        // wire log already holds.
        if let Some(artifacts) = self.session_artifacts(new_session.id()) {
            artifacts
                .bump_turn_id_floor(crate::session::turn_usage::wire_log_max_turn_id(forked_ops));
        }

        // Fork the agent into the new session. The live in-memory context
        // is rebuilt from the SAME truncated stream the persisted log got
        // (fold the forked ops into a fresh AgentState, reconstruct the
        // memory — the identical rebuild a later resume performs), so the
        // live fork and the persisted fork can never disagree: no
        // source-live deep copy is taken at all. This replaces F2's
        // interim deep-copy approximation: the source's in-memory tail
        // (which the wire pipeline cut away) never reaches the fork.
        let agent_lifecycle = AgentLifecycle::new(Arc::clone(new_session));
        let mut source_config = source_agent
            .config()
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        // The agent is being created in the FORK session, so its session-
        // scoped plumbing must follow: the source config's `event_bus` points
        // at the SOURCE session's bus, and inheriting it verbatim would send
        // the fork's goal/skill tool announcements (`event.goal.updated` /
        // `event.skill.activated`) onto the source's subscribers — the fork's
        // own clients would never see them live (the same "buses are never
        // reused" rule the resume path applies at restore time). The fork's
        // artifacts were installed by the eager create above, so the fork's
        // bus is always available here. The interaction kernel gets the same
        // treatment: it binds the agent's ask flows to the session's gates,
        // and the source's kernel would register the fork's pending
        // approvals/questions on the SOURCE session's gates (the fork's
        // clients could never see or answer them).
        if let Some(fork_artifacts) = self.session_artifacts(new_session.id()) {
            source_config.event_bus = Some(Arc::clone(&fork_artifacts.event_bus));
            source_config.interaction_kernel = Some(Arc::new(crate::InteractionKernel::new(
                Arc::clone(&fork_artifacts.approval_gate),
                Arc::clone(&fork_artifacts.question_gate),
            )));
        } else {
            // A fork without artifacts cannot announce or gate anywhere —
            // keep the tools silent rather than leaking onto the source's
            // bus and gates.
            source_config.event_bus = None;
            source_config.interaction_kernel = None;
        }
        agent_lifecycle
            .fork_agent_with_memory(source_agent, source_config, || {
                let mut state = AgentState::new();
                for op in forked_ops {
                    mirri_proto::apply_with_limits(&mut state, op, None);
                }
                reconstruct_context_memory(&state)
            })
            .await?;

        Ok(appended_ops)
    }

    /// Copy the source session's remaining forkable entities into the
    /// landed target (F5, design §3.3's file-entity table):
    ///
    /// - **Per-member journals** — for every member the forked MAIN ops
    ///   register (`subagent.registered`; the same registry a resume
    ///   rebuilds children from), the member's own
    ///   `agents/<id>/wire.jsonl` is read through the standard in-memory
    ///   migration chain, truncated to the member's OWN latest completed
    ///   turn (member turn ids are independent of the main log's — the
    ///   journal_route.rs split semantics), filtered through the same
    ///   fork table, and written into the target. A member without a
    ///   journal is skipped (the restore face's own tolerance).
    /// - **blobs/** — full copy. Blob payloads are content-addressed by
    ///   the `blobref:` URIs the carried history references, so a fork
    ///   missing one cannot rehydrate; the blob set is small and the
    ///   copy is the simplest correct disposition.
    /// - **TS agents manifest** — the source `state.json`'s `agents` dict
    ///   is carried verbatim (member ids unchanged); a source without
    ///   one mints none.
    ///
    /// `tasks/` and `snapshot.json` are deliberately NOT copied: task
    /// lifecycle state is live-run-scoped (a fork restarts its own task
    /// history from the wire replay) and a snapshot describes the SOURCE
    /// log's folded offset — both would be lies in the target.
    ///
    /// Runs under the caller's atomic-landing rollback: any error here
    /// tears the whole target down.
    async fn copy_fork_entities(
        &self,
        source_session_id: &SessionId,
        forked_ops: &[WireOp],
        new_session: &Arc<SessionScope>,
    ) -> SessionResult<()> {
        // Members come from the forked ops themselves (zero directory
        // enumeration): the registry the resume face rebuilds children
        // from is exactly the `subagent.registered` entries the main
        // log's replay folds.
        let mut member_ids: Vec<String> = Vec::new();
        for op in forked_ops {
            if let WireOp::SubagentRegistered { agent_id, .. } = op
                && !member_ids.contains(agent_id)
            {
                member_ids.push(agent_id.clone());
            }
        }
        for member_id in &member_ids {
            self.fork_member_journal(source_session_id, member_id, new_session.id())
                .await?;
        }

        self.copy_fork_blobs(source_session_id, new_session.id())
            .await?;

        // TS agents manifest: carried verbatim (member ids unchanged —
        // the ids the member journals above landed under).
        let manifest = self
            .persistence
            .read_ts_agents_manifest(source_session_id)
            .await?;
        self.persistence
            .write_ts_agents_manifest(new_session.id(), manifest.as_ref())
            .await?;
        Ok(())
    }

    /// Truncate and copy ONE member's journal into the fork (F5).
    ///
    /// The anchor is the member's OWN latest `turn.ended` — the
    /// member-level default of the main log's rule. A member journal
    /// with no completed turn carries only its turn-less prefix (the
    /// metadata header ahead of the first `turn.prompt`); a member with
    /// no journal at all is skipped with a warning (best-effort, the
    /// restore face's own tolerance — the main fork's history is
    /// unaffected).
    async fn fork_member_journal(
        &self,
        source_session_id: &SessionId,
        member_id: &str,
        target_session_id: &SessionId,
    ) -> SessionResult<()> {
        let journal_path = self
            .persistence
            .agent_wire_log_path(source_session_id, member_id);
        if !journal_path.exists() {
            // zero-io-allow: baseline Path IO, see F-T13-2
            tracing::warn!(
                session_id = %source_session_id,
                member_id,
                journal = %journal_path.display(),
                "member journal missing on fork; the member is not carried"
            );
            return Ok(());
        }
        let migrated_records = migrate_records_in_memory(&journal_path)
            .await
            .map_err(|e| {
                SessionError::WireLog(format!("failed to read {}: {e}", journal_path.display()))
            })?;
        let ops = restore::migrated_records_to_wire_ops(migrated_records, &journal_path)?;

        // Member-level anchor: the member's own latest completed turn.
        // No completed turn → the turn-less prefix only (the member's
        // open bracket never carries — the same rule the main log's
        // default anchor applies).
        let member_ops = match ops.iter().rev().find_map(|op| {
            if let WireOp::TurnEnded { turn_id, .. } = op {
                Some(*turn_id)
            } else {
                None
            }
        }) {
            Some(latest) => crate::session::fork_pipeline::truncate_ops_at_turn(&ops, latest)?,
            None => {
                let prefix_end = ops
                    .iter()
                    .position(|op| matches!(op, WireOp::TurnPrompt { .. }))
                    .unwrap_or(ops.len());
                ops[..prefix_end].to_vec()
            }
        };
        let forked_member_ops = crate::session::fork_pipeline::filter_forked_ops(member_ops);

        // Land the member journal in the target. The path computation
        // creates no directories; the append log's create does.
        let target_path = self
            .persistence
            .agent_wire_log_path(target_session_id, member_id);
        let mut log = JsonlAppendLog::create(&target_path).await.map_err(|e| {
            SessionError::WireLog(format!(
                "failed to open {} for writing: {e}",
                target_path.display()
            ))
        })?;
        // Batch the member journal the same way the main log batches —
        // one fsync for the whole truncated stream. The returned byte
        // count is unused here (callers only need the append's success).
        log.append_batch(&forked_member_ops).await.map_err(|e| {
            SessionError::WireLog(format!(
                "failed to append to {}: {e}",
                target_path.display()
            ))
        })?;
        Ok(())
    }

    /// Copy the source session's `blobs/` directory into the target (F5).
    ///
    /// Full copy through the injected [`DynFileSystem`] (zero-IO T13b):
    /// enumerate the source's blob entries, copy each file across. A
    /// source without a `blobs/` directory is a no-op. Sub-directories
    /// (none are produced by the blob store's flat layout) are skipped —
    /// the store writes blobs as `blobs/<sha256>` flat files.
    async fn copy_fork_blobs(
        &self,
        source_session_id: &SessionId,
        target_session_id: &SessionId,
    ) -> SessionResult<()> {
        let source_blobs = self
            .persistence
            .session_dir(source_session_id)
            .join("blobs");
        let entries = match self.io.iterdir(&source_blobs).await {
            Ok(entries) => entries,
            Err(_) => return Ok(()), // no blobs directory — nothing to copy
        };
        let mut copied = 0usize;
        for entry in entries {
            if !entry.is_file {
                continue;
            }
            let from = source_blobs.join(&entry.name);
            let to = self
                .persistence
                .session_dir(target_session_id)
                .join("blobs")
                .join(&entry.name);
            self.io.copy(&from, &to).await.map_err(|e| {
                SessionError::StateFile(format!(
                    "failed to copy blob {} into the fork: {e}",
                    from.display()
                ))
            })?;
            copied += 1;
        }
        if copied > 0 {
            tracing::debug!(
                source_session_id = %source_session_id,
                target_session_id = %target_session_id,
                copied,
                "forked session blobs copied"
            );
        }
        Ok(())
    }

    /// Tear a failed fork's target down to zero residue (atomic landing,
    /// design §3.5): remove the session from the workspace map, drain and
    /// drop its artifacts, and delete its on-disk directory. Best-effort —
    /// each step logs its failure and the cleanup continues, because the
    /// alternative (returning the cleanup error instead of the pipeline
    /// error) would mask why the fork failed; the worst residue of a
    /// partial rollback is an orphaned directory, never a live-but-broken
    /// session. The SOURCE session is never touched.
    async fn rollback_failed_fork(&self, session_id: &SessionId) {
        // 1. Workspace map: the session must stop resolving as live.
        self.workspace.remove_session(session_id).await;
        // 2. Manager-side bookkeeping: the artifacts entry (bus + gates) —
        //    drained exactly like a close, so no pending gate future or
        //    bus subscriber of the failed target is left hanging — and the
        //    closed-id record, so read paths never resurrect the failure.
        //    The map entry is taken OUT of the lock before any await (the
        //    guard must not live across the async shutdown drain).
        let artifacts = self
            .artifacts
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
        if let Some(artifacts) = artifacts {
            SessionShutdown::new(&artifacts).shutdown().await;
        }
        self.closed_ids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session_id.as_str().to_string());
        // 3. Discard the F2 pending-steer caches: the target session never
        //    becomes live, so its replayed steer pairs must not linger.
        self.pending_steers
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
        self.pending_agent_steers
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(cached, _), _| cached != session_id);
        // 4. On-disk directory: delete the minted files wholesale through
        //    the persistence layer (best-effort, missing dir is a no-op).
        if let Err(error) = self.persistence.delete_session_files(session_id).await {
            tracing::warn!(
                session_id = %session_id,
                %error,
                "failed fork rollback could not delete the target directory"
            );
        }
    }

    /// Read the source session's main wire log and run it through the fork
    /// pipeline (truncate → filter).
    ///
    /// `turn_id: Some(n)` truncates right after that turn's `turn.ended`
    /// (missing → [`SessionError::ForkTargetNotFound`] with the available
    /// range). `None` anchors at the MOST RECENT completed turn — the
    /// last `turn.ended` in the log — so an active turn's open bracket
    /// and any stray tail records are cut; a log with NO completed turn
    /// forks with an empty history (a legal empty fork). A source without
    /// a readable wire log (never conversed / never materialized) also
    /// forks with an empty history.
    async fn read_source_ops_for_fork(
        &self,
        source_session_id: &SessionId,
        turn_id: Option<u64>,
    ) -> SessionResult<Vec<WireOp>> {
        // Retention fork rejection (SDR-003 / §3.3): the earliest entry
        // point before the source's wire log is read — a compressed source
        // fails with the actionable business error instead of producing a
        // fork from a shape the reader would have to guess about. The
        // source must be restored (decompressed) first.
        crate::session::retention::reject_compressed_fork_source(
            &self.persistence.session_dir(source_session_id),
        )?;
        let wire_log_path = self.persistence.wire_log_path(source_session_id);
        if !wire_log_path.exists() {
            // zero-io-allow: baseline Path IO, see F-T13-2
            return Ok(Vec::new());
        }
        let migrated_records = migrate_records_in_memory(&wire_log_path)
            .await
            .map_err(|e| {
                SessionError::WireLog(format!("failed to read {}: {e}", wire_log_path.display()))
            })?;
        let ops = restore::migrated_records_to_wire_ops(migrated_records, &wire_log_path)?;

        // Resolve the anchor: an explicit turn, else the most recent
        // completed turn. A log with no completed turn forks with its
        // turn-less prefix only (the metadata header and the config
        // records ahead of the first turn.prompt — a legal empty-history
        // fork whose model record must survive).
        let anchor = match turn_id {
            Some(target) => target,
            None => match ops.iter().rev().find_map(|op| {
                if let WireOp::TurnEnded { turn_id, .. } = op {
                    Some(*turn_id)
                } else {
                    None
                }
            }) {
                Some(latest) => latest,
                None => {
                    let prefix_end = ops
                        .iter()
                        .position(|op| matches!(op, WireOp::TurnPrompt { .. }))
                        .unwrap_or(ops.len());
                    return Ok(crate::session::fork_pipeline::filter_forked_ops(
                        ops[..prefix_end].to_vec(),
                    ));
                }
            },
        };

        let truncated = crate::session::fork_pipeline::truncate_ops_at_turn(&ops, anchor)?;
        Ok(crate::session::fork_pipeline::filter_forked_ops(truncated))
    }

    /// Close a session, removing it from the workspace.
    ///
    /// The session's on-disk files are **not** deleted by default — they
    /// remain available for future resume. Use
    /// [`SessionPersistence::delete_session_files`] if you want to remove
    /// them permanently. The session's background machinery is stopped: its
    /// cron scheduler is shut down (no further fires) and its running
    /// background tasks are marked stopped. Only this session's gates are
    /// drained through [`SessionShutdown`] — pending requests of other
    /// sessions are untouched — and its event bus is dropped, so no
    /// approval/question future of this session is left hanging.
    ///
    /// Returns [`SessionError::NotFound`] if the session does not exist
    /// (in which case nothing is drained and no event is published).
    pub async fn close_session(
        &self,
        session_id: &SessionId,
        reason: CloseReason,
    ) -> SessionResult<()> {
        let session = self
            .workspace
            .remove_session(session_id)
            .await
            .ok_or_else(|| SessionError::NotFound(session_id.to_string()))?;

        // Record the close so read paths in this process do not silently
        // resurrect the session (an explicit resume clears it again).
        self.closed_ids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session_id.as_str().to_string());

        // Fire the SessionEnd external-command hook (TS input shape:
        // `{ reason }`). Fired before teardown so the hook can still observe
        // the session scope. Fire-and-forget: a block decision does not
        // prevent the close.
        let _ = fire_session_end_hook(&session, &reason).await;

        // Stop the session's background machinery before the session scope's
        // last strong reference drops: cron fires are no longer injected,
        // the session's interactive terminals are killed (best-effort — a
        // PTY failure must not prevent the close), and the background tasks
        // converge under the kill-grace window (D-63/D-64): every attached
        // cancel signal fires, tasks get `kill_grace` to settle naturally,
        // and only the still-running remainder lands `stopped` (wire
        // `killed`). `keep_alive_on_exit` (D-64) skips the convergence
        // entirely — the tasks keep running past the close and are
        // reconciled as stopped on the next load (the TS "lost" semantics).
        session.cron().shutdown();
        if session.config().task_lifecycle.keep_alive_on_exit {
            tracing::info!(
                session_id = %session_id,
                "keep_alive_on_exit: skipping background task convergence"
            );
            // SDR-013 d: a surviving task is exempt from the lifetime
            // ceiling — the user who configured exit-survival already said
            // "keep it alive", so the resident sweeper must not claim it
            // once the session (and its driver) is gone. Stamped here,
            // before the scope drops, and persisted per task so the
            // sweeper judges the exemption across restarts.
            session.tasks().mark_all_running_lifetime_exempt();
        } else {
            // The close convergence must not leave a killed-notification
            // debt: suppress every still-active task's terminal
            // notification first (TS stopAllOnExit parity) — the resumed
            // session sees stopped tasks, never notifications for them.
            session.tasks().suppress_all_active();
            session
                .tasks()
                .stop_all_with_grace(session.config().task_lifecycle.kill_grace)
                .await;
        }
        if let Err(error) = session.terminal_pool().close_all().await {
            tracing::warn!(
                session_id = %session_id,
                %error,
                "failed to close the session's terminals during close"
            );
        }

        // Persist a state snapshot so a later resume can replay the wire log
        // from this checkpoint instead of from the beginning. Best-effort:
        // the wire log remains the source of truth, so a snapshot failure
        // must not prevent the close.
        if let Err(error) = self.snapshot_session_on_close(session_id).await {
            tracing::warn!(
                session_id = %session_id,
                %error,
                "failed to write state snapshot during close; resume will replay the full wire log"
            );
        }

        // Flush the metadata snapshot as well: the mid-turn write-through for
        // `SessionMetaUpdated` is asynchronous, so a title (or rename/tag
        // change) announced just before the close may not have landed yet.
        // `state.json` is what resume reads back — losing the auto title here
        // would make the resumed session look untitled and re-trigger the
        // side title call on its next user turn. The write-through's own
        // guard applies (SDR-004): a lazily created session that never
        // conversed has no wire log and must leave nothing on disk.
        // Best-effort, like the snapshot above: a save failure must not
        // prevent the close.
        if session.wire_log_installed().await {
            let metadata = session.metadata().await;
            if let Err(error) = self.persistence.save_state(session_id, &metadata).await {
                tracing::warn!(
                    session_id = %session_id,
                    %error,
                    "failed to persist session state during close; the change stays in memory"
                );
            }
        }

        // Take this session's artifacts out of the manager: their last
        // reference is dropped right after close, closing the session's bus.
        let artifacts = self
            .artifacts
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);

        // Clear the F2 pending-steer caches: the session is closing, so its
        // replayed steer pairs must not outlive it (a later resume re-derives
        // a fresh pair set from its own fold).
        self.pending_steers
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
        self.pending_agent_steers
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(cached, _), _| cached != session_id);

        let Some(artifacts) = artifacts else {
            // The session existed in the workspace but had no artifacts —
            // an internal inconsistency; log and continue with the close.
            tracing::warn!(
                session_id = %session_id,
                "closed session had no artifacts installed"
            );
            tracing::info!(session_id = %session_id, reason = ?reason, "session closed");
            return Ok(());
        };

        // Cancel all pending approval/question requests for this session.
        let shutdown = SessionShutdown::new(&artifacts);
        shutdown.shutdown().await;

        artifacts.event_bus.publish(SessionEvent::SessionClosed {
            session_id: uuid_from_id(session_id.as_str()),
        });

        tracing::info!(session_id = %session_id, reason = ?reason, "session closed");
        Ok(())
    }

    /// List all active sessions in the workspace with summary info.
    pub async fn list_sessions(&self) -> Vec<SessionInfo> {
        let mut infos = Vec::new();
        for session_id in self.workspace.list_sessions().await {
            if let Some(session) = self.workspace.get_session(&session_id).await {
                let metadata = session.metadata().await;
                let agent_count = session.list_agents().await.len();
                infos.push(SessionInfo {
                    id: session_id.as_str().to_string(),
                    title: metadata.title,
                    created_at: metadata.created_at,
                    updated_at: metadata.updated_at,
                    agent_count,
                    tags: metadata.tags.into_iter().collect(),
                    archived: metadata.archived,
                    // Live sessions carry no retention stamps: the archive
                    // lifecycle only writes them into state.json, and a
                    // working session is never compressed (ADR-004).
                    archived_at: None,
                    compressed: false,
                    compressed_at: None,
                    pre_compressed_size: None,
                    status: SessionStatus::Running,
                    last_used_at: metadata.last_used_at,
                });
            }
        }
        infos
    }

    /// List all sessions — both active (in-memory) and persisted (on-disk) —
    /// merged into a single list sorted by `created_at` descending.
    ///
    /// This method calls [`SessionPersistence::list_persisted_sessions`] to
    /// discover sessions on disk, then checks each against the workspace's
    /// active sessions. Sessions that are live in the workspace are marked
    /// [`SessionStatus::Running`]; sessions that exist only on disk are marked
    /// [`SessionStatus::Closed`].
    ///
    /// Archived persisted sessions (the `state.json` flag set by
    /// `session.archive`) are excluded unless `include_archived` is set —
    /// the TS engine's default listing filter. Live sessions are always
    /// included (a live session is never hidden by the flag).
    ///
    /// For persisted sessions that are not active, the metadata (title,
    /// timestamps, tags) is loaded from `state.json`. For active sessions,
    /// the live metadata is used (which may have been updated since the
    /// session was created).
    ///
    /// Sessions whose `state.json` is missing or unreadable are silently
    /// skipped (matching [`SessionPersistence::list_persisted_sessions`]
    /// filtering).
    pub async fn list_all_sessions(&self, include_archived: bool) -> Vec<SessionInfo> {
        // Collect active sessions once (avoid TOCTOU race from double call).
        let active_ids_vec = self.workspace.list_sessions().await;
        let active_ids: std::collections::HashSet<String> = active_ids_vec
            .iter()
            .map(|id| id.as_str().to_string())
            .collect();

        let mut infos: Vec<SessionInfo> = Vec::new();

        // Add active sessions from the workspace (live metadata + agent count).
        for session_id in &active_ids_vec {
            if let Some(session) = self.workspace.get_session(session_id).await {
                let metadata = session.metadata().await;
                let agent_count = session.list_agents().await.len();
                infos.push(SessionInfo {
                    id: session_id.as_str().to_string(),
                    title: metadata.title,
                    created_at: metadata.created_at,
                    updated_at: metadata.updated_at,
                    agent_count,
                    tags: metadata.tags.into_iter().collect(),
                    archived: metadata.archived,
                    // Live sessions carry no retention stamps: the archive
                    // lifecycle only writes them into state.json, and a
                    // working session is never compressed (ADR-004).
                    archived_at: None,
                    compressed: false,
                    compressed_at: None,
                    pre_compressed_size: None,
                    status: SessionStatus::Running,
                    last_used_at: metadata.last_used_at,
                });
            }
        }

        // Add persisted-only sessions (not in the active set). Archived rows
        // drop out unless the caller opted in (the persistence layer filters
        // on the state.json flag).
        for persisted in self
            .persistence
            .list_persisted_sessions(include_archived)
            .await
        {
            if active_ids.contains(&persisted.session_id) {
                continue;
            }

            // Load metadata from state.json for the persisted session.
            let session_id = SessionId::new(&persisted.session_id);
            let metadata = match self.persistence.load_state(&session_id).await {
                Ok(m) => m,
                Err(_) => continue, // Skip sessions with unreadable state.
            };

            infos.push(SessionInfo {
                id: persisted.session_id,
                title: metadata.title,
                created_at: metadata.created_at,
                updated_at: metadata.updated_at,
                agent_count: 0,
                tags: metadata.tags.into_iter().collect(),
                archived: persisted.archived,
                archived_at: persisted.archived_at,
                compressed: persisted.compressed,
                compressed_at: persisted.compressed_at,
                pre_compressed_size: persisted.pre_compressed_size,
                status: SessionStatus::Closed,
                last_used_at: metadata.last_used_at,
            });
        }

        // Sort by created_at descending (newest first).
        infos.sort_by_key(|info| std::cmp::Reverse(info.created_at));
        infos
    }

    /// Get a session by ID.
    pub async fn get_session(&self, session_id: &SessionId) -> Option<Arc<SessionScope>> {
        self.workspace.get_session(session_id).await
    }

    /// Materialize a lazily created session (SDR-004 — the first user
    /// message's trigger): write `state.json`, open `wire.jsonl`, and flush
    /// the wire ops buffered since creation. From this point on the session
    /// is byte-identical to an eagerly created one (the create-time
    /// `Metadata` / config records replay in their original order).
    ///
    /// Idempotent and concurrency-safe: an already-materialized (or eagerly
    /// created, or resumed) session answers `Ok(false)` without touching the
    /// disk; a session that is not live answers `Ok(false)` as well (there is
    /// nothing to materialize — a persisted session's writer was installed
    /// at resume). A session that cannot persist fails here instead of
    /// running unlogged (the eager path's "must not start silent" invariant,
    /// moved to the first send).
    pub async fn materialize_session(&self, session_id: &SessionId) -> SessionResult<bool> {
        let Some(session) = self.get_session(session_id).await else {
            return Ok(false);
        };
        if session.wire_log_installed().await {
            return Ok(false);
        }
        // Persist the (possibly mutated since creation) metadata first — the
        // state write is atomic and idempotent, so even a racing double run
        // is harmless.
        let metadata = session.metadata().await;
        self.persistence.save_state(session_id, &metadata).await?;
        let wire_log_path = self.persistence.wire_log_path(session_id);
        let materialized = session
            .materialize_wire_log(async {
                JsonlAppendLog::create(&wire_log_path).await.map_err(|e| {
                    SessionError::WireLog(format!(
                        "failed to open {} for writing: {e}",
                        wire_log_path.display()
                    ))
                })
            })
            .await?;
        Ok(materialized)
    }

    /// Whether this process closed the session (see [`Self::close_session`]).
    /// Read paths use this to avoid silently resurrecting a closed session;
    /// a fresh manager after a restart reports `false` for every persisted
    /// session, which is what makes restart survival work.
    pub fn is_closed(&self, session_id: &SessionId) -> bool {
        self.closed_ids
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(session_id.as_str())
    }

    /// Incrementally absorb the foreign updates appended to a live
    /// session's journals (spec §5, US C4.2 / C1.2): read ONLY the bytes
    /// past each journal's known-size anchor — first the session's main
    /// journal into the main agent's in-memory state, then every
    /// registered child's own journal into the child's scope — and
    /// publish the UI-visible events — no full replay, ever.
    ///
    /// A `subagent.registered` op absorbed from the main journal lazily
    /// materializes the child agent's scope and its journal slot (batch
    /// 1.5): the child journal then drains on this and every subsequent
    /// pass, so a foreign instance's subagent activity becomes visible
    /// here within one tick.
    ///
    /// A journal whose on-disk size shrank BELOW the anchor
    /// ([`VersionCheck::Truncated`](crate::session::consistency::VersionCheck)
    /// — the session directory was rebuilt externally) is not recoverable
    /// by incremental absorption: the main journal's case logs
    /// a warning, resets the anchor, and returns an empty report; the
    /// caller is expected to fall back to a full `resume` (spec §5). A
    /// CHILD journal's truncation is local: the child's anchor resets and
    /// the pass continues with the remaining children (the stop-following
    /// semantics apply to that child only, matching the spec's
    /// per-journal independence).
    pub async fn absorb_session_foreign_updates(
        &self,
        session: &Arc<SessionScope>,
    ) -> SessionResult<crate::session::consistency::AbsorbReport> {
        let wire_path = self.persistence.wire_log_path(session.id());
        let known = session.wire_log_known_size();

        // External truncation (rebuilt session directory): not recoverable
        // incrementally — warn, zero the anchor, and report empty. The
        // upper layer re-resumes the session (spec §5).
        if let Some(disk) =
            crate::session::consistency::probe_journal_version(self.sync_io.as_ref(), &wire_path)
            && crate::session::consistency::check_version(disk, known)
                == crate::session::consistency::VersionCheck::Truncated
        {
            tracing::warn!(
                session = %session.id().as_str(),
                known_size = known,
                disk_size = disk.size,
                "session consistency: journal truncated externally; full resume required"
            );
            session.reset_wire_log_anchor().await;
            return Ok(crate::session::consistency::AbsorbReport::default());
        }

        // The main agent is the fold target for the main journal's
        // conversation-domain ops (messages, loop events, turn ends).
        let agent = session
            .get_agent(&AgentId::new(MAIN_AGENT_ID))
            .await
            .ok_or_else(|| SessionError::AgentNotFound(MAIN_AGENT_ID.to_string()))?;
        let agent_id = MAIN_AGENT_ID.to_string();
        let session_id = session.id().as_str().to_string();

        // The session's bus: the same artifacts the relay subscribed to.
        // A live session created through the manager always has artifacts;
        // without them there is nobody to notify — absorb into memory only.
        let event_bus = self
            .session_artifacts(session.id())
            .map(|artifacts| artifacts.event_bus);

        // Pass 1 — the main journal. `SubagentRegistered` and the other
        // session-domain ops are routed (NOT folded into the main agent);
        // everything else folds exactly as batch 1 did. The drained ops are
        // applied AFTER the drain released the writer lock: the child
        // materialization awaits (agent creation, journal open) and must
        // never run under it.
        let mut registered_children: Vec<mirri_proto::SubagentRegistration> = Vec::new();
        let mut main_report = session
            .drain_wire_log_foreign_ops(&mut |op| match op {
                WireOp::SubagentRegistered {
                    agent_id: child_id,
                    profile_name,
                    model,
                    thinking_effort,
                    parent_agent_id,
                    swarm_item,
                    parent_tool_call_id,
                    swarm_index,
                } => {
                    registered_children.push(mirri_proto::SubagentRegistration {
                        agent_id: child_id,
                        profile_name,
                        model,
                        thinking_effort,
                        parent_agent_id,
                        swarm_item,
                        parent_tool_call_id,
                        swarm_index,
                    });
                }
                foldable => {
                    crate::session::consistency::apply_absorbed_op(
                        &agent,
                        event_bus.as_deref(),
                        &agent_id,
                        &session_id,
                        foldable,
                    );
                }
            })
            .await
            .map_err(|e| {
                SessionError::WireLog(format!(
                    "failed to absorb foreign updates from {}: {e}",
                    wire_path.display()
                ))
            })?;
        // The drained registration ops are main-journal ops and are already
        // counted by the drain's own `ops_absorbed` (they were handed to
        // `on_op`); the folded ones were counted by the drain too — no
        // adjustment needed. `child_ops_applied` below carries the
        // child-journal work separately.

        // Apply the routed registrations outside any wire-log lock: each
        // one lazily materializes the child's scope + journal slot, and
        // synthesizes the lifecycle frames the web builds the child card
        // from (review F1 — `SubagentSpawned`/`SubagentStarted` have no
        // WireOp carrier, so the absorb path is their only producer on a
        // following instance).
        for registration in registered_children {
            crate::session::consistency::absorb_subagent_registration(
                session,
                event_bus.as_deref(),
                &registration,
            )
            .await;
        }

        // Pass 2 — every registered child's journal, into the child's own
        // scope. The registrations this pass just materialized are included
        // (list_subagent_registrations reads the live registry the fold
        // above seeded); their fresh anchors sit at the journals' current
        // lengths, so the first drain folds the child's whole history.
        for registration in session.list_subagent_registrations().await {
            let child_id = registration.agent_id.clone();
            // A child already stopped in an earlier pass (its truncation
            // signal is up): the stop persists across ticks — only a full
            // resume (which re-installs the writer) clears the signal.
            // The size probe below would read the rebuilt journal ABOVE
            // the zeroed anchor as Divergent, so the signal — not the
            // size — is what governs here. Checked BEFORE the journal
            // slot is resolved, so a stopped child never takes the
            // slot's read lock (review F5).
            if session.child_wire_log_truncated(&child_id).await {
                tracing::debug!(
                    session = %session.id().as_str(),
                    agent_id = %child_id,
                    "session consistency: child already stopped (truncated); skipping"
                );
                continue;
            }
            let child_path = self
                .persistence
                .agent_wire_log_path(session.id(), &child_id);
            let child_journal = session.child_wire_log(&child_id).await;
            let child_known = child_journal.known_size();
            // A child journal that shrank below its anchor (externally
            // rebuilt): stop following THAT child — reset its anchor so
            // the stop is observable (truncated flag), and continue with
            // the remaining children. The main journal's truncation
            // semantics (full resume) stay the pass-level decision above.
            if let Some(disk) = crate::session::consistency::probe_journal_version(
                self.sync_io.as_ref(),
                &child_path,
            ) && crate::session::consistency::check_version(disk, child_known)
                == crate::session::consistency::VersionCheck::Truncated
            {
                tracing::warn!(
                    session = %session.id().as_str(),
                    agent_id = %child_id,
                    known_size = child_known,
                    disk_size = disk.size,
                    "session consistency: child journal truncated externally; following of this child stops (full resume re-establishes it)"
                );
                child_journal.reset_anchor().await;
                continue;
            }
            let child = match session.get_agent(&AgentId::new(&child_id)).await {
                Some(child) => child,
                None => {
                    // The registration fold did not (or could not) build
                    // this child's scope — nothing to fold into; the anchor
                    // stays put so a later successful materialization
                    // resumes from the same point.
                    tracing::debug!(
                        session = %session.id().as_str(),
                        agent_id = %child_id,
                        "session consistency: no child scope; child journal not drained"
                    );
                    continue;
                }
            };
            let child_id_for_events = child_id.clone();
            let session_id_for_events = session.id().as_str().to_string();
            let child_report = child_journal
                .drain_foreign_ops(&mut |op| {
                    crate::session::consistency::apply_absorbed_op(
                        &child,
                        event_bus.as_deref(),
                        &child_id_for_events,
                        &session_id_for_events,
                        op,
                    );
                })
                .await
                .map_err(|e| {
                    SessionError::WireLog(format!(
                        "failed to absorb foreign updates from {}: {e}",
                        child_path.display()
                    ))
                })?;
            main_report.child_ops_applied += child_report.ops_absorbed;
        }

        Ok(main_report)
    }

    /// One-time legacy flat-layout migration — see
    /// [`SessionPersistence::migrate_legacy_flat_sessions`].
    ///
    /// The current workspace's `cwd` is the backfill attribution for flat
    /// sessions whose `state.json` carries no `workDir`. Best-effort by
    /// design: hosts call it once at startup (the embedded server does so
    /// on the first RPC), and every per-session failure is a warning, never
    /// a startup abort. Idempotent — a second run finds nothing flat.
    pub async fn migrate_legacy_flat_sessions(&self) -> SessionResult<usize> {
        let current_work_dir = self.workspace.cwd().to_string_lossy().into_owned();
        self.persistence
            .migrate_legacy_flat_sessions(&current_work_dir)
            .await
    }
}

mod restore;
use restore::{fire_session_end_hook, fire_session_start_hook};
#[cfg(test)]
pub(crate) use restore::{parse_goal_budget, parse_goal_status, parse_permission_mode};

#[cfg(test)]
mod tests;
