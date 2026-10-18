//! The workspace registry — the engine's in-process multi-workspace owner
//! (design `docs/specs/rust-core-design/08-multi-workspace.md` §2, batch W1).
//!
//! A [`WorkspaceRegistry`] holds the in-memory projection of the cross-engine
//! `workspaces.json` registry (one [`WorkspaceRegistration`] per row), a map
//! of **lazily built** per-workspace runtimes ([`WorkspaceRuntime`] = scope +
//! manager + per-root MCP manager — the isolation unit of D2), and the
//! **active-session table** (session-id → workspace-id) that powers the W3
//! routing seam.
//!
//! Lifecycle (adjudicated D5: register-first + lazy build + host pinning):
//!
//! - [`WorkspaceRegistry::register`] only records a row (no runtime).
//! - [`WorkspaceRegistry::register_and_build`] records **and** builds — the
//!   default workspace's path (serve's most-recent row / the embedded host's
//!   pinned root), so a non-empty registry never delays startup (N rows
//!   registered → exactly the default built).
//! - [`WorkspaceRegistry::ensure_built`] is the lazy entry: the first touch
//!   of a registered-but-unbuilt workspace constructs its runtime through the
//!   injected [`RuntimeBuilder`] (the bootstrap's per-workspace assembly)
//!   exactly once, caching the result.
//! - [`WorkspaceRegistry::manager_for`] resolves a session id through the
//!   three-level seam (active table → index fallback → not-found, design §3);
//!   W1 lands the type + active-table path, W3 consumes it across the
//!   dispatch surfaces.
//!
//! All interior state is behind `RwLock`s; the registry is `Send + Sync` and
//! cheaply clonable (`Arc` at the composition root).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use mirri_core::hook_engine::HookDef;
use mirri_core::{SessionId, SessionManager, WorkspaceScope, encode_work_dir_key};
use mirri_session_index::{
    RegistryWriter, SessionIndex, WORKSPACE_REGISTRY_FILENAME, WorkspaceRow,
};

/// One row of the registry (the in-memory projection of the cross-engine
/// `workspaces.json` entry; `id` = the cross-engine registry id = the
/// persistent bucket key `wd_<slug>_<hash12>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRegistration {
    /// The workspace key (`wd_<slug>_<hash12>`) — the identity shared by the
    /// registry file, the session persistence bucket, and the index
    /// `workspace_id` column.
    pub id: String,
    /// The workspace root directory.
    pub root: PathBuf,
    /// The display name, when one was registered (the TS engine writes the
    /// basename; a synthesized row carries `None`).
    pub name: Option<String>,
    /// RFC 3339 timestamp of the first registration.
    pub created_at: String,
    /// RFC 3339 timestamp of the last open.
    pub last_opened_at: String,
}

/// One built workspace runtime — the per-workspace isolation unit (D2): the
/// workspace's scope, its single-workspace [`SessionManager`], and the
/// per-root workspace-pool MCP manager.
pub struct WorkspaceRuntime {
    /// The registry id this runtime was built for (the bucket key).
    pub workspace_id: String,
    /// The workspace scope (sandbox root, skill catalog, config).
    pub scope: Arc<WorkspaceScope>,
    /// The session manager bound to this workspace's scope.
    pub manager: Arc<SessionManager>,
    /// The workspace-pool MCP connection manager (project layers + plugin
    /// servers) discovered for this workspace's root. `None` when no MCP
    /// config exists. The default runtime's manager feeds `EngineParts`
    /// (W1); W7 fans the per-workspace managers out.
    pub mcp_manager: Option<Arc<mirri_mcp::McpConnectionManager>>,
    /// The layered MCP config view routing this workspace's pools (carried
    /// so a later batch can connect it without re-discovery). W1 never
    /// builds non-default runtimes, so only the default's config is ever
    /// consumed (through `EngineParts`).
    pub pending_mcp_configs: Option<mirri_mcp::config::LayeredMcpConfig>,
    /// The W7 trust-gate verdict this runtime was built under (design 08
    /// §5 D6): `true` when the workspace's project layers loaded (MCP
    /// config + AGENTS.md project section), `false` for an untrusted
    /// workspace (user-layer-only). The reconcile kernel and the lazy
    /// workspace-pool connect read it — an untrusted runtime's project
    /// files are never re-read, let alone connected.
    ///
    /// **Live since W8**: the verdict is an in-place-flippable atomic the
    /// `workspace.setTrust` face updates after the home-level store write
    /// lands (a grant/revoke is same-process hot-reloaded, the TS v2
    /// `onDidChange` parity — a runtime REBUILD is never needed, so the
    /// runtime, its manager, its sessions and their agents all stay
    /// alive through a flip). The build-time seed still comes from the
    /// store (the default workspace is trusted by construction); a flip
    /// that happens in ANOTHER process is picked up by this process only
    /// through its own next store read (build / kernel gate).
    pub trusted: std::sync::atomic::AtomicBool,
    /// Guard against the per-workspace lazy MCP connect (W7): the embedded
    /// server's first engine call spawns each built runtime's
    /// project-layer connect exactly once — the
    /// `EmbeddedServer::maybe_spawn_mcp_connect` pattern, per runtime.
    pub mcp_connect_spawned: std::sync::atomic::AtomicBool,
    /// The workspace's hook base — the scope config's hooks BEFORE the
    /// enabled plugins' hooks were merged (item E). The plugin-reload
    /// refresh (item C4) re-derives the merged set from this base plus the
    /// current `enabled_hooks()`, so plugins can add/remove hooks without
    /// a runtime rebuild; sessions created after the refresh inherit the
    /// refreshed workspace hook engine.
    pub base_hooks: Vec<HookDef>,
}

impl WorkspaceRuntime {
    /// The LIVE trust verdict (W8): the build-time store seed, flipped in
    /// place by the `workspace.setTrust` face. Read this, never the raw
    /// atomic, from outside the registry.
    pub fn trusted(&self) -> bool {
        self.trusted.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Flip the live trust verdict (W8): the `workspace.setTrust` face's
    /// runtime refresh — the atomic release makes the flip visible to the
    /// reconcile kernel and the shadow view on their next read.
    pub fn set_trusted(&self, trusted: bool) {
        self.trusted
            .store(trusted, std::sync::atomic::Ordering::Release);
    }

    /// Refresh the workspace's plugin-derived hook set in place (item C4 —
    /// the TS reload re-registration): clear the live hook engine and
    /// re-register the base (config/user) hooks plus the current plugin
    /// hooks, in the same `[...configured, ...pluginHooks]` order the
    /// runtime build used. Sessions created after the refresh inherit the
    /// new set; sessions already created keep their assembly-time engine.
    pub fn refresh_plugin_hooks(&self, plugin_hooks: Vec<HookDef>) {
        let mut merged = self.base_hooks.clone();
        merged.extend(plugin_hooks);
        let engine = self.scope.hook_engine();
        engine.clear();
        engine.register_all(merged);
    }
}

/// The result of [`WorkspaceRegistry::manager_for`] — the routing seam's
/// resolution (design §3). The `workspace_id` rides on both hit variants so
/// callers can re-register derived ids (a forked session, an active-table
/// backfill) against the owning workspace without a second lookup.
pub enum ManagerResolution {
    /// The session is active in this process and its workspace is built.
    Active {
        workspace_id: String,
        manager: Arc<SessionManager>,
    },
    /// The session is not active but its owning workspace was resolved via
    /// the index fallback or the bucket probe (and built on demand).
    Restored {
        workspace_id: String,
        manager: Arc<SessionManager>,
    },
    /// The session belongs to no known workspace — never a silent fallback
    /// to the default.
    NotFound,
}

impl std::fmt::Debug for ManagerResolution {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ManagerResolution::Active {
                workspace_id,
                manager,
            } => write!(
                f,
                "Active({workspace_id} / {})",
                manager.workspace().id().as_str()
            ),
            ManagerResolution::Restored {
                workspace_id,
                manager,
            } => write!(
                f,
                "Restored({workspace_id} / {})",
                manager.workspace().id().as_str()
            ),
            ManagerResolution::NotFound => write!(f, "NotFound"),
        }
    }
}

