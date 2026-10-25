//! Skill directory watcher (ADR-010 decision 1, plan Task 5): the file-
//! system trigger for the skill-catalog hot-reload loop.
//!
//! One watcher per built workspace runtime, mounted right after the
//! runtime is built and dropped when the registry removes the workspace
//! (the lifetime rule of decision 1). The listen set is the full
//! four-channel root list ([`WorkspaceScope::skill_root_paths`]) plus the
//! config `extra_skill_dirs` and the plugin-derived roots — refreshed and
//! re-mounted idempotently after every completed reload, because the
//! config can change the extra dirs and a `plugin.reload` can add or
//! remove plugin roots.
//!
//! Architecture (all shaped after the `mirri-cli` watcher's proven
//! skeleton — `recommended_watcher` + a mpsc bridge + a quiet period +
//! watch-loss retry — rewritten as an embedded-side minimal version; the
//! CLI side stays untouched by plan fiat):
//!
//! - a candidate root that does not exist yet is watched through its
//!   nearest **existing** ancestor, and a create event under that
//!   ancestor re-runs the mount refresh so the now-existing root gets
//!   its own watch (the mcp watcher's parent-dir precedent);
//! - a burst of events collapses through a ~1 s quiet period (wider than
//!   the config watcher's 200 ms — a skill author's save→tweak→save
//!   cycle is more common) before one `reload_workspace(File)` runs;
//! - a watch the OS dropped is re-registered after 2 s, retried on the
//!   same cadence until it succeeds (the `WATCH_RETRY_DELAY` precedent);
//! - the spawned task owns an explicit shutdown channel — dropping the
//!   [`SkillDirectoryWatcher`] handle sends the signal and the task's
//!   select loop exits, so no runtime removal leaks a watcher task.
//!
//! The watcher holds strong references to the orchestrator (it triggers
//! reloads) and the workspace runtime; nothing in the orchestrator or
//! the runtime points back at the watcher, so no `Arc` cycle forms and
//! the registry drop actually releases the task.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use notify::Watcher;
use tokio::sync::mpsc::UnboundedReceiver;

use mirri_core::ServerConfig;

use crate::skill_reload::{
    SkillReloadContext, SkillReloadOrchestrator, SkillReloadTrigger,
};
use crate::workspace_registry::WorkspaceRuntime;

/// How long the event stream must stay quiet before the pending reload
/// runs — one save burst (temp write + rename, or save→tweak→save) must
/// collapse into one rescan. ~1 s per ADR-010 decision 1, deliberately
/// wider than the config watcher's 200 ms.
const QUIET_PERIOD: Duration = Duration::from_millis(1000);

/// Delay before re-registering a watch the OS dropped (e.g. the watched
/// directory was deleted and recreated) — the `mirri-cli` watcher's
/// `WATCH_RETRY_DELAY` precedent.
const WATCH_RETRY_DELAY: Duration = Duration::from_secs(2);

/// A handle to one workspace's running skill-directory watcher.
///
/// Dropping the handle signals the watcher task to exit (the explicit
/// shutdown channel — a spawned task holding a watcher never stops on
/// its own, so the drop path MUST be observable), which is what the
/// registry's workspace removal relies on to reclaim the task.
pub struct SkillDirectoryWatcher {
    /// The shutdown signal — dropped (closing the channel) when this
    /// handle drops; the watcher task's select loop observes the close
    /// and exits.
    _shutdown: tokio::sync::oneshot::Sender<()>,
}

impl Drop for SkillDirectoryWatcher {
    fn drop(&mut self) {
        // Closing the oneshot (by letting the sender drop) IS the
        // signal; the task's `recv()` resolves `Err` (closed) and the
        // select loop breaks.
    }
}

/// The reload-side inputs a watcher needs beyond the runtime itself —
/// the same snapshot shape [`SkillReloadContext`] replays with, owned
/// so the watcher task can re-derive both the context and the listen
/// set whenever a refresh moment arrives (a file event, or the periodic
/// watch-loss retry).
#[derive(Clone)]
pub struct SkillWatcherInputs {
    /// The live server configuration handle the reload replays with —
    /// re-read on every trigger, so a config write that adds
    /// `extra_skill_dirs` reaches BOTH the catalog (the rescan) and the
    /// listen set (the re-mount) without re-creating the watcher.
    pub config: Arc<std::sync::RwLock<ServerConfig>>,
    /// The injected Mirri home, when one exists.
    pub home_dir: Option<PathBuf>,
    /// The plugin manager, when plugins are enabled — contributes the
    /// plugin-derived roots to both the replay and the listen set.
    pub plugin_manager: Option<Arc<mirri_plugin_host::PluginManager>>,
    /// The SDK's explicit skill directories (v1/v2 `skillDirs`).
    pub skill_dirs: Vec<PathBuf>,
}

