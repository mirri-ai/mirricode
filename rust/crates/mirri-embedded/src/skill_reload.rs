//! Skill reload orchestration (ADR-010 decisions 3 + 4): the single
//! serialized entry point every skill-catalog rescan goes through.
//!
//! Four trigger paths converge here — the file watcher (plan Task 5), the
//! config-write resync, the `plugin.reload` skill replay, and the manual
//! `skill.reload` RPC — all sharing [`SkillReloadOrchestrator`]:
//!
//! 1. a per-workspace `std::sync::Mutex` serializes rescans (concurrent
//!    triggers interleave never; the whole replay is synchronous IO with no
//!    await point, so the std lock is safe in both sync and async callers —
//!    it is just never held across an await);
//! 2. the rescan itself replays the discovery sequence onto a **scratch**
//!    [`SkillCatalog`] and swaps it in atomically
//!    ([`SkillCatalog::replace_all`] — bootstrap's
//!    `apply_skill_discovery`, re-parameterized to build off to the side),
//!    so readers never observe the clear-then-discover window;
//! 3. the pre/post snapshots gate the notification through
//!    [`diff_skill_catalogs`]: an empty diff means no event, no session
//!    pending — the rescan itself still ran (config semantics unchanged);
//!    a non-empty diff parks the refresh signal on every live session
//!    ([`SessionScope::mark_skill_catalog_refreshed`] — the interactive
//!    main agent's turn flow flushes it into a next-turn system-reminder)
//!    and broadcasts a workspace-level change event on the
//!    [`tokio::sync::broadcast`] channel (the `mcp_pool_events` precedent
//!    shape).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use mirri_core::{ServerConfig, SkillCatalogDiff, diff_skill_catalogs};
use crate::workspace_registry::WorkspaceRuntime;
use serde_json::{Value, json};

/// Capacity of the workspace-level skill-catalog change broadcast — the
/// same bounded-broadcast philosophy as the MCP pool event channel: the
/// payload is a cheap `Value` clone, a lagged receiver self-disposes by
/// dropping events (the notification surface tolerates gaps; the durable
/// state is the catalog itself), and a `send` failure means "no
/// subscribers" and is ignored.
const SKILL_CATALOG_EVENT_CAPACITY: usize = 256;

/// What triggered a catalog rescan — stamped on the change event so
/// consumers can distinguish user-initiated reloads (the manual RPC /
/// TUI command) from automatic ones (file watcher, config write, plugin
/// reload). The ADR-010 event schema's `trigger` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillReloadTrigger {
    /// A file-system change under a watched skill root (plan Task 5's
    /// watcher).
    File,
    /// A config write to a skill-derived key (`extra_skill_dirs` /
    /// `disabled_skills`) — the resync path.
    Config,
    /// A `plugin.reload` re-deriving plugin skill roots.
    Plugin,
    /// The manual `skill.reload` RPC / TUI `/skills reload` command.
    Manual,
}

impl SkillReloadTrigger {
    /// The wire string for the event payload's `trigger` field.
    fn as_str(self) -> &'static str {
        match self {
            SkillReloadTrigger::File => "file",
            SkillReloadTrigger::Config => "config",
            SkillReloadTrigger::Plugin => "plugin",
            SkillReloadTrigger::Manual => "manual",
        }
    }
}

/// The inputs a rescan needs beyond the runtime itself — exactly the
/// parameters `bootstrap::apply_skill_discovery` replays with (config
/// snapshot, injected home, plugin manager, explicit SDK skill dirs),
/// carried so the orchestrator can be invoked from async contexts that
/// already hold the snapshots.
pub struct SkillReloadContext<'a> {
    /// The current server configuration snapshot (the config-derived
    /// discovery pieces: `extra_skill_dirs`, `disabled_skills`).
    pub config: &'a ServerConfig,
    /// The injected Mirri home, when one exists — resolves the user-brand
    /// `<home>/skills` root.
    pub home_dir: Option<&'a Path>,
    /// The plugin manager, when plugins are enabled — contributes plugin
    /// skill roots.
    pub plugin_manager: Option<&'a mirri_plugin_host::PluginManager>,
    /// The SDK's explicit skill directories (v1/v2 `skillDirs`).
    pub skill_dirs: &'a [PathBuf],
}