/// The index-fallback injection seam ([`ManagerResolution::Restored`]): the
/// composition root implements it over the sqlite `SessionIndex`; tests use
/// an in-memory map.
pub trait SessionWorkspaceResolver: Send + Sync {
    /// The workspace id owning a persisted session, when the index knows it.
    fn workspace_of_session(&self, session_id: &str) -> Option<String>;
}

/// The runtime builder the registry invokes on a workspace's first touch
/// (the bootstrap's per-workspace assembly; tests inject a recording fake to
/// assert laziness).
pub type RuntimeBuilder =
    Arc<dyn Fn(&WorkspaceRegistration) -> anyhow::Result<WorkspaceRuntime> + Send + Sync>;

/// The in-process workspace registry: rows + built runtimes + the
/// active-session table + the default-workspace pointer.
pub struct WorkspaceRegistry {
    /// Every registered row (the registry file's in-memory projection).
    rows: RwLock<HashMap<String, WorkspaceRegistration>>,
    /// Runtimes built so far, keyed by workspace id. Non-default workspaces
    /// enter here only when first touched (D5 lazy build).
    built: RwLock<HashMap<String, Arc<WorkspaceRuntime>>>,
    /// Serializes lazy builds (W3): two concurrent first touches of the same
    /// workspace must not both construct its runtime.
    build_lock: Mutex<()>,
    /// The active-session table: session-id → workspace-id (the routing
    /// seam's first level — sessions are noted on create/resume and
    /// forgotten on close/archive).
    active: RwLock<HashMap<String, String>>,
    /// The default workspace id (`serve`: most recent row; embedded host:
    /// the pinned root). `None` until one is registered/built.
    default_id: RwLock<Option<String>>,
    /// The index-fallback resolver (level 2 of `manager_for`). The
    /// composition-root index connection (below) takes precedence when
    /// installed; this injected resolver is the test seam.
    resolver: Arc<dyn SessionWorkspaceResolver>,
    /// The lazy runtime factory invoked by `ensure_built`.
    builder: RuntimeBuilder,
    /// The shared session-index connection (W3 unification): installed by
    /// the composition root when a home exists, lazily opened on the first
    /// fallback hit, and shared with the embedded server's own index reads
    /// so the two never open the file twice. `None` inside the cell when no
    /// home was installed or the open failed.
    session_index: OnceLock<Option<Arc<SessionIndex>>>,
    /// The home the shared index connection roots at (see
    /// [`Self::install_session_index`]).
    session_index_home: RwLock<Option<PathBuf>>,
    /// The D8 touch debounce's pending set (W6, design §2.3 写放大控制):
    /// workspace ids whose `last_opened_at` touch is updated in memory but
    /// not yet flushed to the registry file. Session creation only touches
    /// here — it never writes the registry directly.
    pending_touches: Mutex<std::collections::HashSet<String>>,
    /// Whether a debounce timer task is armed (the first touch after an
    /// idle window schedules exactly one flush; further touches within the
    /// window only update memory).
    touch_flush_armed: std::sync::atomic::AtomicBool,
    /// How many debounce flushes have been performed (the deterministic
    /// test face of the write-amplification red line: N touches → one
    /// flush).
    touch_flush_count: std::sync::atomic::AtomicUsize,
}