/// The complete set of directories the watcher should be listening on:
/// the scope's four-channel roots, the config's resolved `extra_skill_
/// dirs`, and the plugin-derived roots — deduplicated and sorted, so
/// two successive computations diff trivially (the idempotent
/// re-mount).
fn compute_watch_set(runtime: &WorkspaceRuntime, inputs: &SkillWatcherInputs) -> Vec<PathBuf> {
    let mut set: BTreeSet<PathBuf> = runtime
        .scope
        .skill_root_paths(inputs.home_dir.as_deref())
        .into_iter()
        .collect();
    if inputs.skill_dirs.is_empty() {
        // Read through the live handle: the watch set must track config
        // writes, not the mount-time snapshot (the fresh-inputs half of
        // ADR-010 decision 4's dynamic listen-set refresh).
        let config = inputs
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner());
        for entry in config.extra_skill_dirs.iter().flatten() {
            set.insert(crate::bootstrap::resolve_extra_skill_dir_pub(
                entry,
                runtime.scope.cwd(),
            ));
        }
    } else {
        set.extend(inputs.skill_dirs.iter().cloned());
    }
    if let Some(manager) = &inputs.plugin_manager {
        set.extend(
            manager
                .plugin_skill_roots()
                .into_iter()
                .map(|root| root.path),
        );
    }
    set.into_iter().collect()
}

/// A per-`SkillReloadContext`-shaped snapshot the watcher task rebuilds
/// at every trigger (config and plugin roots may change between mounts).
fn reload_context<'a>(
    inputs: &'a SkillWatcherInputs,
    config: &'a ServerConfig,
) -> SkillReloadContext<'a> {
    SkillReloadContext {
        config,
        home_dir: inputs.home_dir.as_deref(),
        plugin_manager: inputs.plugin_manager.as_deref(),
        skill_dirs: &inputs.skill_dirs,
    }
}

/// Mount one watch on `target` (recursive) — the parent-fallback rule
/// applies when `target` does not exist: the nearest existing ancestor
/// is watched instead, so the target's later creation is observed.
/// Returns the path actually watched (`None` when nothing in the chain
/// exists — the root stays unwatched until the next refresh).
fn watch_with_parent_fallback(watcher: &mut impl Watcher, target: &Path) -> Option<PathBuf> {
    if target.is_dir() {
        return watcher
            .watch(target, notify::RecursiveMode::Recursive)
            .ok()
            .map(|()| target.to_path_buf());
    }
    // The nearest existing ancestor of a MISSING directory may itself be
    // a file (e.g. a `.tmp` file squatting on the brand dir's name) —
    // only directories can be watched.
    for ancestor in target.ancestors().skip(1) {
        if ancestor.is_dir() {
            return watcher
                .watch(ancestor, notify::RecursiveMode::Recursive)
                .ok()
                .map(|()| ancestor.to_path_buf());
        }
    }
    None
}

/// The skill manifest filename every discovered skill directory carries
/// (`<root>/<name>/SKILL.md`) — the only file whose change can alter the
/// catalog's manifest-visible fields. Body edits are invisible to the
/// catalog (activation reads the file live), so this is the correct
/// filter granularity.
const SKILL_MANIFEST_FILENAME: &str = "SKILL.md";

/// True for event kinds that represent an actual filesystem change
/// (the `mirri-cli` watcher's `is_mutating_kind` — `Remove` included for
/// atomic editor saves).
fn is_mutating_kind(kind: &notify::EventKind) -> bool {
    matches!(
        kind,
        notify::EventKind::Create(_)
            | notify::EventKind::Modify(_)
            | notify::EventKind::Remove(_)
    )
}

/// Whether an event can change the catalog: a create/modify/remove of a
/// `SKILL.md` manifest, or a directory appearing/disappearing, anywhere
/// under a watched root. The filename gate is what keeps unrelated
/// churn — session writes, scratch files, logs — from arming the
/// debounce and paying a rescan (the `mirri-cli` watcher's
/// `is_relevant_file_event` precedent: narrow the filter, not the
/// watch).
///
/// Directory changes must arm the gate too, because the two watch
/// backends disagree about nested creates. FSEvents is path-recursive,
/// so a brand-new `SKILL.md` is reported however its directory appeared.
/// inotify only reports events for directories it holds a watch on, and
/// notify installs that watch when it *processes* the parent's
/// `Create(Folder)` — so `create_dir_all` followed by an immediate
/// write (how `cp -r`, `git checkout` and any skill installer land a
/// skill) writes the manifest before the new watch exists, and the
/// manifest event is lost. Hot reload then silently stops working on
/// Linux. Triggering on the directory create instead is
/// race-independent: the quiet period that follows is orders of
/// magnitude longer than the manifest write, so the rescan always sees
/// the manifest on disk. Removal is the symmetric case — deleting a
/// skill directory reports the directory, never the manifest inside it.
/// A directory event under a parent-fallback watch can over-trigger a
/// rescan of a root the change did not touch; the empty-diff gate
/// absorbs it (no event, no session pending).
fn is_catalog_relevant(event: &notify::Event) -> bool {
    if !is_mutating_kind(&event.kind) {
        return false;
    }
    let is_directory_change = matches!(
        event.kind,
        notify::EventKind::Create(notify::event::CreateKind::Folder)
            | notify::EventKind::Remove(notify::event::RemoveKind::Folder)
    );
    is_directory_change
        || event.paths.iter().any(|path| {
            path.file_name().and_then(|n| n.to_str()) == Some(SKILL_MANIFEST_FILENAME)
        })
}