/// One completed rescan's observable outcome: the name-classified diff
/// between the pre-rescan snapshot and the post-swap table (empty when the
/// catalog did not change — the rescan still ran).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillReloadOutcome {
    /// The catalog diff this rescan produced (empty buckets = no change).
    pub diff: SkillCatalogDiff,
    /// Whether every live session's pending-reminder flag was parked by
    /// this rescan (diff non-empty; the flush itself is the turn flow's
    /// job, not the orchestrator's).
    pub sessions_marked: usize,
}

impl SkillReloadOutcome {
    /// Whether the rescan changed the catalog (the diff-gate answer —
    /// `true` means the notification fired).
    pub fn changed(&self) -> bool {
        !(self.diff.added.is_empty()
            && self.diff.removed.is_empty()
            && self.diff.updated.is_empty())
    }
}

/// The serialized skill-catalog rescan entry point (ADR-010 decision 3):
/// per-workspace mutex + the workspace-level change-event broadcast.
///
/// The per-workspace serialization is deliberately **not** a map keyed by
/// workspace id — a growing registry of mutexes needs removal bookkeeping,
/// and every current trigger path already holds the workspace's
/// [`Arc<WorkspaceRuntime>`] when it decides to rescan. Instead a single
/// server-scoped mutex serializes ALL workspaces: the critical section is
/// a millisecond-scale synchronous directory replay with no await point,
/// so cross-workspace head-of-line blocking is bounded and rare (rescans
/// are themselves rate-limited by their triggers' quiet windows).
pub struct SkillReloadOrchestrator {
    /// Per-workspace rescan serialization (ADR-010 decision 3 — "按
    /// workspace 串行化", its literal shape): a watcher rescan and a
    /// config rescan for THE SAME workspace must never interleave their
    /// snapshot → replay → swap → diff sequences. Distinct workspaces
    /// rescan in parallel (the old server-wide single mutex head-of-line
    /// blocked them — a multi-workspace server saving skills in each
    /// workspace paid every other workspace's walk). `std::sync::Mutex`
    /// because the whole rescan is synchronous (no await point — never
    /// held across an await, so `tokio::sync::Mutex::blocking_lock`'s
    /// async-context panic hazard never applies). The map itself is an
    /// outer short-held lock guarding the inner per-workspace guards.
    locks: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// The workspace-level skill-catalog change broadcast (ADR-010
    /// decision 4): every completed rescan with a non-empty diff sends one
    /// event here, the `mcp_pool_events` precedent shape. A `send`
    /// failure means "no subscribers" and is ignored.
    events: tokio::sync::broadcast::Sender<Value>,
    /// The camelCase twin of [`Self::events`] the serve-wire subscription
    /// face (`subscribe_skill_catalog_events` → the `skill.catalogChanged`
    /// notification) hands out — the same events re-keyed at the boundary
    /// (see [`Self::subscribe_camelcase`]).
    wire_events: tokio::sync::broadcast::Sender<Value>,
}

impl Default for SkillReloadOrchestrator {
    fn default() -> Self {
        Self::new()
    }
}

impl SkillReloadOrchestrator {
    /// Create the orchestrator with a fresh event broadcast channel.
    pub fn new() -> Self {
        Self {
            locks: Mutex::new(HashMap::new()),
            events: tokio::sync::broadcast::channel(SKILL_CATALOG_EVENT_CAPACITY).0,
            wire_events: tokio::sync::broadcast::channel(SKILL_CATALOG_EVENT_CAPACITY).0,
        }
    }

    /// Subscribe to the workspace-level skill-catalog change events: one
    /// payload per completed rescan whose diff was non-empty, shaped
    /// `{ workspace_id, added[], removed[], updated[], trigger }` (counts
    /// derive from the array lengths).
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Value> {
        self.events.subscribe()
    }

    /// The serve-wire subscription shape ([`Self::subscribe`] re-keyed to
    /// the camelCase notification payload `skill.catalogChanged` carries:
    /// `{ workspaceId, added, removed, updated, trigger }`). A dedicated
    /// channel so the wire consumers and the internal Rust consumers each
    /// observe one stable schema — the internal snake_case one stays as
    /// the ADR-010 event schema.
    pub fn subscribe_camelcase(&self) -> tokio::sync::broadcast::Receiver<Value> {
        self.wire_events.subscribe()
    }