impl WorkspaceRegistry {
    /// A registry with the given index-fallback resolver and runtime
    /// builder. No rows, no default — the composition root registers and
    /// builds from there.
    pub fn new(resolver: Arc<dyn SessionWorkspaceResolver>, builder: RuntimeBuilder) -> Self {
        Self {
            rows: RwLock::new(HashMap::new()),
            built: RwLock::new(HashMap::new()),
            build_lock: Mutex::new(()),
            active: RwLock::new(HashMap::new()),
            default_id: RwLock::new(None),
            resolver,
            builder,
            session_index: OnceLock::new(),
            session_index_home: RwLock::new(None),
            pending_touches: Mutex::new(std::collections::HashSet::new()),
            touch_flush_armed: std::sync::atomic::AtomicBool::new(false),
            touch_flush_count: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// A single-workspace registry wrapping one pre-built manager — the
    /// EmbeddedServer's direct constructors (a bare manager with no
    /// composition root): one entry keyed by the manager's persistence
    /// bucket ([`encode_work_dir_key`] of its workspace cwd), the default
    /// set, and a no-op resolver (nothing to fall back to). The builder is
    /// poisoned: no additional workspace can be built through this
    /// constructor.
    pub fn single(manager: Arc<SessionManager>) -> Self {
        let workspace_id = encode_work_dir_key(manager.workspace().cwd());
        let now = chrono::Utc::now().to_rfc3339();
        let registration = WorkspaceRegistration {
            id: workspace_id.clone(),
            root: manager.workspace().cwd().to_path_buf(),
            name: None,
            created_at: now.clone(),
            last_opened_at: now,
        };
        let runtime = WorkspaceRuntime {
            workspace_id: workspace_id.clone(),
            scope: Arc::clone(manager.workspace()),
            manager: Arc::clone(&manager),
            mcp_manager: None,
            pending_mcp_configs: None,
            trusted: std::sync::atomic::AtomicBool::new(true),
            mcp_connect_spawned: std::sync::atomic::AtomicBool::new(false),
            base_hooks: Vec::new(),
        };
        let registry = Self::new(
            Arc::new(NoopResolver),
            Arc::new(|_| {
                Err(anyhow::anyhow!(
                    "single-workspace registries cannot build further runtimes"
                ))
            }),
        );
        registry.register(registration);
        registry
            .built
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(workspace_id.clone(), Arc::new(runtime));
        *registry
            .default_id
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Some(workspace_id);
        registry
    }

    /// Register a row without building its runtime (D5 register-first).
    pub fn register(&self, registration: WorkspaceRegistration) {
        self.rows
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(registration.id.clone(), registration);
    }

    /// Register a row **and** build its runtime immediately — the default
    /// workspace's path. The registration replaces any prior row with the
    /// same id (touch semantics at the row level).
    pub fn register_and_build(
        &self,
        registration: WorkspaceRegistration,
    ) -> anyhow::Result<Arc<WorkspaceRuntime>> {
        let id = registration.id.clone();
        self.register(registration);
        match self.ensure_built(&id) {
            Some(Ok(runtime)) => Ok(runtime),
            Some(Err(error)) => Err(error),
            None => Err(anyhow::anyhow!(
                "workspace {id} vanished from the registry during build"
            )),
        }
    }

    /// The lazy build entry: a registered-but-unbuilt workspace's runtime is
    /// constructed on first touch and cached. `None` for an unknown id;
    /// `Some(Err(..))` when the builder failed (not cached — a later touch
    /// retries). Concurrent first touches serialize on the registry's build
    /// lock (double-checked: the cache is re-read under the lock), so the
    /// builder runs exactly once per workspace even under contention.
    pub fn ensure_built(
        &self,
        workspace_id: &str,
    ) -> Option<anyhow::Result<Arc<WorkspaceRuntime>>> {
        if let Some(runtime) = self
            .built
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(workspace_id)
        {
            return Some(Ok(Arc::clone(runtime)));
        }
        let _guard = self.build_lock.lock().unwrap_or_else(|e| e.into_inner());
        // Re-read under the lock: a racing builder may have won the race.
        if let Some(runtime) = self
            .built
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(workspace_id)
        {
            return Some(Ok(Arc::clone(runtime)));
        }
        let registration = self
            .rows
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(workspace_id)
            .cloned()?;
        let runtime = match (self.builder)(&registration) {
            Ok(runtime) => runtime,
            Err(error) => return Some(Err(error)),
        };
        let runtime = Arc::new(runtime);
        self.built
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(workspace_id.to_string(), Arc::clone(&runtime));
        Some(Ok(runtime))
    }

    /// The default workspace's built runtime, when one is set and built.
    pub fn default_runtime(&self) -> Option<Arc<WorkspaceRuntime>> {
        let id = self
            .default_id
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()?;
        self.built
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .cloned()
    }

    /// Set the default workspace. Returns `false` (and changes nothing) when
    /// the id is not registered — an unknown default is never synthesized.
    pub fn set_default(&self, workspace_id: &str) -> bool {
        let known = self
            .rows
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains_key(workspace_id);
        if known {
            *self.default_id.write().unwrap_or_else(|e| e.into_inner()) =
                Some(workspace_id.to_string());
        }
        known
    }

    /// The registration row for a workspace id (the W6 remove/touch surface
    /// reads through this).
    pub fn registration(&self, workspace_id: &str) -> Option<WorkspaceRegistration> {
        self.rows
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(workspace_id)
            .cloned()
    }

    /// Every registration row, sorted by id for deterministic iteration
    /// (the real `last_opened_at` ordering lands with the W2 list
    /// semantics).
    pub fn registrations(&self) -> Vec<WorkspaceRegistration> {
        let mut rows: Vec<WorkspaceRegistration> = self
            .rows
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect();
        rows.sort_by(|a, b| a.id.cmp(&b.id));
        rows
    }

    /// Unregister a workspace: drop its row and its built runtime (W6's
    /// "remove" mode). The active table is left untouched — live sessions
    /// route through their own entries until the delete sweep closes them.
    pub fn remove(&self, workspace_id: &str) {
        self.rows
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(workspace_id);
        self.built
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(workspace_id);
        let mut default = self.default_id.write().unwrap_or_else(|e| e.into_inner());
        if default.as_deref() == Some(workspace_id) {
            *default = None;
        }
    }

    /// The D8 touch debounce (design §2.3 写放大控制, W6): advance the
    /// workspace's `last_opened_at` **in memory only** and mark the id
    /// pending a registry flush. Session creation calls this — it must never
    /// write the registry file directly; the flush is deferred to the
    /// debounce window (or the next immediate write path, which flushes
    /// pending touches first). Returns whether the registration exists (a
    /// touch of an unknown id is a no-op, never an error).
    pub fn touch_opened(&self, workspace_id: &str) -> bool {
        let mut rows = self.rows.write().unwrap_or_else(|e| e.into_inner());
        let Some(registration) = rows.get_mut(workspace_id) else {
            return false;
        };
        registration.last_opened_at = chrono::Utc::now().to_rfc3339();
        drop(rows);
        self.pending_touches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(workspace_id.to_string());
        true
    }

    /// Whether any registry touch is pending a flush (the debounce test
    /// face).
    pub fn has_pending_registry_touches(&self) -> bool {
        !self
            .pending_touches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    }

    /// Flush the pending `last_opened_at` touches (the W6 debounce drain):
    /// every touched registration is written through the index read model
    /// and ONE D8 merged write of all current rows lands in
    /// `workspaces.json`. Register/rename/delete call this before their own
    /// immediate write; the debounce timer calls it when the window
    /// elapses. A no-op when nothing is pending. Returns whether a flush
    /// was performed.
    pub fn flush_pending_registry_touches(&self) -> bool {
        let pending: Vec<String> = {
            let mut pending = self
                .pending_touches
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            pending.drain().collect()
        };
        // The armed flag releases with the drain: a touch arriving after the
        // drain arms a fresh timer even if the old task has not exited yet.
        self.touch_flush_armed
            .store(false, std::sync::atomic::Ordering::Release);
        if pending.is_empty() {
            return false;
        }
        let Some(home) = self
            .session_index_home
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        else {
            // No home (every direct-constructor engine): the touch was
            // in-memory only — nothing to flush, never a failure.
            return false;
        };
        if let Some(index) = self.shared_session_index() {
            for id in &pending {
                if let Some(registration) = self.registration(id)
                    && let Err(error) = index.upsert_workspace(&registration_row(&registration))
                {
                    tracing::warn!(
                        workspace = %id,
                        %error,
                        "flushing the touched workspace's index row failed"
                    );
                }
            }
        }
        let local_rows: Vec<WorkspaceRow> =
            self.registrations().iter().map(registration_row).collect();
        if let Err(error) =
            RegistryWriter::new(home.join(WORKSPACE_REGISTRY_FILENAME)).write_merged(&local_rows)
        {
            tracing::warn!(
                %error,
                "flushing debounced workspace touches to workspaces.json failed"
            );
            return false;
        }
        self.touch_flush_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        true
    }

    /// How many debounce flushes have written the registry (the
    /// write-amplification red line's deterministic counter).
    pub fn touch_flush_count(&self) -> usize {
        self.touch_flush_count
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Try to arm the debounce timer: `false` when one is already armed (a
    /// second touch inside the window must not schedule a second flush).
    /// The embedded server's touch helper calls this; the timer task itself
    /// calls [`Self::flush_pending_registry_touches`] when the window ends.
    pub fn try_arm_touch_flush(&self) -> bool {
        !self
            .touch_flush_armed
            .swap(true, std::sync::atomic::Ordering::AcqRel)
    }

    /// How long after the first pending touch the debounce timer flushes
    /// (design §2.3 "批量/定时" — the batch window).
    pub const TOUCH_DEBOUNCE_WINDOW: std::time::Duration = std::time::Duration::from_secs(3);

    /// The pending registry touches, drained as owned ids (the immediate
    /// write paths fold them into their own write — register/rename/delete
    /// flush first so no touch is lost).
    pub fn take_pending_registry_touches(&self) -> Vec<String> {
        self.pending_touches
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
            .collect()
    }

    /// Note a session as active in its workspace (the routing seam's first
    /// level; called on create/resume/restore).
    pub fn note_active(&self, session_id: &str, workspace_id: &str) {
        self.active
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session_id.to_string(), workspace_id.to_string());
    }

    /// Forget a session's active entry (called on close/archive).
    pub fn forget_active(&self, session_id: &str) {
        self.active
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
    }

    /// The routing seam (design §3): active session table → index fallback
    /// (the owning workspace is lazily built on a hit) → built-runtime
    /// bucket probe → not-found. Never falls back to the default silently —
    /// an unknown session is [`ManagerResolution::NotFound`]. The resolution
    /// is synchronous (the builder is synchronous).
    ///
    /// The bucket probe (level 3) is *honest attribution*, not the forbidden
    /// silent default: a session whose `state.json` physically lives in a
    /// built runtime's own bucket belongs to that workspace, so engines
    /// without an index (no injected home — every direct-constructor
    /// server) and cold index misses still resolve their persisted
    /// sessions. The default runtime is probed first, the rest by id (a
    /// deterministic order).
    pub fn manager_for(&self, session_id: &str) -> ManagerResolution {
        // Level 1: the active-session table — this process's authoritative
        // answer. The owning workspace must be built to serve the manager;
        // an active-but-unbuildable entry resolves to NotFound rather than
        // mis-routing to the default.
        let from_active = self
            .active
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(session_id)
            .cloned();
        if let Some(workspace_id) = from_active {
            if let Some(Ok(runtime)) = self.ensure_built(&workspace_id) {
                return ManagerResolution::Active {
                    workspace_id,
                    manager: Arc::clone(&runtime.manager),
                };
            }
            return ManagerResolution::NotFound;
        }
        // Level 2: the index fallback — a persisted-but-inactive session's
        // owning workspace (resume / read-only operations on cold sessions).
        // The composition-root-shared connection takes precedence; the
        // injected resolver is the test seam.
        let from_index = self
            .shared_session_index()
            .and_then(|index| index.get_session(session_id).map(|row| row.workspace_id))
            .or_else(|| self.resolver.workspace_of_session(session_id));
        if let Some(workspace_id) = from_index {
            if let Some(Ok(runtime)) = self.ensure_built(&workspace_id) {
                return ManagerResolution::Restored {
                    workspace_id,
                    manager: Arc::clone(&runtime.manager),
                };
            }
            return ManagerResolution::NotFound;
        }
        // Level 3: the built-runtime bucket probe (see the method doc).
        if let Some((workspace_id, runtime)) = self.probe_built_buckets(session_id) {
            return ManagerResolution::Restored {
                workspace_id,
                manager: Arc::clone(&runtime.manager),
            };
        }
        // Unresolvable — never the default.
        ManagerResolution::NotFound
    }

    /// Probe the built runtimes' own persistence buckets for a session's
    /// `state.json` — default workspace first, then the rest by id. See
    /// [`Self::manager_for`] for why this is attribution, not fallback.
    fn probe_built_buckets(&self, session_id: &str) -> Option<(String, Arc<WorkspaceRuntime>)> {
        let sid = SessionId::new(session_id);
        let built = self.built.read().unwrap_or_else(|e| e.into_inner());
        let default_id = self
            .default_id
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let owns = |runtime: &Arc<WorkspaceRuntime>| {
            runtime.manager.persistence().state_path(&sid).is_file()
        };
        if let Some((id, runtime)) = default_id.as_deref().and_then(|id| {
            built
                .get(id)
                .map(|runtime| (id.to_string(), Arc::clone(runtime)))
        }) && owns(&runtime)
        {
            return Some((id, runtime));
        }
        let mut others: Vec<(&String, &Arc<WorkspaceRuntime>)> = built
            .iter()
            .filter(|(id, _)| Some(id.as_str()) != default_id.as_deref())
            .collect();
        others.sort_by(|a, b| a.0.cmp(b.0));
        for (id, runtime) in others {
            if owns(runtime) {
                return Some((id.clone(), Arc::clone(runtime)));
            }
        }
        None
    }

    /// Install the shared session-index connection (the composition root's
    /// call when a home exists): one lazy-opened connection at
    /// `<home>/session-index.db` shared by the index fallback and the
    /// embedded server's own index reads. Opening stays lazy (the first
    /// fallback hit / read), so assembly never creates the file.
    pub fn install_session_index(&self, home: PathBuf) {
        *self
            .session_index_home
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Some(home);
    }

    /// The installed session-index home, when one was installed — the
    /// `<home>` the shared `session-index.db` (and the D8
    /// `workspaces.json.lock` the reconcile takes) lives under. `None` for
    /// a workspace-local engine with no injected home.
    pub fn session_index_home(&self) -> Option<PathBuf> {
        self.session_index_home
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The shared session-index connection, lazily opened. `None` when no
    /// home was installed or the open failed (the fallback degrades to off,
    /// never a failure).
    pub fn shared_session_index(&self) -> Option<Arc<SessionIndex>> {
        let home = self
            .session_index_home
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()?;
        self.session_index
            .get_or_init(
                || match SessionIndex::open(&home.join("session-index.db")) {
                    Ok(index) => Some(Arc::new(index)),
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            home = %home.display(),
                            "opening session-index.db failed; the index fallback is disabled"
                        );
                        None
                    }
                },
            )
            .clone()
    }

    /// The workspace's built runtime, when one is cached — a read-only peek
    /// that NEVER builds (unlike [`Self::ensure_built`]). The W6 delete
    /// sweep reads through this: unregistering a never-touched registration
    /// must not build its runtime just to find it holds no live sessions.
    pub fn built_runtime(&self, workspace_id: &str) -> Option<Arc<WorkspaceRuntime>> {
        self.built
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(workspace_id)
            .cloned()
    }

    /// Every built runtime, default workspace first then by id (the same
    /// deterministic order the bucket probe uses) — the W4 live-session
    /// union's iteration face. Registered-but-unbuilt runtimes are absent
    /// (they hold no live sessions until first touched).
    pub fn built_runtimes(&self) -> Vec<Arc<WorkspaceRuntime>> {
        let built = self.built.read().unwrap_or_else(|e| e.into_inner());
        let default_id = self
            .default_id
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let mut runtimes: Vec<Arc<WorkspaceRuntime>> = Vec::with_capacity(built.len());
        if let Some(runtime) = default_id.as_deref().and_then(|id| built.get(id)) {
            runtimes.push(Arc::clone(runtime));
        }
        let mut others: Vec<(&String, &Arc<WorkspaceRuntime>)> = built
            .iter()
            .filter(|(id, _)| Some(id.as_str()) != default_id.as_deref())
            .collect();
        others.sort_by(|a, b| a.0.cmp(b.0));
        runtimes.extend(others.into_iter().map(|(_, runtime)| Arc::clone(runtime)));
        runtimes
    }

    /// The default workspace's registration id, when one is set (serve: the
    /// most recent row; an embedded host: the pinned root). The W5 listing
    /// visibility pin reads this.
    pub fn default_workspace_id(&self) -> Option<String> {
        self.default_id
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Group every registration by the physical directory it addresses
    /// (ADR D3's alias set): rows whose roots share a
    /// [`workspace_root_key`] fold into one group. Groups are sorted by
    /// their first id, ids within a group sorted — deterministic for tests.
    /// The index-derived sibling ids (legacy split buckets) join through
    /// [`Self::alias_group_of`] instead.
    pub fn alias_groups(&self) -> Vec<Vec<String>> {
        let mut keyed: Vec<(String, String)> = self
            .registrations()
            .into_iter()
            .map(|row| (workspace_root_key(&row.root), row.id))
            .collect();
        keyed.sort();
        let mut groups: Vec<Vec<String>> = Vec::new();
        for (key, id) in keyed {
            match groups.last_mut() {
                Some(group)
                    if !group.is_empty()
                        && self
                            .registration(&group[0])
                            .is_some_and(|row| workspace_root_key(&row.root) == key) =>
                {
                    group.push(id);
                }
                _ => groups.push(vec![id]),
            }
        }
        groups
    }

    /// Every workspace id addressing the same physical directory as the
    /// requested registration — the D3 alias set the listing and counting
    /// filters expand through (the TS `IWorkspaceAliases.resolveAliasIds`
    /// port): ① registered rows whose root folds to the same key; ② the
    /// session index's unregistered bucket ids whose rows' `cwd` spells the
    /// same root (a legacy split bucket — the rust analogue of the TS
    /// `session_index.jsonl` arm). An unknown or index-less id answers
    /// `[id]` alone.
    pub fn alias_group_of(&self, workspace_id: &str) -> Vec<String> {
        let Some(registration) = self.registration(workspace_id) else {
            return vec![workspace_id.to_string()];
        };
        let root_key = workspace_root_key(&registration.root);
        let mut group: Vec<String> = self
            .registrations()
            .iter()
            .filter(|row| workspace_root_key(&row.root) == root_key)
            .map(|row| row.id.clone())
            .collect();
        if let Some(index) = self.shared_session_index()
            && let Ok(pairs) = index.workspace_id_cwds()
        {
            for (bucket_id, cwd) in pairs {
                let Some(cwd) = cwd else { continue };
                if workspace_root_key(Path::new(&cwd)) == root_key && !group.contains(&bucket_id) {
                    group.push(bucket_id);
                }
            }
        }
        group.sort();
        group.dedup();
        group
    }

    /// The number of runtimes built so far — the startup-delay assertion
    /// (D5): N registered rows with a built default must report 1.
    pub fn built_count(&self) -> usize {
        self.built.read().unwrap_or_else(|e| e.into_inner()).len()
    }
}

/// Convert a registration to the index/registry row shape — the touch
/// flush's write-through conversion (the same tolerance as the read side: an
/// unparseable timestamp falls back to now).
fn registration_row(registration: &WorkspaceRegistration) -> WorkspaceRow {
    let parse = |value: &str| {
        chrono::DateTime::parse_from_rfc3339(value)
            .map(|parsed| parsed.with_timezone(&chrono::Utc))
            .unwrap_or_else(|_| chrono::Utc::now())
    };
    WorkspaceRow {
        id: registration.id.clone(),
        root: Some(registration.root.to_string_lossy().into_owned()),
        name: registration.name.clone(),
        created_at: parse(&registration.created_at),
        last_opened_at: parse(&registration.last_opened_at),
        sort_order: 0,
    }
}

/// The comparison-only fold key answering "is this the same directory?"
/// without changing the id already minted for it — the TS
/// `workspaceRootKey` port (`workdir-slug.ts`): separators normalized to `/`,
/// trailing separators stripped, and the whole string lowercased **only when
/// win-shaped** (a string-shape check — `[A-Za-z]:`, `//` — not a platform
/// check, so POSIX case variants do **not** fold, exactly the TS behaviour).
/// `~` is never expanded (the TS side expands at the input boundary, not in
/// the fold) and symlinks are never resolved — the rust chain first
/// lexically resolves the path (`.`/`..`/duplicate-segment cleaning, the
/// registration chain's own normalization).
pub(crate) fn workspace_root_key(root: &Path) -> String {
    use mirri_core::lexically_resolve;
    let slashed = lexically_resolve(root).to_string_lossy().replace('\\', "/");
    let trimmed = slashed.trim_end_matches('/');
    let win_shaped = {
        let bytes = trimmed.as_bytes();
        (trimmed.len() >= 2 && bytes[0].is_ascii_alphabetic() && matches!(bytes.get(1), Some(b':')))
            || trimmed.starts_with("//")
    };
    if win_shaped {
        trimmed.to_lowercase()
    } else {
        trimmed.to_string()
    }
}

/// The no-op resolver (embedded hosts with nothing to fall back to — every
/// resolution goes active-table → not-found).
pub(crate) struct NoopResolver;

impl SessionWorkspaceResolver for NoopResolver {
    fn workspace_of_session(&self, _session_id: &str) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mirri_core::{WorkspaceId, WorkspaceScopeConfig};

    /// One registration row with the plan's fixture shape.
    fn reg(id: &str) -> WorkspaceRegistration {
        WorkspaceRegistration {
            id: id.into(),
            root: PathBuf::from(format!("/tmp/ws-{id}")),
            name: None,
            created_at: "2026-09-11T00:00:00.000Z".into(),
            last_opened_at: "2026-09-11T00:00:00.000Z".into(),
        }
    }

    /// The test-double resolver: a plain map answering `workspace_of_session`
    /// from memory (the index fallback).
    #[derive(Default)]
    struct MapResolver {
        map: std::sync::Mutex<HashMap<String, String>>,
    }

    impl MapResolver {
        fn set(&self, session_id: &str, workspace_id: &str) {
            self.map
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(session_id.to_string(), workspace_id.to_string());
        }
    }

    impl SessionWorkspaceResolver for MapResolver {
        fn workspace_of_session(&self, session_id: &str) -> Option<String> {
            self.map
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(session_id)
                .cloned()
        }
    }

    /// The minimal fake runtime — same shape as
    /// `embedded_server/tests.rs::test_server`: a tempdir-rooted
    /// [`WorkspaceScope`] + [`SessionManager`], no persistence.
    fn fake_runtime(id: &str) -> WorkspaceRuntime {
        let tmp = tempfile::tempdir().expect("tempdir must be creatable");
        rooted_runtime(id, tmp.path())
    }

    /// A fake runtime rooted at a **kept** directory — the bucket-probe
    /// fixture (its persistence bucket outlives the builder call, so a
    /// `state.json` written into it stays visible to the probe).
    fn rooted_runtime(id: &str, root: &std::path::Path) -> WorkspaceRuntime {
        let workspace = Arc::new(WorkspaceScope::new(
            WorkspaceId::new(id),
            root.to_path_buf(),
            WorkspaceScopeConfig::default(),
        ));
        let manager = Arc::new(SessionManager::new(workspace));
        WorkspaceRuntime {
            workspace_id: id.to_string(),
            scope: Arc::clone(manager.workspace()),
            manager,
            mcp_manager: None,
            pending_mcp_configs: None,
            trusted: std::sync::atomic::AtomicBool::new(true),
            mcp_connect_spawned: std::sync::atomic::AtomicBool::new(false),
            base_hooks: Vec::new(),
        }
    }

    /// A recording builder asserting which ids were lazily constructed.
    fn recording_builder(built: Arc<std::sync::Mutex<Vec<String>>>) -> RuntimeBuilder {
        Arc::new(move |reg| {
            built
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(reg.id.clone());
            Ok(fake_runtime(&reg.id))
        })
    }

    // Given a registry with N registered rows, when restored via the
    // composition root, then only the default workspace runtime is built
    // (D5 lazy build).
    #[test]
    fn should_build_only_default_runtime_when_n_rows_registered() {
        let built_log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let registry = WorkspaceRegistry::new(
            Arc::new(MapResolver::default()),
            recording_builder(Arc::clone(&built_log)),
        );
        registry.register_and_build(reg("wd_default")).unwrap();
        registry.register(reg("wd_lazy_a"));
        registry.register(reg("wd_lazy_b"));
        assert_eq!(registry.built_count(), 1);
        assert_eq!(
            *built_log.lock().unwrap_or_else(|e| e.into_inner()),
            vec!["wd_default"]
        );
    }

    // Given a lazily registered workspace, when ensure_built is first
    // called, then the runtime is constructed exactly once and cached.
    #[test]
    fn should_build_lazy_runtime_once_when_first_touched() {
        let built_log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let registry = WorkspaceRegistry::new(
            Arc::new(MapResolver::default()),
            recording_builder(Arc::clone(&built_log)),
        );
        registry.register(reg("wd_lazy_a"));
        assert_eq!(registry.built_count(), 0);

        let first = registry
            .ensure_built("wd_lazy_a")
            .expect("row is registered")
            .expect("build succeeds");
        let second = registry
            .ensure_built("wd_lazy_a")
            .expect("row is registered")
            .expect("build succeeds");
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(registry.built_count(), 1);
        assert_eq!(
            *built_log.lock().unwrap_or_else(|e| e.into_inner()),
            vec!["wd_lazy_a"]
        );
    }

    // Given an active session in workspace A, when manager_for resolves it,
    // then the session lands on A's manager without falling back to default.
    #[test]
    fn should_resolve_active_session_to_own_workspace_not_default() {
        let built_log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let registry = WorkspaceRegistry::new(
            Arc::new(MapResolver::default()),
            recording_builder(Arc::clone(&built_log)),
        );
        registry.register_and_build(reg("wd_default")).unwrap();
        registry.register(reg("wd_workspace_a"));
        registry.note_active("sess-in-a", "wd_workspace_a");

        match registry.manager_for("sess-in-a") {
            ManagerResolution::Active {
                workspace_id,
                manager,
            } => {
                // The session's own workspace manager — not the default.
                assert_eq!(manager.workspace().id().as_str(), "wd_workspace_a");
                assert_eq!(workspace_id, "wd_workspace_a");
                assert_eq!(
                    *built_log.lock().unwrap_or_else(|e| e.into_inner()),
                    vec!["wd_default", "wd_workspace_a"]
                );
            }
            other => panic!("must resolve to Active, got {other:?}"),
        }
    }

    // Given an unknown session id, when manager_for resolves it, then the
    // result is NotFound (never a silent default fallback).
    #[test]
    fn should_return_not_found_when_session_unknown() {
        let registry = WorkspaceRegistry::new(
            Arc::new(MapResolver::default()),
            recording_builder(Arc::new(std::sync::Mutex::new(Vec::new()))),
        );
        registry.register_and_build(reg("wd_default")).unwrap();

        assert!(matches!(
            registry.manager_for("sess-unknown"),
            ManagerResolution::NotFound
        ));
    }

    // Given a persisted-but-inactive session (index hit), when manager_for
    // resolves it, then the owning workspace is lazily built and its
    // manager returned.
    #[test]
    fn should_restore_persisted_session_via_index_fallback() {
        let resolver = MapResolver::default();
        resolver.set("sess-persisted", "wd_persisted_ws");
        let built_log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let registry = WorkspaceRegistry::new(
            Arc::new(resolver),
            recording_builder(Arc::clone(&built_log)),
        );
        registry.register_and_build(reg("wd_default")).unwrap();
        registry.register(reg("wd_persisted_ws"));

        match registry.manager_for("sess-persisted") {
            ManagerResolution::Restored {
                workspace_id,
                manager,
            } => {
                assert_eq!(manager.workspace().id().as_str(), "wd_persisted_ws");
                assert_eq!(workspace_id, "wd_persisted_ws");
                assert_eq!(
                    *built_log.lock().unwrap_or_else(|e| e.into_inner()),
                    vec!["wd_default", "wd_persisted_ws"]
                );
            }
            other => panic!("must resolve to Restored, got {other:?}"),
        }
    }

    // Given a built workspace, when it is removed, then its row and runtime
    // are gone and a removed default leaves no default.
    #[test]
    fn should_remove_the_row_and_runtime_when_removing_a_workspace() {
        let registry = WorkspaceRegistry::new(
            Arc::new(MapResolver::default()),
            recording_builder(Arc::new(std::sync::Mutex::new(Vec::new()))),
        );
        registry.register_and_build(reg("wd_default")).unwrap();

        registry.remove("wd_default");

        assert_eq!(registry.built_count(), 0);
        assert!(registry.registration("wd_default").is_none());
        assert!(registry.default_runtime().is_none());
    }

    // Given two registered workspaces, when a non-default one is set as
    // default, then it becomes the default; an unknown id is rejected.
    #[test]
    fn should_switch_the_default_when_setting_another_registered_workspace() {
        let registry = WorkspaceRegistry::new(
            Arc::new(MapResolver::default()),
            recording_builder(Arc::new(std::sync::Mutex::new(Vec::new()))),
        );
        registry.register_and_build(reg("wd_default")).unwrap();
        registry.register_and_build(reg("wd_other")).unwrap();

        assert!(registry.set_default("wd_other"));
        assert_eq!(
            registry.default_runtime().unwrap().workspace_id.as_str(),
            "wd_other"
        );
        assert!(!registry.set_default("wd_unknown"));
        assert_eq!(
            registry.default_runtime().unwrap().workspace_id.as_str(),
            "wd_other"
        );
    }

    // Given a persisted session whose state.json lives in a built non-default
    // runtime's own bucket (no index, no active entry), when manager_for
    // resolves it, then the bucket probe attributes it to that runtime —
    // the no-index engine's only resolution source, and honest attribution
    // (the directory physically lives in that workspace's bucket), never a
    // silent default.
    #[test]
    fn should_attribute_a_persisted_session_to_the_runtime_owning_its_bucket() {
        let root_default = tempfile::tempdir().expect("default tempdir");
        let root_other = tempfile::tempdir().expect("other tempdir");
        let builder: RuntimeBuilder = {
            let default = root_default.path().to_path_buf();
            let other = root_other.path().to_path_buf();
            Arc::new(move |reg| {
                let root = if reg.id == "wd_other" {
                    &other
                } else {
                    &default
                };
                Ok(rooted_runtime(&reg.id, root))
            })
        };
        let registry = WorkspaceRegistry::new(Arc::new(MapResolver::default()), builder);
        registry.register_and_build(reg("wd_default")).unwrap();
        registry.register(reg("wd_other"));
        let other = registry
            .ensure_built("wd_other")
            .expect("the row is registered")
            .expect("the build succeeds");

        // A persisted session in the OTHER workspace's bucket.
        let sid = SessionId::new("sess-on-disk");
        let state = other.manager.persistence().state_path(&sid);
        std::fs::create_dir_all(state.parent().expect("the session dir has a parent"))
            .expect("create the session dir");
        std::fs::write(&state, "{}").expect("write state.json");

        match registry.manager_for("sess-on-disk") {
            ManagerResolution::Restored {
                workspace_id,
                manager,
            } => {
                assert_eq!(workspace_id, "wd_other");
                assert!(Arc::ptr_eq(&manager, &other.manager));
            }
            other => panic!("must resolve to Restored, got {other:?}"),
        }

        // A session no bucket owns stays NotFound — the probe is
        // attribution, not a default fallback.
        assert!(matches!(
            registry.manager_for("sess-missing"),
            ManagerResolution::NotFound
        ));
    }

    // Given a workspace whose index row names it as the owner of a persisted
    // session, when a home is installed on the registry, then the shared
    // index connection resolves the session (and serves every later reader
    // through the same lazily opened connection).
    #[test]
    fn should_resolve_a_persisted_session_through_the_installed_shared_index() {
        use chrono::Utc;

        let home = tempfile::tempdir().expect("home tempdir");
        let index =
            SessionIndex::open(&home.path().join("session-index.db")).expect("open the index");
        index
            .upsert_session(&mirri_session_index::SessionRow {
                id: "sess-indexed".to_string(),
                workspace_id: "wd_indexed".to_string(),
                cwd: None,
                title: None,
                created_at: Utc::now(),
                updated_at: Utc::now(),
                archived: false,
                tags: Vec::new(),
                custom: None,
                last_prompt: None,
                state_mtime: None,
            })
            .expect("upsert the row");
        drop(index);

        let registry = WorkspaceRegistry::new(
            Arc::new(MapResolver::default()),
            recording_builder(Arc::new(std::sync::Mutex::new(Vec::new()))),
        );
        registry.register_and_build(reg("wd_default")).unwrap();
        registry.register(reg("wd_indexed"));
        registry.install_session_index(home.path().to_path_buf());

        match registry.manager_for("sess-indexed") {
            ManagerResolution::Restored { workspace_id, .. } => {
                assert_eq!(workspace_id, "wd_indexed");
            }
            other => panic!("must resolve to Restored, got {other:?}"),
        }
        // The same connection serves later readers (one lazy open).
        assert!(registry.shared_session_index().is_some());
    }

    // Given several threads racing the first touch of a lazy workspace,
    // when they all call ensure_built, then the builder runs exactly once
    // (the double-checked build lock).
    #[test]
    fn should_build_a_lazily_touched_workspace_exactly_once_when_threads_race() {
        let built_log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let registry = WorkspaceRegistry::new(
            Arc::new(MapResolver::default()),
            recording_builder(Arc::clone(&built_log)),
        );
        registry.register(reg("wd_racy"));

        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let registry = &registry;
                let barrier = &barrier;
                scope.spawn(move || {
                    barrier.wait();
                    registry
                        .ensure_built("wd_racy")
                        .expect("the row is registered")
                        .expect("the build succeeds");
                });
            }
        });

        assert_eq!(
            *built_log.lock().unwrap_or_else(|e| e.into_inner()),
            vec!["wd_racy"]
        );
        assert_eq!(registry.built_count(), 1);
    }
    // --- W5: the D3 alias fold (workspaceRootKey parity) ---------------------