/// Re-mount the listen set onto the watcher: unwatch everything not in
/// the new set, watch everything new. Returns the paths now watched
/// (both real roots and fallback ancestors). Idempotent by construction
/// (notify deduplicates an already-registered identical watch).
fn remount(
    watcher: &mut impl Watcher,
    watch_set: &[PathBuf],
    currently_watched: &mut Vec<PathBuf>,
) {
    let desired: BTreeSet<&Path> = watch_set.iter().map(|p| p.as_path()).collect();
    // Unwatch the roots that left the set (their parent-fallback mounts
    // stay — they are shared and harmless; a stale fallback only
    // produces an over-trigger the diff gate absorbs).
    let stale: Vec<PathBuf> = currently_watched
        .iter()
        .filter(|watched| !desired.contains(watched.as_path()))
        .cloned()
        .collect();
    for path in &stale {
        let _ = watcher.unwatch(path);
    }
    // Watch the roots that joined. A path already in `currently_watched`
    // is skipped WITHOUT a `watch()` call: notify 6's macOS backend has no
    // registered-path dedup — every `watch()` appends the path AND
    // stops/recreates the whole FSEventStream (resuming from SinceNow,
    // which drops events racing the rebuild). Re-watching the unchanged
    // majority on every reload both duplicates stream entries and opens
    // that drop window each time; skipping them makes a no-diff reload
    // touch nothing and a one-root change rebuild once.
    let mut watched = Vec::new();
    for root in watch_set {
        let already_watched = currently_watched.iter().any(|watched| watched == root);
        if already_watched {
            watched.push(root.clone());
            continue;
        }
        if let Some(mounted) = watch_with_parent_fallback(watcher, root) {
            watched.push(mounted);
        }
    }
    *currently_watched = watched;
}