    /// Rescan one workspace's skill catalog under the serialization lock
    /// (ADR-010 decisions 2-4): snapshot → replay onto a scratch catalog
    /// → atomic `replace_all` swap → diff → non-empty ⇒ park every live
    /// session's pending-reminder flag + broadcast the change event.
    ///
    /// Sync by design: the whole critical section is synchronous IO (the
    /// discovery replay has no await point), so both sync callers
    /// (`resync_skill_catalogs`) and async callers (the watcher task,
    /// `refresh_plugin_consumers`, the reload RPC) invoke it directly —
    /// the std lock is never held across an await.
    ///
    /// The session-marking step is the one part that needs an async
    /// primitive (workspace session enumeration is a tokio lock); inside
    /// a Tokio runtime it runs via `block_in_place` + a current-thread
    /// barrier task, outside one it spawns a blocking thread — either way
    /// the caller's context never blocks the async reactor.
    pub fn reload_workspace(
        &self,
        runtime: &Arc<WorkspaceRuntime>,
        ctx: &SkillReloadContext<'_>,
        trigger: SkillReloadTrigger,
    ) -> SkillReloadOutcome {
        // Serialize the whole read-replay-swap-diff sequence for THIS
        // workspace (decision 3): a concurrent trigger for the same
        // workspace waits here, then replays over this rescan's
        // already-swapped table — an idempotent replay whose diff is
        // then empty (the notification gate absorbs the double
        // trigger). Other workspaces proceed on their own guards.
        let workspace_lock = {
            let mut locks = self
                .locks
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            Arc::clone(locks.entry(runtime.workspace_id.clone()).or_default())
        };
        let diff = {
            let _serial = workspace_lock
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());

            let catalog = runtime.scope.skills();
            let before = catalog.snapshot();

            // Replay the complete discovery sequence onto a scratch
            // catalog and swap it in atomically (bootstrap's rescan
            // function — the same code path the assembly first build
            // uses).
            crate::bootstrap::apply_skill_discovery(
                &runtime.scope,
                ctx.config,
                ctx.home_dir,
                ctx.plugin_manager,
                ctx.skill_dirs,
            );

            let after = catalog.snapshot();
            let diff = diff_skill_catalogs(&before, &after);
            let changed = !(diff.added.is_empty()
                && diff.removed.is_empty()
                && diff.updated.is_empty());
                if !changed {
                    // The diff gate (decision 4): an unchanged catalog
                    // means no notification and no session pending. The
                    // rescan itself still ran — the config-write
                    // semantics ("replay takes effect") are unchanged by
                    // the gate.
                    return SkillReloadOutcome {
                        diff,
                        sessions_marked: 0,
                    };
                }
                diff
        };

        // Park the refresh signal on every live session of this
        // workspace (decision 5: park-and-flush — only the interactive
        // main agent's turn flow carries the handle that flushes it;
        // every session is marked because the session scope IS the
        // per-session boundary). OUTSIDE the serial guard: the guard's
        // job is the snapshot → replay → swap → diff sequence — holding
        // it through the marking thread (a thread spawn + a short-lived
        // runtime, ~0.5–2ms) only lengthens the same-workspace queue.
        let sessions_marked = mark_live_sessions(runtime);

        // Broadcast the workspace-level change event (decision 4): the
        // payload carries the name lists and the trigger; counts derive
        // from the array lengths on the consumer side.
        let payload = json!({
            "workspace_id": runtime.workspace_id,
            "added": diff.added,
            "removed": diff.removed,
            "updated": diff.updated,
            "trigger": trigger.as_str(),
        });
        // A send failure means "no subscribers" — the notification
        // surface tolerates it (the catalog swap is already durable).
        let _ = self.events.send(payload.clone());
        // The serve-wire twin (the `skill.catalogChanged` notification
        // payload): the same event re-keyed to camelCase for the wire
        // consumers (`subscribe_skill_catalog_events`).
        let wire_payload = json!({
            "workspaceId": runtime.workspace_id,
            "added": diff.added,
            "removed": diff.removed,
            "updated": diff.updated,
            "trigger": trigger.as_str(),
        });
        let _ = self.wire_events.send(wire_payload);