    /// One registration row with an explicit root (the alias tests need
    /// roots that differ as strings but fold as directories).
    fn reg_at(id: &str, root: &str) -> WorkspaceRegistration {
        WorkspaceRegistration {
            id: id.into(),
            root: PathBuf::from(root),
            name: None,
            created_at: "2026-09-11T00:00:00.000Z".into(),
            last_opened_at: "2026-09-11T00:00:00.000Z".into(),
        }
    }

    /// A registry with no rows (the fold helpers are read-only).
    fn empty_registry() -> WorkspaceRegistry {
        WorkspaceRegistry::new(
            Arc::new(MapResolver::default()),
            recording_builder(Arc::new(std::sync::Mutex::new(Vec::new()))),
        )
    }

    // Given the TS `workspaceRootKey` port, when two spellings address the
    // same physical directory, then they fold — trailing separators strip,
    // backslash separators normalize to `/`, and `.`/`..` segments clean
    // through the lexical resolve — while POSIX case variants never fold
    // (only win-shaped strings lowercase, and a resolved POSIX path is
    // never win-shaped) and `~` never expands (an input-boundary concern
    // in TS, not the fold's).
    #[test]
    fn should_fold_root_spellings_the_ts_way() {
        // Trailing separators and backslash separators fold.
        assert_eq!(workspace_root_key(Path::new("/a/b/")), "/a/b");
        assert_eq!(workspace_root_key(Path::new("/a\\b")), "/a/b");
        assert_eq!(
            workspace_root_key(Path::new("/a/b/")),
            workspace_root_key(Path::new("/a\\b"))
        );
        // `.`/`..` segments fold through the lexical resolve.
        assert_eq!(workspace_root_key(Path::new("/a/./b/../c")), "/a/c");
        assert_eq!(workspace_root_key(Path::new("/a/b/c/..")), "/a/b");
        // POSIX case does NOT fold (the win-shaped lowercase arm is
        // Windows-only through the resolved chain — a resolved POSIX path
        // starts with `/`, never a drive letter or `//`).
        assert_ne!(
            workspace_root_key(Path::new("/tmp/Proj")),
            workspace_root_key(Path::new("/tmp/proj"))
        );
        // `~` never expands (the TS side expands at the input boundary).
        let tilde = workspace_root_key(Path::new("~/proj"));
        assert!(tilde.contains('~'), "the tilde survives verbatim: {tilde}");
        if let Ok(home) = std::env::var("HOME") {
            assert_ne!(
                tilde,
                workspace_root_key(Path::new(&format!("{home}/proj"))),
                "the fold never expands ~ against the home directory"
            );
        }
    }