/// Spawn one workspace's skill-directory watcher.
///
/// Returns `None` — with a `warn!` — when the notify watcher cannot even
/// be created (ADR-010 decision 8: that workspace's automatic awareness
/// silently degrades; every other feature, including the manual reload,
/// is unaffected). The listen-set computation (path canonicalization)
/// and the initial mount (recursive FS watches over potentially large
/// skill trees) are **moved onto the spawned task** — this function only
/// creates the notify watcher and returns a handle immediately, so the
/// lazy `call_method` mount pass never blocks an RPC on filesystem
/// work (the t8 slow-MCP pin: `session.create` must not pay for the
/// watcher's directory scans).
pub async fn spawn_skill_watcher(
    runtime: &Arc<WorkspaceRuntime>,
    orchestrator: &Arc<SkillReloadOrchestrator>,
    inputs: SkillWatcherInputs,
) -> Option<SkillDirectoryWatcher> {
    let workspace_id = runtime.workspace_id.clone();

    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let (error_tx, mut error_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let mut watcher = match notify::recommended_watcher(
        move |result: notify::Result<notify::Event>| match result {
            Ok(event) if is_catalog_relevant(&event) => {
                let _ = event_tx.send(());
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(
                    workspace = %workspace_id,
                    %error,
                    "skill directory watcher reported an event error; re-mounting"
                );
                let _ = error_tx.send(());
            }
        },
    ) {
        Ok(watcher) => watcher,
        Err(error) => {
            tracing::warn!(
                workspace = %runtime.workspace_id,
                %error,
                "cannot start the skill directory watcher; skill changes will not be hot-reloaded"
            );
            return None;
        }
    };
    // The listen-set computation (path canonicalization) and the recursive
    // FS watches are BLOCKING filesystem work — they run on the dedicated
    // blocking pool (a synchronous mount on the async runtime starves
    // every task on the thread; the t8 slow-MCP pin measures exactly
    // that). The mount is AWAITED to completion before the handle
    // returns: a caller proceeding on the handle's existence alone
    // would race the watch registration — a file written in that window
    // is invisible (FSEvents streams from registration) with no later
    // signal to recover it. The first RPC pays this once per workspace.
    let mount_runtime = Arc::clone(runtime);
    let (watch_set, currently_watched, inputs) =
        match tokio::task::spawn_blocking(move || {
            let watch_set = compute_watch_set(&mount_runtime, &inputs);
            let mut currently_watched: Vec<PathBuf> = Vec::new();
            remount(&mut watcher, &watch_set, &mut currently_watched);
            (watch_set, currently_watched, watcher, inputs, mount_runtime)
        })
        .await
        {
            Ok((watch_set, currently_watched, returned_watcher, returned_inputs, _r)) => {
                watcher = returned_watcher;
                (watch_set, currently_watched, returned_inputs)
            }
            Err(panic) => {
                // The closure panicked — its moved state is gone; the
                // honest exit is a failed mount (decision 8's degraded
                // surface: every other feature, manual reload included,
                // is unaffected).
                tracing::warn!(?panic, "skill watcher mount task panicked");
                return None;
            }
        };
    if currently_watched.is_empty() {
        tracing::warn!(
            workspace = %runtime.workspace_id,
            "skill directory watcher has no watchable path; skill hot-reload is disabled"
        );
        return None;
    }
    tracing::info!(
        workspace = %runtime.workspace_id,
        ?currently_watched,
        "started the skill directory watcher"
    );

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let runtime = Arc::clone(runtime);
    let orchestrator = Arc::clone(orchestrator);
    let debounce = QUIET_PERIOD;
    let workspace_id = runtime.workspace_id.clone();
    let mut watch_set = watch_set;
    let mut currently_watched = currently_watched;
    let inputs = inputs;
    tokio::spawn(async move {
        // Keep the notify watcher alive for the whole task lifetime.
        let mut watcher = watcher;
        let mut pending_remount: Option<tokio::time::Instant> = None;

        loop {
            let retry_deadline = pending_remount;
            tokio::select! {
                // Shutdown: the handle dropped — exit the loop, dropping
                // the notify watcher (the OS watches go with it).
                _ = &mut shutdown_rx => break,
                event = event_rx.recv() => {
                    if event.is_none() {
                        break;
                    }
                    wait_for_quiet_period(&mut event_rx, debounce).await;
                    // Shutdown may have landed during the quiet window —
                    // do not fire a trailing reload at a workspace whose
                    // handle (and possibly runtime) is already gone.
                    if shutdown_rx.try_recv().is_ok() {
                        break;
                    }
                    // Re-derive the inputs before the rescan: a config
                    // write since mount time may have changed
                    // `extra_skill_dirs` — the live config handle is
                    // read HERE, once per trigger, and the same snapshot
                    // feeds the reload context AND the listen-set
                    // re-derivation, so a newly configured dir enters
                    // both the catalog and the watch set on the next
                    // trigger (ADR-010 decision 4's dynamic refresh).
                    // KNOWN LIMITATION (open item, macOS): the reload and
                    // the re-mount run INLINE on this async worker — the
                    // same synchronous shape the initial mount used to
                    // need escaping from. Moving them to spawn_blocking
                    // (tried: reload-only, reload+remount, differential
                    // remount) breaks notify 6's FsEventWatcher on
                    // macOS: incremental `watch()` calls after the
                    // watcher instance has round-tripped a
                    // spawn_blocking closure silently stop delivering
                    // events for the newly added paths (the FSEventStream
                    // is stopped and recreated per call, resuming from
                    // SinceNow). Until the notify behavior is understood
                    // or worked around, this arm stays synchronous; a
                    // local-SSD rescan is milliseconds. A network-home
                    // deployment should keep `skillHotReload` off (the
                    // flag's default) and use the manual reload.
                    let config_snapshot = inputs
                        .config
                        .read()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone();
                    let _ = orchestrator.reload_workspace(
                        &runtime,
                        &reload_context(&inputs, &config_snapshot),
                        SkillReloadTrigger::File,
                    );
                    // The completed reload is the refresh moment for the
                    // listen set (decision: the watch set is re-derived
                    // here, inside the watcher task, so no host-side
                    // handle is needed and no Arc cycle can form).
                    watch_set = compute_watch_set(&runtime, &inputs);
                    remount(&mut watcher, &watch_set, &mut currently_watched);
                }
                error = error_rx.recv() => {
                    if error.is_none() {
                        break;
                    }
                    tracing::warn!(
                        workspace = %workspace_id,
                        "skill directory watcher lost a watch; re-mounting"
                    );
                    pending_remount =
                        Some(tokio::time::Instant::now() + WATCH_RETRY_DELAY);
                }
                _ = async {
                    tokio::time::sleep_until(
                        retry_deadline.expect("guarded by retry_deadline.is_some()"),
                    )
                    .await
                }, if retry_deadline.is_some() => {
                    pending_remount = None;
                    remount(&mut watcher, &watch_set, &mut currently_watched);
                    if currently_watched.is_empty() {
                        // Nothing could be mounted yet — keep retrying.
                        pending_remount =
                            Some(tokio::time::Instant::now() + WATCH_RETRY_DELAY);
                    }
                }
            }
        }
        tracing::debug!(
            workspace = %runtime.workspace_id,
            "skill directory watcher task exited"
        );
    });

    Some(SkillDirectoryWatcher {
        _shutdown: shutdown_tx,
    })
}

/// Wait until `receiver` stays empty for the full `quiet_for` window —
/// each event inside the window restarts it (the `mirri-cli` watcher's
/// `wait_for_quiet_period` semantics: a burst collapses into one
/// reload).
async fn wait_for_quiet_period(receiver: &mut UnboundedReceiver<()>, quiet_for: Duration) {
    loop {
        match tokio::time::timeout(quiet_for, receiver.recv()).await {
            Ok(Some(())) => continue,
            Ok(None) | Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bootstrap::test_env;

    /// A session resolver that knows nothing — the isolated test
    /// registry's attribution fallback.
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

    /// Given event kinds the watcher can observe, when the relevance gate
    /// runs, then a skill directory appearing or disappearing arms the
    /// debounce on its own — the inotify backend loses the manifest event
    /// of a directory created and written in one burst (it has no watch
    /// on the new directory yet), so accepting only `SKILL.md` paths
    /// would leave hot reload dead on Linux. Unrelated file and
    /// non-directory churn stays filtered.
    #[test]
    fn should_arm_the_gate_on_directory_events_as_well_as_skill_manifests() {
        use notify::event::{CreateKind, DataChange, ModifyKind, RemoveKind};
        use notify::{Event, EventKind};

        let at = |name: &str| PathBuf::from("/ws/.agents/skills").join(name);
        let event = |kind: EventKind, path: PathBuf| Event::new(kind).add_path(path);

        assert!(
            is_catalog_relevant(&event(
                EventKind::Create(CreateKind::Folder),
                at("brand-new-skill")
            )),
            "a new skill directory must arm the debounce (the missed-watch race)"
        );
        assert!(
            is_catalog_relevant(&event(
                EventKind::Remove(RemoveKind::Folder),
                at("retired-skill")
            )),
            "a deleted skill directory must arm the debounce"
        );
        assert!(
            is_catalog_relevant(&event(
                EventKind::Create(CreateKind::File),
                at("alpha").join("SKILL.md")
            )),
            "a manifest write must still arm the debounce"
        );
        assert!(
            !is_catalog_relevant(&event(EventKind::Create(CreateKind::File), at("scratch.txt"))),
            "an unrelated file must not pay a rescan"
        );
        assert!(
            !is_catalog_relevant(&event(
                EventKind::Modify(ModifyKind::Data(DataChange::Any)),
                at("notes")
            )),
            "a non-directory modify must not pay a rescan"
        );
        assert!(
            !is_catalog_relevant(&event(EventKind::Access(notify::event::AccessKind::Read), at("alpha"))),
            "an access event is not a catalog change"
        );
    }

    /// The hermetic `$HOME` pin — same pattern as the skill_reload tests
    /// (the default roots read `$HOME/.agents/skills` at replay time).
    fn with_pinned_home<T>(isolated: &Path, f: impl FnOnce() -> T) -> T {
        let _home_lock = test_env::home_lock();
        let previous_home = std::env::var_os("HOME");
        // SAFETY: restored immediately after `f`, under the crate home
        // lock.
        unsafe { std::env::set_var("HOME", isolated) };
        let out = f();
        // SAFETY: see the set_var above.
        match previous_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        out
    }

    /// The watcher-flavored variant of [`with_pinned_home`]: the pin is
    /// HELD for the whole test body (an owned guard, not a scoped one) —
    /// the watcher task's reloads and listen-set computations read
    /// `$HOME` at trigger time, milliseconds after the build, so a
    /// scoped pin that restores the real `$HOME` would leak the actual
    /// user's `~/.agents/skills` into every replay (the async-task race
    /// the sync tests never face). The guard releases when the test
    /// ends; the crate home lock serializes the held window against
    /// every other HOME-reading test in the binary.
    fn pinned_home_guard(isolated: &Path) -> (PinnedHomeGuard, PathBuf) {
        let _home_lock = test_env::home_lock();
        let previous_home = std::env::var_os("HOME");
        // SAFETY: held for the rest of the test; restored by the guard's
        // Drop (still under the crate home lock, which the guard also
        // holds — the lock outlives the env restore by construction).
        unsafe { std::env::set_var("HOME", isolated) };
        (
            PinnedHomeGuard {
                _home_lock,
                previous_home,
            },
            isolated.to_path_buf(),
        )
    }

    /// The RAII pin: restores `$HOME` and releases the crate home lock on
    /// drop. The env restore happens while the lock is still held (the
    /// field order keeps `_home_lock` alive until after
    /// `previous_home` is written back).
    struct PinnedHomeGuard {
        _home_lock: std::sync::MutexGuard<'static, ()>,
        previous_home: Option<std::ffi::OsString>,
    }

    impl Drop for PinnedHomeGuard {
        fn drop(&mut self) {
            // SAFETY: restores the pre-test value before the lock guard
            // (declared first) drops — the same serialization window the
            // scoped pin uses.
            match self.previous_home.take() {
                Some(value) => unsafe { std::env::set_var("HOME", value) },
                None => unsafe { std::env::remove_var("HOME") },
            }
        }
    }

    /// A built runtime over a temporary workspace root — the same
    /// hand-assembled-`SharedEngineServices` shape the skill_reload
    /// tests use (empty config, isolated Mirri home, no plugins).
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
        registry
            .register_and_build(crate::workspace_registry::WorkspaceRegistration {
                id: id.to_string(),
                root: workspace_root.path().to_path_buf(),
                name: None,
                created_at: "2026-01-01T00:00:00Z".to_string(),
                last_opened_at: "2026-01-01T00:00:00Z".to_string(),
            })
            .expect("workspace runtime build")
    }

    /// The minimal watcher inputs over an isolated home — empty config,
    /// no plugins, no explicit skill dirs.
    fn inputs(home: &tempfile::TempDir) -> SkillWatcherInputs {
        SkillWatcherInputs {
            config: std::sync::Arc::new(std::sync::RwLock::new(
                ServerConfig::default(),
            )),
            home_dir: Some(home.path().to_path_buf()),
            plugin_manager: None,
            skill_dirs: Vec::new(),
        }
    }

    /// Poll the catalog until `predicate` holds or the deadline passes —
    /// the debounce plus OS-event latency means the reload lands
    /// asynchronously, so the tests observe the durable state (the
    /// swapped catalog), never the task internals.
    async fn wait_until(deadline: Duration, predicate: impl Fn() -> bool) -> bool {
        let started = std::time::Instant::now();
        while started.elapsed() < deadline {
            if predicate() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        predicate()
    }

    /// Given a workspace with a live watcher, when a new skill directory
    /// is written under a watched root, then the quiet period elapses,
    /// one `File`-triggered reload runs, and the swapped catalog serves
    /// the new skill (the broadcast carries the addition).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn should_trigger_a_reload_when_a_skill_is_written_under_a_watched_root() {
        let workspace_root = tempfile::tempdir().expect("workspace tempdir");
        let home = tempfile::tempdir().expect("home tempdir");
        let isolated_home = tempfile::tempdir().expect("isolated $HOME tempdir");
        let skills_root = workspace_root.path().join(".agents/skills");
        write_skill(&skills_root, "alpha", "first");

        // The pin is HELD for the whole test: the watcher task's replays
        // read `$HOME` at trigger time (long after this function's
        // synchronous build section ends).
        let (_pin, _isolated) = pinned_home_guard(isolated_home.path());
        let runtime = built_runtime(&workspace_root, &home, "ws-watch");
        assert!(
            runtime.scope.skills().get("alpha").is_some(),
            "the assembly's first build discovered the seed skill"
        );

        let orchestrator = Arc::new(SkillReloadOrchestrator::new());
        let mut events = orchestrator.subscribe();
        let _watcher = spawn_skill_watcher(&runtime, &orchestrator, inputs(&home))
            .await.expect("watcher spawn");
        // The mount runs on the spawned task — await its completion so
        // the fixture write below is genuinely event-driven (not racing
        // the watch registration).

        // When a new skill lands under the watched root
        write_skill(&skills_root, "beta", "second");

        // Then the quiet period passes and the reload swapped the new
        // skill into the live catalog
        assert!(
            wait_until(Duration::from_secs(10), || {
                runtime.scope.skills().get("beta").is_some()
            })
            .await,
            "the watched skill write must trigger a reload that serves the new skill"
        );

        // And the broadcast carried the addition with the file trigger
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("event within timeout")
            .expect("channel open");
        assert_eq!(event["workspace_id"], serde_json::json!(runtime.workspace_id));
        assert_eq!(event["added"], serde_json::json!(["beta"]));
        assert_eq!(event["trigger"], serde_json::json!("file"));
    }

    /// Given a workspace with a live watcher and a config write that
    /// adds an `extra_skill_dirs` entry pointing at a NEW directory,
    /// when the existing watched root sees another skill write, then the
    /// trigger-time re-derivation picks the fresh config up and the
    /// newly configured extra dir is SERVED (it entered the listen set
    /// via the same snapshot) — the dynamic listen-set refresh (ADR-010
    /// decision 4).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn should_watch_a_configured_extra_skill_dir_after_a_config_write() {
        let workspace_root = tempfile::tempdir().expect("workspace tempdir");
        let home = tempfile::tempdir().expect("home tempdir");
        let isolated_home = tempfile::tempdir().expect("isolated $HOME tempdir");
        let skills_root = workspace_root.path().join(".agents/skills");
        write_skill(&skills_root, "alpha", "first");

        let (_pin, _isolated) = pinned_home_guard(isolated_home.path());
        let runtime = built_runtime(&workspace_root, &home, "ws-extra-dirs");
        let orchestrator = Arc::new(SkillReloadOrchestrator::new());
        let inputs = inputs(&home);
        let _watcher = spawn_skill_watcher(&runtime, &orchestrator, inputs.clone())
            .await
            .expect("watcher spawn (the mount completes before the handle returns)");

        // When the config adds an extra dir (the write lands in the LIVE
        // handle the watcher replays through)
        let extra_root = tempfile::tempdir().expect("extra skill dir");
        write_skill(extra_root.path(), "extra-one", "from the extra dir");
        {
            let mut config = inputs
                .config
                .write()
                .unwrap_or_else(|e| e.into_inner());
            config.extra_skill_dirs = Some(vec![extra_root
                .path()
                .to_str()
                .expect("utf-8 tmpdir path")
                .to_string()]);
        }

        // And any subsequent trigger under the already-watched root (the
        // trigger is what re-derives the listen set from the fresh
        // config)
        write_skill(&skills_root, "beta", "second");

        // Then the next reload serves BOTH the trigger skill and the
        // newly configured extra dir's skill
        assert!(
            wait_until(Duration::from_secs(10), || {
                runtime.scope.skills().get("beta").is_some()
                    && runtime.scope.skills().get("extra-one").is_some()
            })
            .await,
            "the post-config-write reload must serve the trigger skill AND the newly configured extra dir"
        );

        // And a LATER write inside the extra dir itself is observed
        // (it entered the listen set on the previous re-mount)
        // Settle past notify's stop/restart blind window on macOS: every
        // appended watch rebuilds the FSEventStream with SinceNow, so a
        // write racing the rebuild is dropped with no later signal (the
        // manual reload stays the fallback).
        tokio::time::sleep(Duration::from_millis(500)).await;
        write_skill(extra_root.path(), "extra-two", "the second extra skill");
        assert!(
            wait_until(Duration::from_secs(10), || {
                runtime.scope.skills().get("extra-two").is_some()
            })
            .await,
            "the extra dir must be watched after the config write (the dynamic listen-set refresh)"
        );
    }

    /// Given a workspace with a live watcher, when a file unrelated to
    /// skills (a `.tmp` scratch file at the workspace root, outside every
    /// watched skill root) is written, then no reload runs — the catalog
    /// is unchanged and the broadcast stays silent.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn should_not_trigger_when_an_unrelated_file_changes() {
        let workspace_root = tempfile::tempdir().expect("workspace tempdir");
        let home = tempfile::tempdir().expect("home tempdir");
        let isolated_home = tempfile::tempdir().expect("isolated $HOME tempdir");
        write_skill(
            &workspace_root.path().join(".agents/skills"),
            "alpha",
            "first",
        );

        let runtime = built_runtime(&workspace_root, &home, "ws-unrelated");

        // The pin is HELD for the whole test (the replay reads `$HOME` at
        // trigger time; a scoped pin would leak the real home's skills in
        // and make the unrelated write look like a catalog change).
        let (_pin, _isolated) = pinned_home_guard(isolated_home.path());

        let orchestrator = Arc::new(SkillReloadOrchestrator::new());
        let mut events = orchestrator.subscribe();
        let _watcher = spawn_skill_watcher(&runtime, &orchestrator, inputs(&home))
            .await.expect("watcher spawn");

        // When an unrelated file churns at the workspace root (not under
        // any watched skill root; the `.tmp` naming also matches the
        // skill-loader's own ignore rule)
        tokio::time::sleep(Duration::from_millis(300)).await;
        std::fs::write(workspace_root.path().join("notes.tmp.tmp"), "scratch")
            .expect("scratch write");
        std::fs::write(workspace_root.path().join("more-scratch.txt"), "scratch")
            .expect("scratch write 2");

        // Then no event is broadcast within the debounce-plus-margin
        // window (the catalog could not have changed)
        assert!(
            tokio::time::timeout(
                QUIET_PERIOD + Duration::from_secs(3),
                events.recv()
            )
            .await
            .is_err(),
            "an unrelated file write must not trigger a skill reload"
        );
        assert!(
            runtime.scope.skills().get("alpha").is_some()
                && runtime
                    .scope
                    .skills()
                    .snapshot()
                    .iter()
                    .all(|(name, _)| name != "notes.tmp.tmp".split('.').next().unwrap_or("")),
            "the catalog is untouched by the unrelated write"
        );
    }

    /// Given a candidate skill root that does not exist yet (the
    /// user-brand `<home>/skills` directory), when it is created and a
    /// skill is written into it, then the parent-fallback watch observes
    /// the creation, the watcher re-mounts onto the new root, and the
    /// reload serves the new skill.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn should_watch_through_the_parent_when_the_candidate_root_is_created_later() {
        let workspace_root = tempfile::tempdir().expect("workspace tempdir");
        let home = tempfile::tempdir().expect("home tempdir");
        let isolated_home = tempfile::tempdir().expect("isolated $HOME tempdir");
        // The project skills root exists with one seed; the user-brand
        // root (`<home>/skills`) deliberately does NOT exist at mount
        // time — the watcher must fall back to watching `home` itself.
        write_skill(
            &workspace_root.path().join(".agents/skills"),
            "alpha",
            "first",
        );
        let brand_root = home.path().join("skills");
        assert!(
            !brand_root.is_dir(),
            "the brand root must start missing for the fallback test"
        );

        let runtime = with_pinned_home(isolated_home.path(), || {
            built_runtime(&workspace_root, &home, "ws-parent")
        });

        // Re-pin for the whole live phase: the watcher task's replays
        // read `$HOME` at trigger time (the scoped build pin above already
        // restored the real home by now).
        let (_pin, _isolated) = pinned_home_guard(isolated_home.path());

        let orchestrator = Arc::new(SkillReloadOrchestrator::new());
        let _watcher = spawn_skill_watcher(&runtime, &orchestrator, inputs(&home))
            .await.expect("watcher spawn");

        // When the missing root is created and a skill lands in it (the
        // create fires under the watched parent; the post-reload remount
        // then owns the new root directly)
        write_skill(&brand_root, "brand-skill", "from the brand root");

        // Then the fallback observation triggered the reload and the
        // brand skill is served
        assert!(
            wait_until(Duration::from_secs(10), || {
                runtime.scope.skills().get("brand-skill").is_some()
            })
            .await,
            "a skill created under a parent-watched candidate root must still hot-reload"
        );
    }

    /// Given a workspace with a live watcher, when the watcher handle is
    /// dropped, then the task exits — a later skill write triggers no
    /// reload (the catalog stays stale by design: decision 8's
    /// degraded-honesty surface).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn should_stop_the_watcher_task_when_the_handle_drops() {
        let workspace_root = tempfile::tempdir().expect("workspace tempdir");
        let home = tempfile::tempdir().expect("home tempdir");
        let isolated_home = tempfile::tempdir().expect("isolated $HOME tempdir");
        let skills_root = workspace_root.path().join(".agents/skills");
        write_skill(&skills_root, "alpha", "first");

        let runtime = with_pinned_home(isolated_home.path(), || {
            built_runtime(&workspace_root, &home, "ws-drop")
        });

        // Re-pin for the whole live phase: the watcher's replays read
        // `$HOME` at trigger time.
        let (_pin, _isolated) = pinned_home_guard(isolated_home.path());

        let orchestrator = Arc::new(SkillReloadOrchestrator::new());
        let watcher = spawn_skill_watcher(&runtime, &orchestrator, inputs(&home))
            .await.expect("watcher spawn");

        // Sanity: the watcher is alive (a write reloads while held).
        write_skill(&skills_root, "beta", "second");
        assert!(
            wait_until(Duration::from_secs(10), || {
                runtime.scope.skills().get("beta").is_some()
            })
            .await,
            "while held, the watcher must reload"
        );

        // When the handle drops (the registry-remove path)
        drop(watcher);
        // Give the task time to observe the shutdown signal and exit.
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Then a later write does NOT reload — the task is gone, not
        // merely quiet (the whole point of the explicit shutdown channel:
        // a leaked task would keep reloading forever).
        write_skill(&skills_root, "gamma", "third");
        assert!(
            !wait_until(QUIET_PERIOD + Duration::from_secs(3), || {
                runtime.scope.skills().get("gamma").is_some()
            })
            .await,
            "after the handle drops, the watcher task must no longer reload"
        );
        assert!(
            runtime.scope.skills().get("gamma").is_none(),
            "the catalog stays stale after the watcher is dropped"
        );
    }

}