        SkillReloadOutcome {
            diff,
            sessions_marked,
        }
    }
}

/// Park the skill-catalog refresh signal on every live session of this
/// workspace (ADR-010 decision 5): the enumeration itself is async (the
/// workspace's session map is a tokio lock), so this runs on a dedicated
/// short-lived thread's own single-threaded runtime and joins it — safe in
/// every caller context (multi-thread runtime, current-thread runtime,
/// sync no-runtime) with no `block_in_place` panic hazards, and never
/// holding the orchestrator's std lock across the caller's own awaits
/// (this whole function is synchronous from the caller's view). Returns
/// the number of sessions marked.
fn mark_live_sessions(runtime: &Arc<WorkspaceRuntime>) -> usize {
    let scope = Arc::clone(&runtime.scope);
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("skill-reload session-marking runtime")
            .block_on(async move { mark_sessions_async(&scope).await })
    })
    .join()
    .unwrap_or_else(|panic| {
        // A panic in the marking thread (runtime build failure included —
        // the `expect` above) must not be silently swallowed: the reload
        // itself succeeded, only the reminder parking degraded.
        tracing::warn!(
            panic = ?panic,
            "skill-reload session marking failed; live sessions did not get the pending reminder flag"
        );
        0
    })
}

/// The async core of the session-marking step: enumerate the workspace's
/// live sessions and park the refresh signal on each.
async fn mark_sessions_async(scope: &Arc<mirri_core::WorkspaceScope>) -> usize {
    let session_ids = scope.list_sessions().await;
    let mut marked = 0;
    for id in session_ids {
        if let Some(session) = scope.get_session(&id).await {
            session.mark_skill_catalog_refreshed();
            marked += 1;
        }
    }
    marked
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootstrap::test_env;
    use mirri_core::SessionId;
    use mirri_core::scope::SessionMetadata;

    /// A session resolver that knows nothing — the isolated test registry's
    /// attribution fallback (no shared session index in these tests).
    struct NoopResolver;

    impl crate::workspace_registry::SessionWorkspaceResolver for NoopResolver {
        fn workspace_of_session(&self, _session_id: &str) -> Option<String> {
            None
        }
    }

    /// Write one skill directory (`<root>/<name>/SKILL.md`) under `root`.
    fn write_skill(root: &Path, name: &str, description: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).expect("skill dir");
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\ndescription: {description}\n---\n# {name}\n"),
        )
        .expect("SKILL.md");
    }

    /// The hermetic HOME pin the bootstrap skill tests use — the default
    /// roots read `$HOME/.agents/skills` at replay time, so the pin keeps
    /// a parallel test's real home from leaking skills in.
    fn with_pinned_home<T>(isolated: &Path, f: impl FnOnce() -> T) -> T {
        let _home_lock = test_env::home_lock();
        let previous_home = std::env::var_os("HOME");
        // SAFETY: restored immediately after `f`, under the crate home
        // lock (the existing bootstrap tests' pin pattern).
        unsafe { std::env::set_var("HOME", isolated) };
        let out = f();
        // SAFETY: see the set_var above.
        match previous_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        out
    }

    /// A built runtime over a temporary workspace root — built through
    /// the registry's `register_and_build` (the same
    /// `build_workspace_runtime` path every workspace goes through, with
    /// a minimal hand-assembled `SharedEngineServices`: empty config,
    /// an isolated Mirri home, no plugins / MCP / profiles).
    fn built_runtime(
        workspace_root: &tempfile::TempDir,
        home: &tempfile::TempDir,
        id: &str,
    ) -> Arc<WorkspaceRuntime> {
        use crate::bootstrap::SharedEngineServices;
        use crate::providers::ConfigAwareLlmFactory;

        let config: Arc<std::sync::RwLock<ServerConfig>> =
            Arc::new(std::sync::RwLock::new(ServerConfig::default()));
        let shared = Arc::new(SharedEngineServices {
            config,
            profile_manager: Arc::new(mirri_core::ProfileManager::new()),
            user_mcp_pool: None,
            plugin_manager: None,
            llm_factory: Arc::new(ConfigAwareLlmFactory::new(
                Arc::new(std::sync::RwLock::new(ServerConfig::default())),
                Some(home.path().to_path_buf()),
            )),
            home_dir: Some(home.path().to_path_buf()),
            mcp_home_base: None,
            policy: crate::bootstrap::WorkspacePolicy::UntrustedDefault,
            skill_dirs: Vec::new(),
            trust_store: None,
            default_registration_id: None,
            default_scope_id: None,
        });
        let registry = crate::workspace_registry::WorkspaceRegistry::new(
            Arc::new(NoopResolver),
            Arc::new(move |reg| {
                crate::bootstrap::build_workspace_runtime(reg, &shared)
            }),
        );
        registry.register_and_build(crate::workspace_registry::WorkspaceRegistration {
            id: id.to_string(),
            root: workspace_root.path().to_path_buf(),
            name: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            last_opened_at: "2026-01-01T00:00:00Z".to_string(),
        })
        .expect("workspace runtime build")
    }

    /// Given a workspace with two skills, when a third skill directory is
    /// added and `reload_workspace` runs, then the diff reports the new
    /// name, the broadcast carries one event with the name list and the
    /// trigger, and every live session's pending flag is set.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn should_diff_broadcast_and_mark_sessions_when_a_skill_is_added() {
        let workspace_root = tempfile::tempdir().expect("workspace tempdir");
        let home = tempfile::tempdir().expect("home tempdir");
        let isolated_home = tempfile::tempdir().expect("isolated $HOME tempdir");
        write_skill(&workspace_root.path().join(".agents/skills"), "alpha", "first");
        write_skill(&workspace_root.path().join(".agents/skills"), "beta", "second");

        let runtime = with_pinned_home(isolated_home.path(), || {
            built_runtime(&workspace_root, &home, "ws-add")
        });
        let catalog = runtime.scope.skills();
        assert!(
            catalog.get("alpha").is_some() && catalog.get("beta").is_some(),
            "the assembly's first build discovered both seed skills"
        );

        // One live session in the workspace (the interactive flush's
        // consumer — here we only assert the pending flag itself).
        let session_id = SessionId::new("reload-test-session");
        let session = runtime
            .scope
            .create_session(
                session_id.clone(),
                SessionMetadata::default(),
            )
            .await
            .expect("session create");

        write_skill(&workspace_root.path().join(".agents/skills"), "gamma", "third");

        let orchestrator = SkillReloadOrchestrator::new();
        let mut events = orchestrator.subscribe();
        let config = ServerConfig::default();
        let ctx = SkillReloadContext {
            config: &config,
            home_dir: Some(home.path()),
            plugin_manager: None,
            skill_dirs: &[],
        };

        // When the rescan runs (under the pinned $HOME — the default
        // roots re-read it at replay time)
        let outcome = with_pinned_home(isolated_home.path(), || {
            orchestrator.reload_workspace(&runtime, &ctx, SkillReloadTrigger::File)
        });

        // Then the diff reports exactly the added name
        assert_eq!(outcome.diff.added, vec!["gamma".to_string()]);
        assert!(outcome.diff.removed.is_empty());
        assert!(outcome.diff.updated.is_empty());
        assert_eq!(outcome.sessions_marked, 1);

        // And the broadcast carried one event with the name list and the
        // trigger
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), events.recv())
            .await
            .expect("event within timeout")
            .expect("channel open");
        assert_eq!(event["workspace_id"], json!(runtime.workspace_id));
        assert_eq!(event["added"], json!(["gamma"]));
        assert_eq!(event["removed"], json!([]));
        assert_eq!(event["updated"], json!([]));
        assert_eq!(event["trigger"], json!("file"));

        // And the live session's pending flag is set (the flush is the
        // turn flow's job — here only the parking is observable)
        assert!(session.take_pending_skill_reminder());
        // Idempotent take: the second read is clear.
        assert!(!session.take_pending_skill_reminder());
    }

    /// Given a workspace whose skills did not change, when
    /// `reload_workspace` runs, then the diff is empty, no event is
    /// broadcast, and no session pending is parked.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn should_not_notify_or_mark_when_nothing_changed() {
        let workspace_root = tempfile::tempdir().expect("workspace tempdir");
        let home = tempfile::tempdir().expect("home tempdir");
        let isolated_home = tempfile::tempdir().expect("isolated $HOME tempdir");
        write_skill(&workspace_root.path().join(".agents/skills"), "alpha", "first");

        let runtime = with_pinned_home(isolated_home.path(), || {
            built_runtime(&workspace_root, &home, "ws-quiet")
        });
        let session_id = SessionId::new("reload-test-session-quiet");
        let session = runtime
            .scope
            .create_session(session_id, SessionMetadata::default())
            .await
            .expect("session create");

        let orchestrator = SkillReloadOrchestrator::new();
        let mut events = orchestrator.subscribe();
        let config = ServerConfig::default();
        let ctx = SkillReloadContext {
            config: &config,
            home_dir: Some(home.path()),
            plugin_manager: None,
            skill_dirs: &[],
        };

        // When the rescan runs over an unchanged disk state
        let outcome = with_pinned_home(isolated_home.path(), || {
            orchestrator.reload_workspace(&runtime, &ctx, SkillReloadTrigger::Manual)
        });

        // Then the diff is empty, no session was marked, and no event
        // arrives (the timeout fires on an idle channel)
        assert!(!outcome.changed());
        assert_eq!(outcome.sessions_marked, 0);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(200), events.recv())
                .await
                .is_err(),
            "no event must be broadcast for an unchanged catalog"
        );
        assert!(!session.take_pending_skill_reminder());
    }

    /// Given two concurrent rescan triggers for the same workspace, when
    /// both run, then they complete serially — the second replays over the
    /// first's swapped table and produces an empty diff (the
    /// double-trigger degradation ADR-010 decision 3 predicts).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn should_serialize_concurrent_reloads_of_the_same_workspace() {
        let workspace_root = tempfile::tempdir().expect("workspace tempdir");
        let home = tempfile::tempdir().expect("home tempdir");
        let isolated_home = tempfile::tempdir().expect("isolated $HOME tempdir");
        write_skill(&workspace_root.path().join(".agents/skills"), "alpha", "first");

        let runtime = with_pinned_home(isolated_home.path(), || {
            built_runtime(&workspace_root, &home, "ws-concurrent")
        });
        write_skill(&workspace_root.path().join(".agents/skills"), "beta", "second");

        let orchestrator = Arc::new(SkillReloadOrchestrator::new());

        // When two triggers race (the same disk state — exactly one
        // first-comer's diff is non-empty; the serialized second sees the
        // already-swapped table)
        let mut handles = Vec::new();
        for _ in 0..2 {
            let orchestrator = Arc::clone(&orchestrator);
            let runtime = Arc::clone(&runtime);
            let home = home.path().to_path_buf();
            let isolated = isolated_home.path().to_path_buf();
            handles.push(tokio::task::spawn_blocking(move || {
                with_pinned_home(&isolated, || {
                    let config = ServerConfig::default();
                    let ctx = SkillReloadContext {
                        config: &config,
                        home_dir: Some(&home),
                        plugin_manager: None,
                        skill_dirs: &[],
                    };
                    orchestrator.reload_workspace(&runtime, &ctx, SkillReloadTrigger::File)
                })
            }));
        }
        let outcomes: Vec<SkillReloadOutcome> = futures::future::join_all(handles)
            .await
            .into_iter()
            .map(|joined| joined.expect("reload task"))
            .collect();

        // Then both completed (the serialization admitted both) and the
        // non-empty diffs sum to exactly one "beta" addition — the second
        // rescan replayed over the first's table and diffed empty.
        assert_eq!(outcomes.len(), 2);
        let total_added: usize = outcomes
            .iter()
            .map(|o| o.diff.added.iter().filter(|n| n.as_str() == "beta").count())
            .sum();
        assert_eq!(
            total_added, 1,
            "exactly one of the two serialized rescans may report the addition"
        );
        // The swapped table serves the new skill.
        assert!(runtime.scope.skills().get("beta").is_some());
    }
}