    // Given registered rows, when the alias groups form, then rows whose
    // roots share a folded root key land in ONE group (the separator and
    // trailing-slash variants) while distinct directories — including
    // POSIX case variants — stay apart.
    #[test]
    fn should_group_registrations_by_their_folded_root_key() {
        let registry = empty_registry();
        registry.register(reg_at("wd_fold_a", "/tmp/w5/fold"));
        registry.register(reg_at("wd_fold_b", "/tmp\\w5\\fold\\"));
        registry.register(reg_at("wd_fold_c", "/tmp/w5/./fold/"));
        registry.register(reg_at("wd_other", "/tmp/w5/other"));
        registry.register(reg_at("wd_case_upper", "/tmp/W5"));
        registry.register(reg_at("wd_case_lower", "/tmp/w5"));

        let mut groups = registry.alias_groups();
        groups.sort();
        assert_eq!(
            groups,
            vec![
                vec!["wd_case_lower".to_string()],
                vec!["wd_case_upper".to_string()],
                vec![
                    "wd_fold_a".to_string(),
                    "wd_fold_b".to_string(),
                    "wd_fold_c".to_string()
                ],
                vec!["wd_other".to_string()],
            ],
            "the separator spellings fold; the case variants stay apart"
        );

        // And `alias_group_of` answers the same set for every member.
        for member in ["wd_fold_a", "wd_fold_b", "wd_fold_c"] {
            assert_eq!(
                registry.alias_group_of(member),
                vec![
                    "wd_fold_a".to_string(),
                    "wd_fold_b".to_string(),
                    "wd_fold_c".to_string()
                ],
                "every member of the fold answers the full group"
            );
        }
        // While a POSIX case sibling stays alone (never folded).
        assert_eq!(
            registry.alias_group_of("wd_case_upper"),
            vec!["wd_case_upper".to_string()]
        );
    }

    // Given a registered workspace and a session index whose rows live in
    // an unregistered bucket whose `cwd` spells the same root, when
    // `alias_group_of` runs, then the index-derived sibling joins the group
    // (the rust analogue of the TS legacy `session_index.jsonl` arm — a
    // legacy split bucket counts with its siblings), while an unknown id
    // answers itself alone and an index-less registry never invents
    // siblings.
    #[test]
    fn should_expand_an_alias_group_with_index_split_buckets() {
        let home = tempfile::tempdir().expect("home tempdir");
        let index = mirri_session_index::SessionIndex::open(&home.path().join("session-index.db"))
            .expect("open the index");
        let row = |id: &str, cwd: Option<&str>| mirri_session_index::SessionRow {
            id: format!("sess-{id}"),
            workspace_id: id.to_string(),
            cwd: cwd.map(str::to_string),
            title: None,
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            archived: false,
            tags: Vec::new(),
            custom: None,
            last_prompt: None,
            state_mtime: None,
        };
        index
            .upsert_session(&row("wd_split_bucket", Some("/tmp/w5/split")))
            .expect("the split bucket's row lands");
        index
            .upsert_session(&row("wd_split_case", Some("/tmp/W5/split")))
            .expect("the case-variant row lands (never folds)");
        index
            .upsert_session(&row("wd_split_cwdless", None))
            .expect("the cwd-less row lands (no fold source)");
        drop(index);

        let registry = empty_registry();
        registry.register(reg_at("wd_main", "/tmp/w5\\split"));
        registry.install_session_index(home.path().to_path_buf());

        // The index arm folds by the ROW's cwd (the row names where the
        // session actually lives), so the split bucket joins…
        assert_eq!(
            registry.alias_group_of("wd_main"),
            vec!["wd_main".to_string(), "wd_split_bucket".to_string()],
            "the trailing-backslash root folds with the split bucket's cwd"
        );
        // …while a case-variant cwd never folds (POSIX) and a cwd-less
        // row is no fold source at all — both stay out of the group above.

        // An unknown id answers itself alone — never an error.
        assert_eq!(
            registry.alias_group_of("wd_unknown"),
            vec!["wd_unknown".to_string()]
        );

        // And `alias_groups` (the workspace.list count face) groups ONLY
        // the registered rows — the index-derived siblings ride through
        // `alias_group_of` instead.
        let mut groups = registry.alias_groups();
        groups.sort();
        assert_eq!(
            groups,
            vec![vec!["wd_main".to_string()]],
            "alias_groups partitions registrations only"
        );
    }

    // Given five session-creation touches on a registered workspace with an
    // installed home, when the touches accumulate before any flush, then the
    // registry file is untouched (write amplification red line) while the
    // in-memory row advances; and when the flush hook runs, then exactly ONE
    // merged write lands (the second flush is a no-op).
    #[test]
    fn should_flush_pending_touches_exactly_once_when_quiesced() {
        use serde_json::json;
        let home = tempfile::tempdir().expect("home tempdir");
        let registry_path = home.path().join("workspaces.json");
        let id = mirri_core::encode_work_dir_key(std::path::Path::new("/tmp/w6/touched"));
        let row = json!({
            "version": 1,
            "workspaces": {
                id.as_str(): {
                    "root": "/tmp/w6/touched",
                    "name": "touched",
                    "created_at": "2024-01-01T00:00:00.000Z",
                    "last_opened_at": "2024-06-01T00:00:00.000Z",
                }
            },
            "deleted_workspace_ids": [],
        });
        std::fs::write(&registry_path, serde_json::to_string(&row).unwrap()).unwrap();
        let registry = empty_registry();
        registry.register(reg_at(&id, "/tmp/w6/touched"));
        registry.install_session_index(home.path().to_path_buf());

        // Five touches (session.create's debounce path) — in-memory only.
        for _ in 0..5 {
            assert!(registry.touch_opened(&id));
        }
        assert!(registry.has_pending_registry_touches());
        let on_disk = std::fs::read_to_string(&registry_path).unwrap();
        assert!(
            on_disk.contains("2024-06-01T00:00:00.000Z"),
            "no touch may write the registry before the debounce window ends"
        );
        // But the in-memory row DID advance past the fixture timestamp (the
        // listing sorts on the in-memory value, so the touch is immediately
        // visible there — that is the whole point of the in-memory arm).
        assert_ne!(
            registry.registration(&id).unwrap().last_opened_at,
            "2024-06-01T00:00:00.000Z"
        );

        // Quiesce: the deterministic flush hook performs ONE write…
        assert!(registry.flush_pending_registry_touches());
        assert_eq!(registry.touch_flush_count(), 1);
        assert!(!registry.has_pending_registry_touches());
        let after = std::fs::read_to_string(&registry_path).unwrap();
        assert!(
            !after.contains("2024-06-01T00:00:00.000Z"),
            "the flushed file carries the advanced last_opened_at"
        );

        // …and a second flush is a no-op (nothing pending).
        assert!(!registry.flush_pending_registry_touches());
        assert_eq!(registry.touch_flush_count(), 1);
    }

    // Given a touched workspace with NO installed home (every direct-
    // constructor engine), when the flush runs, then it is a harmless no-op
    // (the touch was in-memory only) — and an unknown id never touches.
    #[test]
    fn should_noop_the_touch_flush_when_no_home_is_installed() {
        let registry = empty_registry();
        registry.register(reg("wd_touchy"));

        assert!(registry.touch_opened("wd_touchy"));
        assert!(!registry.flush_pending_registry_touches());
        assert_eq!(registry.touch_flush_count(), 0);

        // An unknown id is a no-op touch, never an error.
        assert!(!registry.touch_opened("wd_unknown"));
    }

    // Given an armed debounce flag, when a second touch arrives inside the
    // window, then no second timer is armed (the arm is one-shot until the
    // flush drains it).
    #[test]
    fn should_arm_the_touch_flush_timer_only_once_per_window() {
        let registry = empty_registry();
        registry.register(reg("wd_touchy"));

        assert!(registry.try_arm_touch_flush(), "the first touch arms");
        assert!(
            !registry.try_arm_touch_flush(),
            "the second touch inside the window must not re-arm"
        );
        registry.touch_opened("wd_touchy");
        registry.flush_pending_registry_touches();
        assert!(
            registry.try_arm_touch_flush(),
            "after the drain a fresh touch arms again"
        );
    }
}
