//! Zero-config hot-reload file watchers.
//!
//! [`spawn_config_watchers`] watches the files that drive a running server's
//! runtime configuration and reconciles the corresponding in-memory
//! registries when they change on disk:
//!
//! - **MCP servers** (Fix 11A, re-lined by F-2/F-6): watches the three
//!   `mcp.json` layers (global `~/.mirri-code/mcp.json`, project-root
//!   `.mcp.json`, project-local `.mirri-code/mcp.json`). On change it
//!   invokes the caller's **sync entry** — the engine's single reconcile
//!   kernel (`mcp.sync`) — and nothing else: the watcher owns no diff of
//!   its own, so user-layer edits land on the user pool and workspace
//!   edits on the workspace pool exactly as a config write would (the
//!   historical in-watcher diff reconciled a single manager against the
//!   three-layer merge, which under the three-pool model duplicated every
//!   user server into the workspace pool — the F-2 log-storm root cause).
//! - **Agent profiles** (Fix 11B, re-lined by R2-3): watches the profile
//!   discovery roots (the user agents dirs plus the legacy `profiles/`) —
//!   a root that does not exist yet through its existing parent (the
//!   `watched_parent_dirs` pattern), so a later-created agents dir becomes
//!   hot-reloadable without a restart. On change it re-discovers the roots
//!   with skip-and-collect semantics and replaces the [`ProfileManager`]
//!   registrations with the current good set — per file, a profile that no
//!   longer parses keeps its last known good registration (an intentional
//!   divergence from the TS loader's re-discovery; see [`reload_profiles`]
//!   for the adjudication), so a broken save cannot unregister a working
//!   profile, while deleted files drop out and the built-ins always
//!   survive (see [`reload_profiles`]).
//! - **config.toml** (Fix 11C + G-12): watches `config.toml`. On change it
//!   re-parses the file and hot-reloads every reloadable section: the shared
//!   [`ServerConfig`] snapshot is swapped (providers/models, thinking,
//!   permission, loop control, yolo, plan mode — everything read through the
//!   `Arc<RwLock<ServerConfig>>`), and the [`HookEngine`] registrations are
//!   replaced with the new `[[hooks]]` section (clear + re-register, never
//!   append). `experimental` flags are deliberately **not** reloaded — they
//!   require a restart — and session-level configuration is untouched.
//!
//! Every watcher runs in a background `tokio` task for the lifetime of the
//! process. `notify`'s OS event callback is synchronous, so events are
//! bridged over an unbounded mpsc channel into the async task, where bursts
//! (e.g. the temp-file-write-and-rename of an atomic editor save) are
//! collapsed through a 200 ms debounce window before the reload runs.
//!
//! Watchers are best-effort by design: a path that cannot be watched, a
//! config that fails to parse, or a reload that errors is logged (warn) and
//! never takes the server down. A failed reload leaves the previous in-memory
//! state in place wherever the reload sequence permits it.
//!
//! # Session hook semantics
//!
//! Sessions snapshot the workspace hook engine at creation, so a hooks
//! reload applies to hooks inherited by *new* sessions; already-running
//! sessions keep the hooks they were created with until they are recreated.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use mirri_core::ServerConfig;
use mirri_core::config::ConfigChange;
use mirri_core::hook_engine::HookEngine;
use mirri_core::profile::{
    DiscoveredProfile, ProfileManager, ProfileSourceRoot, discover_profiles,
};
use mirri_mcp::config::{global_config_path, local_config_path, project_root_config_path};
use mirri_os::LocalSyncFs;
use notify::{Event, EventKind, RecursiveMode, Watcher};
use tokio::sync::mpsc::UnboundedReceiver;

/// Last-known-good discovered profile per discovery file path — the
/// hot-reload protection state (see [`reload_profiles`]): a file that
/// fails to parse on a later pass keeps the profile it last produced,
/// until it parses again or disappears from the scan. The whole
/// [`DiscoveredProfile`] is kept so the re-registration carries its
/// provenance (the R3-5 source tag + file path) through the reload.
type LastGoodProfiles = Arc<std::sync::Mutex<HashMap<PathBuf, DiscoveredProfile>>>;

/// Persistent across-reload fingerprint of the watched agent-profile roots:
/// (absolute path → last-modified) for every profile-eligible file the last
/// discovery pass saw, sorted. Two identical fingerprints mean the
/// discovery-visible on-disk set did not change since the last reload, so a
/// reload triggered by a spurious event can be skipped cheaply.
type ProfilesFingerprint = Arc<std::sync::Mutex<Option<BTreeMap<PathBuf, SystemTime>>>>;

/// A file-change burst must go quiet for this long before the pending reload
/// runs. Long enough to absorb the temp-write + rename of an atomic editor
/// save, short enough that hot reload still feels instant.
const DEBOUNCE_WINDOW: Duration = Duration::from_millis(200);

/// Delay before re-registering a watch that the OS dropped (e.g. because the
/// watched directory was deleted). Long enough for a directory being
/// recreated (deletion + atomic rename) to exist again.
const WATCH_RETRY_DELAY: Duration = Duration::from_secs(2);

// ---------------------------------------------------------------------------
// Public inputs
// ---------------------------------------------------------------------------

/// The engine's MCP reconcile entry the watcher invokes on a config-file
/// event (F-2/F-6): returns the (already started) reconcile future — in
/// the CLI host, a `mcp.sync` dispatch on the server state whose result is
/// only logged. Kept as a closure so the watcher holds no pool handle and
/// no second diff — the type enforces that a file event can do nothing
/// but run the single kernel.
pub type McpSyncClosure = Arc<dyn Fn() -> futures::future::BoxFuture<'static, ()> + Send + Sync>;

/// Inputs for the MCP config watcher (Fix 11A, re-lined by F-2/F-6; W7
/// multi-root: every registered workspace's layer files are watched).
pub struct McpHotReloadInputs {
    /// User home directory — resolves layer 1 (`~/.mirri-code/mcp.json`).
    pub home_dir: PathBuf,
    /// Every registered workspace root (W7) — each root contributes both
    /// of its project layer files (layer 2 `<root>/.mcp.json` and layer 3
    /// `<root>/.mirri-code/mcp.json`) to the watched set. The watcher
    /// owns no trust decision of its own: an untrusted root's file event
    /// dispatches the same `mcp.sync`, and the engine's reconcile kernel
    /// is the gate that keeps that root's pool empty.
    pub workspace_roots: Vec<PathBuf>,
    /// The engine reconcile entry a file event invokes (the `mcp.sync`
    /// kernel — the watcher applies no pool operations of its own).
    pub sync: McpSyncClosure,
}

/// Inputs for the profiles watcher (Fix 11B, re-lined by R2-3).
pub struct ProfilesHotReloadInputs {
    /// The profile discovery roots — the user agents dirs plus the legacy
    /// `profiles/` dir. Every root that exists is watched (recursively); a
    /// root that does not exist yet is watched through its existing parent
    /// with the event filter narrowed to the root's subtree (the TS
    /// `watched_parent_dirs` / `subtreeWatchFilter` pattern), so a newly
    /// created agents dir becomes hot-reloadable without a restart. A root
    /// whose whole parent chain is missing stays unwatched until the next
    /// explicit reload.
    pub roots: Vec<PathBuf>,
    /// The profile manager to clear and reload.
    pub manager: Arc<ProfileManager>,
}

/// Callback notified with the applied configuration after a successful
/// config.toml hot-reload (see [`ConfigHotReloadInputs::on_reload`]).
pub type ConfigReloadedCallback = Arc<dyn Fn(&ServerConfig) + Send + Sync>;

/// Inputs for the config.toml full-config watcher (Fix 11C + G-12).
pub struct ConfigHotReloadInputs {
    /// Path to `config.toml` (its reloadable sections are hot-reloaded).
    pub config_path: PathBuf,
    /// The hook engine whose registrations are replaced on change.
    pub engine: Arc<HookEngine>,
    /// The shared server configuration snapshot. On a successful reload its
    /// value is swapped so every reader (e.g. `config.get` wire shape, the
    /// embedded server's [`ConfigModelCatalog`], future domains) observes the
    /// new providers/models/thinking/permission/loop control/yolo/plan mode.
    /// `experimental` flags are preserved from the previous value — they are
    /// restart-only by design.
    ///
    /// The snapshot is a `std::sync::RwLock` (not a tokio lock) so the exact
    /// same `Arc` can back both this watcher and the embedded server's
    /// config (see `mirri_embedded::EmbeddedServer::with_providers`) — one
    /// source of truth shared by hot-reload and every config reader.
    pub config: Arc<RwLock<ServerConfig>>,
    /// Optional callback invoked after a successful reload with the new
    /// applied configuration (e.g. to propagate reloadable settings into live
    /// engine state or notify running sessions). Best-effort: it runs
    /// synchronously inside the watcher task; a callback that panics is
    /// contained by the task and never takes the server down. The callback
    /// receives an owned clone of the applied config — the watcher never
    /// invokes it while holding any lock, so a callback that takes the write
    /// lock itself cannot deadlock.
    pub on_reload: Option<ConfigReloadedCallback>,
}

/// All inputs for [`spawn_config_watchers`].
///
/// Each field enables exactly one watcher; `None` disables it (e.g. the MCP
/// watcher when no home directory can be resolved at startup).
pub struct HotReloadInputs {
    /// MCP server config watcher, or `None` to disable it.
    pub mcp: Option<McpHotReloadInputs>,
    /// Agent-profiles watcher, or `None` to disable it.
    pub profiles: Option<ProfilesHotReloadInputs>,
    /// config.toml full-config watcher, or `None` to disable it.
    pub config: Option<ConfigHotReloadInputs>,
}

/// Spawn the configured hot-reload watchers as detached background tasks.
///
/// Every watcher runs for the lifetime of the server. Failures (unwatchable
/// paths, watch-registration errors, reload errors) are logged and degrade
/// to "no hot reload" rather than aborting startup or crashing the server.
pub fn spawn_config_watchers(inputs: HotReloadInputs) {
    if let Some(mcp) = inputs.mcp {
        spawn_mcp_watcher(mcp);
    }
    if let Some(profiles) = inputs.profiles {
        spawn_profiles_watcher(profiles);
    }
    if let Some(config) = inputs.config {
        spawn_config_toml_watcher(config);
    }
}

// ---------------------------------------------------------------------------
// Watcher spawning
// ---------------------------------------------------------------------------

/// Spawn one background watcher task.
///
/// Watches `paths` (directories; recursively when `recursive` is set — the
/// profiles discovery walk is recursive, so nested agent files must trigger
/// the reload too; the event `filter` still discards irrelevant files, and
/// events for files the discovery walk itself ignores — e.g. a stray `.md`
/// inside a pruned directory — merely trigger an idempotent no-op reload).
/// The synchronous `notify` callback forwards only events accepted
/// by `filter` over an unbounded channel; the async task collapses bursts
/// with a debounce window and then runs `reload`. The watcher handle is
/// kept inside the task so watching continues until the process exits.
///
/// When `notify` reports an error (e.g. a watched directory was deleted and
/// the OS dropped the watch), the affected watch is re-registered after a
/// short delay. If the path still does not exist, the re-registration is
/// retried on the same cadence until it succeeds, so a recreated directory
/// does not permanently lose hot-reload.
fn spawn_watcher<F, R, Fut>(
    label: &'static str,
    paths: Vec<PathBuf>,
    recursive: bool,
    filter: F,
    reload: R,
) where
    F: Fn(&Event) -> bool + Send + 'static,
    R: Fn() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let mode = if recursive {
        RecursiveMode::Recursive
    } else {
        RecursiveMode::NonRecursive
    };
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<()>();
    // Errors travel on a separate channel so a path-level error is handled
    // immediately instead of being swallowed by the debounce window. The
    // payload is the affected path when the error carries one.
    let (error_sender, mut error_receiver) =
        tokio::sync::mpsc::unbounded_channel::<Option<PathBuf>>();
    let mut watcher = match notify::recommended_watcher(move |result: notify::Result<Event>| {
        match result {
            Ok(event) if filter(&event) => {
                // The receiver coalesces events; a full channel never blocks
                // the notify callback thread.
                let _ = sender.send(());
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(%error, "{label} watcher reported an event error");
                // Path-related errors populate `Error::paths`; the async
                // task uses the path (when known) to re-register the lost
                // watch.
                let _ = error_sender.send(error.paths.first().cloned());
            }
        }
    }) {
        Ok(watcher) => watcher,
        Err(error) => {
            tracing::warn!(
                %error,
                "cannot start {label} hot-reload watcher; {label} changes will not be hot-reloaded"
            );
            return;
        }
    };

    let mut watched_path_count = 0usize;
    for path in &paths {
        if let Err(error) = watcher.watch(path, mode) {
            tracing::warn!(
                path = %path.display(),
                %error,
                "{label} watcher cannot watch path; hot-reload for it is disabled"
            );
        } else {
            watched_path_count += 1;
        }
    }
    if watched_path_count == 0 {
        tracing::warn!("{label} hot-reload watcher has no watchable paths; disabled");
        return;
    }

    tracing::info!(?paths, "started {label} hot-reload watcher");
    tokio::spawn(async move {
        // Keep the notify watcher alive for the whole server lifetime.
        let mut watcher = watcher;
        let watched_paths = paths;
        // A watch the OS dropped is re-registered after `WATCH_RETRY_DELAY`;
        // the retry stays armed while any path remains unwatchable.
        let mut pending_watch_retry: Option<(tokio::time::Instant, Vec<PathBuf>)> = None;

        loop {
            let retry_deadline = pending_watch_retry.as_ref().map(|(deadline, _)| *deadline);
            tokio::select! {
                event = receiver.recv() => {
                    match event {
                        // Both senders live inside the notify callback
                        // closure, which the watcher owns for the whole
                        // task, so a closed channel ends the task.
                        None => break,
                        Some(()) => {
                            wait_for_quiet_period(&mut receiver, DEBOUNCE_WINDOW).await;
                            reload().await;
                        }
                    }
                }
                error = error_receiver.recv() => match error {
                    None => break,
                    Some(maybe_path) => {
                        tracing::warn!("{label} watcher lost a watch after an event error; re-registering");
                        let retry_paths = match maybe_path {
                            Some(error_path)
                                if watched_paths
                                    .iter()
                                    .any(|watched| same_file(watched, &error_path)) =>
                            {
                                vec![error_path]
                            }
                            // No path context (or an unrelated path): assume
                            // all watched paths may be affected.
                            _ => watched_paths.clone(),
                        };
                        pending_watch_retry = Some((
                            tokio::time::Instant::now() + WATCH_RETRY_DELAY,
                            retry_paths,
                        ));
                    }
                },
                _ = async {
                    tokio::time::sleep_until(retry_deadline.expect("guarded by retry_deadline.is_some()"))
                        .await
                }, if retry_deadline.is_some() => {
                    let (_, retry_paths) = pending_watch_retry.take().expect("pending retry");
                    let mut still_unwatchable = false;
                    for retry_path in &retry_paths {
                        match watcher.watch(retry_path, mode) {
                            Ok(()) => tracing::info!(
                                path = %retry_path.display(),
                                "re-registered {label} hot-reload watch"
                            ),
                            Err(error) => {
                                tracing::debug!(
                                    path = %retry_path.display(),
                                    %error,
                                    "cannot re-register {label} hot-reload watch yet; will keep trying"
                                );
                                still_unwatchable = true;
                            }
                        }
                    }
                    if still_unwatchable {
                        // The path still does not exist; keep trying on the
                        // same cadence until it comes back.
                        pending_watch_retry = Some((
                            tokio::time::Instant::now() + WATCH_RETRY_DELAY,
                            retry_paths,
                        ));
                    }
                }
            }
        }
    });
}

/// Spawn the MCP config watcher (Fix 11A, re-lined by F-2/F-6).
///
/// The three config-file parents are watched as directories (not the files
/// themselves), so a config file is observed even before it exists and
/// survives atomic-save renames. Missing `.mirri-code` parents are created —
/// they are product config directories and must exist for the config to be
/// loadable in the first place. A file event invokes the caller's sync
/// entry — the engine's `mcp.sync` reconcile kernel — which owns the
/// desired-state diff, the pool routing, and the conservative
/// keep-current-connections stance on a layer that fails to parse.
fn spawn_mcp_watcher(inputs: McpHotReloadInputs) {
    let mut watched_files = vec![global_config_path(&inputs.home_dir)];
    for root in &inputs.workspace_roots {
        watched_files.push(project_root_config_path(root));
        watched_files.push(local_config_path(root));
    }
    watched_files.sort();
    watched_files.dedup();
    let watched_dirs = watched_parent_dirs(&watched_files);
    for dir in &watched_dirs {
        if let Err(error) = std::fs::create_dir_all(dir) {
            tracing::warn!(
                dir = %dir.display(),
                %error,
                "cannot create MCP config directory; hot-reload for it is disabled"
            );
        }
    }

    let filter_files = watched_files.clone();
    let sync = inputs.sync;
    spawn_watcher(
        "MCP config",
        watched_dirs,
        false,
        move |event: &Event| is_relevant_file_event(event, &filter_files),
        move || {
            let sync = Arc::clone(&sync);
            async move {
                // The single reconcile kernel (F-6): no second diff here —
                // the summary-level result is logged by the entry itself.
                (sync)().await;
            }
        },
    );
}

/// Spawn the agent-profiles watcher (Fix 11B, re-lined by R2-3).
///
/// Existing roots are watched recursively (the discovery walk is recursive).
/// A candidate root that does not exist yet is watched through its **parent**
/// when the parent exists (the `watched_parent_dirs` pattern, mirroring the
/// TS `UserAgentProfileLoaderService.watchUserAgentRoots`): the parent is
/// watched recursively with the event filter narrowed to the configured
/// roots' subtrees (the TS `subtreeWatchFilter`), so a later creation of the
/// root — and of profile files inside it — still fires the reload instead of
/// needing a restart. A root whose whole parent chain is missing stays
/// unwatched, exactly like the TS generic `.agents` root (picked up by the
/// next explicit reload).
fn spawn_profiles_watcher(inputs: ProfilesHotReloadInputs) {
    let roots = inputs.roots;
    // The composition root (the funnel) guarantees the legacy profiles root
    // exists; the user agents roots are probed, not created — creating
    // `<mirri-home>/agents` or `~/.agents/agents` on every boot would change
    // the discovery semantics (an empty brand dir is still a brand root,
    // and the TS engine never creates them either).
    let (existing_roots, missing_roots): (Vec<PathBuf>, Vec<PathBuf>) =
        roots.iter().cloned().partition(|root| root.is_dir());
    // Missing roots: watch each existing parent (deduped — one parent may
    // front several candidate roots) so the roots' later creation is
    // observed.
    let mut watched: Vec<PathBuf> = existing_roots;
    watched.extend(
        watched_parent_dirs(&missing_roots)
            .into_iter()
            .filter(|parent| parent.is_dir()),
    );
    watched.sort();
    watched.dedup();
    if watched.is_empty() {
        tracing::warn!(
            ?roots,
            "no agent profile root exists; profile hot-reload is disabled"
        );
        return;
    }

    let manager = Arc::clone(&inputs.manager);
    let last_good: LastGoodProfiles = Arc::new(std::sync::Mutex::new(HashMap::new()));
    let fingerprint: ProfilesFingerprint = Arc::new(std::sync::Mutex::new(None));
    // Seed the last-known-good map with the current good pass — the
    // spawn-time snapshot of what the funnel just loaded, so the first
    // reload after a file breaks keeps that profile instead of losing it.
    seed_last_good(&roots, &last_good);
    // The filter is narrowed to the configured roots' subtrees: a parent
    // watch (for a not-yet-existing root) must not fire on unrelated
    // profile-extension files elsewhere under the parent, and a watch on
    // an existing root is already inside its own subtree, so the guard is
    // a no-op there. Each root guards under BOTH spellings — the
    // configured path and its canonical form — because event paths arrive
    // canonicalized on some platforms (`/tmp` → `/private/tmp`; see
    // [`same_file`]), and a missing root only has a canonical form through
    // [`canonicalize_through_ancestors`].
    let filter_roots: Vec<PathBuf> = roots
        .iter()
        .flat_map(|root| {
            let mut spellings = vec![root.clone()];
            if let Some(canonical) = canonicalize_through_ancestors(root)
                && canonical != *root
            {
                spellings.push(canonical);
            }
            spellings
        })
        .collect();
    spawn_watcher(
        "agent profiles",
        watched,
        // Recursive: the discovery walk is recursive (depth 8), so a
        // profile created or edited in a subdirectory of a root must fire
        // the reload too — and a parent watch must observe files inside a
        // root created after watch start (a non-recursive watch would only
        // see the root directory itself, which the profile-extension
        // filter drops).
        true,
        move |event: &Event| is_relevant_profile_root_event(event, &filter_roots),
        move || {
            let manager = Arc::clone(&manager);
            let roots = roots.clone();
            let last_good = Arc::clone(&last_good);
            let fingerprint = Arc::clone(&fingerprint);
            async move {
                let _changed =
                    reload_profiles(&manager, &roots, &last_good, Some(&fingerprint)).await;
            }
        },
    );
}

/// Spawn the config.toml full-config watcher (Fix 11C + G-12).
fn spawn_config_toml_watcher(inputs: ConfigHotReloadInputs) {
    let config_path = inputs.config_path;
    let Some(config_dir) = config_path.parent().map(|parent| parent.to_path_buf()) else {
        tracing::warn!(
            config_path = %config_path.display(),
            "config.toml has no parent directory; config hot-reload is disabled"
        );
        return;
    };
    if let Err(error) = std::fs::create_dir_all(&config_dir) {
        tracing::warn!(
            dir = %config_dir.display(),
            %error,
            "cannot create config directory; config hot-reload is disabled"
        );
        return;
    }

    let filter_file = config_path.clone();
    let reload_path = config_path.clone();
    let engine = Arc::clone(&inputs.engine);
    let config_ref = Arc::clone(&inputs.config);
    let on_reload = inputs.on_reload;
    spawn_watcher(
        "config.toml",
        vec![config_dir],
        false,
        move |event: &Event| is_relevant_file_event(event, std::slice::from_ref(&filter_file)),
        move || {
            let engine = Arc::clone(&engine);
            let config_ref = Arc::clone(&config_ref);
            let on_reload = on_reload.clone();
            let watched_config_path = reload_path.clone();
            async move {
                reload_full_config(
                    &watched_config_path,
                    &engine,
                    &config_ref,
                    on_reload.as_ref(),
                )
                .await
            }
        },
    );
}

// ---------------------------------------------------------------------------
// Event filtering
// ---------------------------------------------------------------------------

/// Deduplicated, sorted parent directories of `files` — the directories a
/// file watcher must watch to observe create/modify/remove of the file.
fn watched_parent_dirs(files: &[PathBuf]) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = files
        .iter()
        .filter_map(|file| file.parent().map(|parent| parent.to_path_buf()))
        .collect();
    dirs.sort();
    dirs.dedup();
    dirs
}

/// True for event kinds that represent an actual filesystem change.
///
/// `Remove` is included even for file watchers because editors commonly save
/// atomically — writing a temp file and renaming it over the target — which
/// surfaces to the watcher as `Remove` of the old inode followed by `Create`
/// of the new one.
fn is_mutating_kind(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    )
}

/// True when `event` is a create/modify/remove of one of `watched_files`.
///
/// The comparison is by full path, with a filename+parent fallback so minor
/// path-normalization differences between the watched path and the delivered
/// event path do not drop a reload.
fn is_relevant_file_event(event: &Event, watched_files: &[PathBuf]) -> bool {
    is_mutating_kind(&event.kind)
        && event
            .paths
            .iter()
            .any(|path| watched_files.iter().any(|watched| same_file(path, watched)))
}

/// True when both paths denote the same file.
///
/// The comparison is lexical first, then falls back to canonical paths:
/// watcher-delivered event paths can differ from the watched path purely
/// through filesystem aliasing (e.g. `/tmp` → `/private/tmp` on macOS), which
/// would otherwise drop reloads for paths under a symlinked prefix.
fn same_file(a: &Path, b: &Path) -> bool {
    if a == b || (a.file_name() == b.file_name() && a.parent() == b.parent()) {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(canonical_a), Ok(canonical_b)) => canonical_a == canonical_b,
        _ => false,
    }
}

/// Canonicalize `dir`, resolving a not-yet-existing path through its
/// deepest existing ancestor (the missing tail is re-appended unchanged).
///
/// The profiles watcher guards its event filter by "path under a configured
/// root", but a root that does not exist yet cannot be canonicalized — and
/// event paths arrive canonicalized on some platforms (see [`same_file`]).
/// Resolving through the ancestor keeps the guard correct for both
/// spellings of a future root.
fn canonicalize_through_ancestors(dir: &Path) -> Option<PathBuf> {
    if let Ok(canonical) = std::fs::canonicalize(dir) {
        return Some(canonical);
    }
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    for ancestor in dir.ancestors().skip(1) {
        match std::fs::canonicalize(ancestor) {
            Ok(canonical) => {
                let mut path = canonical;
                for component in tail.iter().rev() {
                    path.push(component);
                }
                return Some(path);
            }
            Err(_) => {
                if let Some(name) = ancestor.file_name() {
                    tail.push(name.to_os_string());
                }
            }
        }
    }
    None
}

/// True when `path` looks like a loadable agent profile file (`*.md`,
/// `*.toml`, `*.json`, case-insensitive) — the extension half of the
/// profiles event filter. `.yaml`/`.yml` are deliberately excluded: they
/// never load, so a change to one cannot alter the registry.
fn is_profile_file_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            extension.eq_ignore_ascii_case("md")
                || extension.eq_ignore_ascii_case("toml")
                || extension.eq_ignore_ascii_case("json")
        })
}

/// True when `event` is a create/modify/remove of a profile file under one
/// of the discovery `roots` — the extension filter narrowed by the TS
/// `subtreeWatchFilter` pattern.
///
/// The root guard matters for parent watches (a not-yet-existing root
/// watched through its existing parent, so its later creation is observed):
/// without it, every unrelated profile-extension file anywhere under the
/// parent would fire a reload. For a watch on an existing root the guard is
/// a no-op — the watch is already inside that root's subtree.
fn is_relevant_profile_root_event(event: &Event, roots: &[PathBuf]) -> bool {
    is_mutating_kind(&event.kind)
        && event.paths.iter().any(|path| {
            is_profile_file_path(path) && roots.iter().any(|root| path.starts_with(root))
        })
}

// ---------------------------------------------------------------------------
// Reload orchestration
// ---------------------------------------------------------------------------

/// Seed a [`LastGoodProfiles`] map with the current good discovery pass
/// (skipped files contribute nothing — they have no good registration yet).
///
/// Called at watcher spawn to snapshot what the boot scan loaded; the reload
/// path maintains the map from there on.
fn seed_last_good(roots: &[PathBuf], last_good: &LastGoodProfiles) {
    // Every root the CLI watches is user-level today (the boot chain: the
    // user agents dirs plus the legacy profiles dir) — tag them all
    // [`ProfileSource::User`] (R3-5).
    let tagged: Vec<ProfileSourceRoot> = roots.iter().cloned().map(Into::into).collect();
    match discover_profiles(&tagged, &LocalSyncFs) {
        Ok(discovery) => {
            let mut keep = last_good
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            for discovered in discovery.profiles {
                keep.insert(discovered.path.clone(), discovered);
            }
        }
        Err(error) => {
            tracing::warn!(
                ?roots,
                %error,
                "agent profile watcher cannot seed the last-known-good set"
            );
        }
    }
}

/// Reload all agent profiles from the discovery roots (R2-3 contract).
///
/// File create/modify/delete events collapse into a single reload. The
/// reload is **incremental** (the pre-R2-3 all-or-nothing staging is
/// gone): the roots are re-discovered with skip-and-collect semantics and
/// the registrations are replaced with the current good set — with one
/// protection carried over, per file rather than per directory: a file
/// that used to be good but now fails to parse (or whose legacy `extends`
/// fails) keeps its **last known good registration**, so a broken save
/// cannot unregister a working profile. Files that vanished from the
/// scan entirely (deleted or renamed away) drop out. The built-ins are
/// always re-registered by [`ProfileManager::clear`]. A file declaring
/// the essential name loads like any other (D16: it overrides the
/// built-in content by priority, so the override survives the reload).
///
/// The reload refreshes the R3-5 merge projection too: every re-registered
/// profile carries its discovery provenance (source tag + file path,
/// [`ProfileManager::register_discovered`]) and the same-name losers land
/// on the manager's suppressed-candidates projection
/// ([`ProfileManager::set_suppressed_candidates`]) — mirroring what
/// [`ProfileManager::load_from_roots`] does at boot, so a hot reload
/// never serves stale `source` / `file_path` / `suppressed_candidates`
/// on the wire.
///
/// A discovery-level failure (a root that exists but cannot be listed)
/// keeps the previous registrations unchanged, as before.
///
/// **Intentional divergence from the TS engine** (adjudicated R3-7): the TS
/// `UserAgentProfileLoaderService` recomputes its contribution from a fresh
/// discovery on every debounced watch event, so a file that stops parsing
/// drops out of the registry until it parses again; this watcher keeps the
/// last-known-good registration instead. Aligning either direction is not
/// cheap — dropping would undo the R2-3 hot-reload safety contract (and its
/// acceptance tests) and unregister working profiles on a broken save,
/// while keeping on the TS side would need per-file memo state inside the
/// workspace-scoped loader service. Each side fits its host: the TS loader
/// is a declarative contribution recompute, this watcher guards an
/// imperative registry that must keep serving mid-session.
/// The maximum directory depth the profile watcher walks below a root when
/// computing a discovery fingerprint — mirroring [`MAX_PROFILE_SCAN_DEPTH`]
/// in mirri-core so the fingerprint covers exactly the files the discovery
/// could load.
const FINGERPRINT_SCAN_DEPTH: usize = 8;

/// Walk `dir` (recursively, to [`FINGERPRINT_SCAN_DEPTH`]) and record the
/// last-modified time of every existing profile-eligible file into
/// `fingerprint`. Mindful of the walk in [`discover_profiles`]: the pruning
/// rules (dot-entries and `node_modules`) mirror the loader so the
/// fingerprint and the discovery agree on which files are in scope.
fn fingerprint_root(dir: &Path, depth: usize, fingerprint: &mut BTreeMap<PathBuf, SystemTime>) {
    if depth > FINGERPRINT_SCAN_DEPTH {
        return;
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        // Mirrors the discovery walk's pruning: dot-entries and
        // node_modules are skipped (TS walk semantics).
        if name.starts_with('.') || name == "node_modules" {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if meta.is_dir() {
            fingerprint_root(&path, depth + 1, fingerprint);
        } else if meta.is_file()
            && is_profile_file_path(&path)
            && let Ok(modified) = meta.modified()
        {
            fingerprint.insert(path, modified);
        }
    }
}

/// Fingerprint the currently discovery-visible set under `roots`. `None`
/// when the scan cannot be completed (the caller then proceeds — a scan
/// failure must not suppress a legitimate reload).
fn fingerprint_profiles(roots: &[PathBuf]) -> Option<BTreeMap<PathBuf, SystemTime>> {
    let mut fingerprint = BTreeMap::new();
    for root in roots {
        if root.is_dir() {
            fingerprint_root(root, 0, &mut fingerprint);
        }
    }
    Some(fingerprint)
}

/// True when the on-disk profile set under `roots` differs from the
/// previously recorded `previous` fingerprint. A scan failure (or no
/// previous fingerprint) reports a difference so the caller never skips a
/// legitimate reload on the first pass or while reading is unreliable.
fn fingerprint_changed(
    roots: &[PathBuf],
    previous: Option<&BTreeMap<PathBuf, SystemTime>>,
) -> bool {
    let Some(previous) = previous else {
        return true;
    };
    let Some(current) = fingerprint_profiles(roots) else {
        return true;
    };
    current != *previous
}

async fn reload_profiles(
    manager: &ProfileManager,
    roots: &[PathBuf],
    last_good: &LastGoodProfiles,
    fingerprint: Option<&ProfilesFingerprint>,
) -> bool {
    // Belt-and-braces spurious-trigger guard: when the discovery-visible
    // set is byte-identical (same paths, same mtimes) to the last reload,
    // skip the expensive clear + re-register entirely. This makes the no-op
    // suppression robust to watcher-backend quirks (e.g. notify's FSEvents
    // reporting out-of-root events during a stream rebuild) that slip past
    // the event-filter prefix guard. A fingerprint scan failure or a first
    // pass (no prior fingerprint) proceeds as usual.
    if let Some(fingerprint) = fingerprint {
        let previous = fingerprint
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !fingerprint_changed(roots, previous.as_ref()) {
            tracing::debug!(
                ?roots,
                "agent profiles hot-reload skipped: on-disk set unchanged"
            );
            return false;
        }
    }

    // A scan-level failure keeps everything: the registry must not be
    // wiped by an unreadable root.
    // Every watched root is user-level today (see [`seed_last_good`]).
    let tagged: Vec<ProfileSourceRoot> = roots.iter().cloned().map(Into::into).collect();
    let discovery = match discover_profiles(&tagged, &LocalSyncFs) {
        Ok(discovery) => discovery,
        Err(error) => {
            tracing::warn!(
                ?roots,
                %error,
                "agent profile hot-reload failed to scan the roots; keeping current profiles"
            );
            return false;
        }
    };

    // The files seen this pass, good or skipped — anything else was
    // deleted and must drop out of the last-known-good set.
    let good_paths: HashSet<&Path> = discovery
        .profiles
        .iter()
        .map(|d| d.path.as_path())
        .collect();
    let skipped_paths: HashSet<&Path> =
        discovery.skipped.iter().map(|s| s.path.as_path()).collect();

    let mut keep = last_good
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    keep.retain(|path, _| {
        good_paths.contains(path.as_path()) || skipped_paths.contains(path.as_path())
    });
    // Apply the current good set: each fresh winner replaces its own
    // per-path entry and evicts any last-known-good entry of the same
    // profile name (a higher-priority file — e.g. a new same-name `.md` —
    // must outrank the stale one).
    for discovered in &discovery.profiles {
        keep.retain(|_, entry| entry.profile.name != discovered.profile.name);
        keep.insert(discovered.path.clone(), discovered.clone());
    }

    // Evaluate the reload against the current registration state so a
    // spurious trigger that changes nothing can log at debug instead of
    // INFO.
    let before = manager.list_all();

    // Replace the registry with built-ins + the kept set, deterministically.
    let mut registrations: Vec<DiscoveredProfile> = keep.values().cloned().collect();
    registrations.sort_by(|a, b| a.profile.name.cmp(&b.profile.name));
    let mut loaded = 0;
    manager.clear();
    for discovered in &registrations {
        // The error arm is the engine-level backstop (the registration
        // channels accept every discovered name today, the essential
        // included — D16); a stale entry must still never fail the reload.
        if let Err(err) = manager.register_discovered(discovered) {
            tracing::warn!(%err, "agent profile rejected during hot-reload; keeping the built-in");
            continue;
        }
        loaded += 1;
    }
    manager.set_suppressed_candidates(discovery.suppressed);
    for skipped in &discovery.skipped {
        tracing::warn!(
            path = %skipped.path.display(),
            reason = %skipped.reason,
            "agent profile file skipped during hot-reload; \
             keeping its last known good registration"
        );
    }

    // The fingerprint is refreshed after every completed pass (good or
    // degrading) so a later no-change pass recognizes the current state.
    if let Some(fingerprint) = fingerprint
        && let Some(current) = fingerprint_profiles(roots)
    {
        *fingerprint
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(current);
    }

    // The reload changed the effective registration set only when the set
    // actually differs from what was registered before the pass.
    let after = manager.list_all();
    let changed = before != after || !discovery.skipped.is_empty();
    if changed {
        tracing::info!(?roots, loaded, "agent profiles hot-reloaded");
    } else {
        tracing::debug!(?roots, loaded, "agent profiles hot-reloaded (no-op)");
    }
    changed
}

/// Re-parse `config.toml` and hot-reload every reloadable section (G-12).
///
/// A successful reload:
/// 1. Swaps the shared [`ServerConfig`] snapshot under `config_ref` so every
///    reader observes the new providers/models/thinking/permission/loop
///    control/yolo/plan mode values. The previous `experimental` flags are
///    preserved — experimental features are restart-only by design.
/// 2. Replaces the hook engine's registrations with the new `[[hooks]]`
///    section (clear + re-register, never append), but only when the hooks
///    section actually changed.
/// 3. Invokes `on_reload` (when present) with the applied config, so the
///    host can propagate reloadable settings to running sessions.
///
/// Best-effort by design: a parse failure is logged and the current
/// configuration (and hook registrations) are kept; an edit that changes no
/// reloadable section is a no-op. An edit that touches only `experimental` is
/// also a no-op — those flags require a restart. Session-level configuration
/// is not part of `ServerConfig` and is never touched here.
async fn reload_full_config(
    config_path: &Path,
    engine: &HookEngine,
    config_ref: &RwLock<ServerConfig>,
    on_reload: Option<&ConfigReloadedCallback>,
) {
    let new_config = match ServerConfig::load(config_path, &LocalSyncFs) {
        Ok(config) => config,
        Err(error) => {
            tracing::warn!(
                config_path = %config_path.display(),
                %error,
                "config.toml hot-reload failed to parse; keeping current configuration"
            );
            return;
        }
    };

    let previous = config_ref.read().unwrap_or_else(|e| e.into_inner()).clone();
    let changes = new_config.reload_changes(&previous);
    if changes.is_empty() {
        tracing::debug!(
            config_path = %config_path.display(),
            "config.toml changed but no reloadable section differs; keeping current configuration"
        );
        return;
    }

    // Experimental flags are restart-only: preserve them from the previous
    // snapshot instead of taking the (possibly changed) on-disk value.
    let mut applied = new_config.clone();
    applied.experimental = previous.experimental.clone();

    // Swap the shared snapshot first so every reader sees the new config even
    // if a downstream step fails. The snapshot is a plain `std::sync::RwLock`
    // shared with the embedded server's config readers (e.g.
    // `ConfigModelCatalog`), so a reload here propagates to them by
    // construction — one source of truth, no callback round-trip.
    *config_ref.write().unwrap_or_else(|e| e.into_inner()) = applied;

    // Hooks are the one section with live engine state: replace the
    // registrations (clear + re-register), never append. The diff gate above
    // guarantees the hooks section actually changed when we get here.
    if changes.contains(&ConfigChange::Hooks) {
        let new_hooks = new_config.hooks.unwrap_or_default();
        engine.clear();
        engine.register_all(new_hooks.clone());
        tracing::info!(
            config_path = %config_path.display(),
            hook_count = new_hooks.len(),
            "config.toml hooks hot-reloaded (clear + re-register)"
        );
    }

    // Notify the host (e.g. to propagate reloadable settings into running
    // sessions). The callback receives an owned clone captured before any
    // reference to the lock is dropped — never a borrowed guard — so a
    // callback that acquires the write lock itself cannot deadlock.
    // Best-effort: the callback runs inside this task, so a panicking
    // callback is contained and logged by the task runtime.
    if let Some(callback) = on_reload {
        let applied = config_ref.read().unwrap_or_else(|e| e.into_inner()).clone();
        callback(&applied);
    }

    tracing::info!(
        config_path = %config_path.display(),
        changes = ?changes,
        experimental_preserved = true,
        "config.toml hot-reloaded; reloadable sections applied, experimental flags and \
         session-level config keep their previous values"
    );
}

// ---------------------------------------------------------------------------
// Debounce
// ---------------------------------------------------------------------------

/// Wait until `receiver` stays empty for the full `quiet_for` window.
///
/// Each event that arrives inside the window restarts it, so a burst of
/// notifications (e.g. save temp + rename) collapses into a single reload.
async fn wait_for_quiet_period(receiver: &mut UnboundedReceiver<()>, quiet_for: Duration) {
    loop {
        match tokio::time::timeout(quiet_for, receiver.recv()).await {
            // Another event arrived inside the window — start it over.
            Ok(Some(())) => continue,
            // The channel closed — nothing further will arrive.
            Ok(None) => return,
            // The window elapsed quietly — the burst is over.
            Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use futures::FutureExt;
    use mirri_core::ModelCatalog;
    use mirri_core::hook_engine::{HookEngine, HookEventType};
    use mirri_core::profile::{
        ProfileManager, ProfileProvenance, ProfileSource, SuppressedProfileCandidate,
        SuppressedReason,
    };
    use notify::event::{CreateKind, DataChange, ModifyKind, RemoveKind};
    use notify::{Event, EventKind};

    use super::*;

    /// The names of the built-in agent profiles registered on every
    /// [`ProfileManager`] (G-25).
    fn builtin_profile_names() -> Vec<String> {
        vec!["agent", "coder", "explore", "plan"]
            .into_iter()
            .map(String::from)
            .collect()
    }

    /// Write a project-local (layer 3) `mcp.json` listing the given servers,
    /// each either `enabled` or `disabled`, with a stdio `command`. Disabled
    /// servers connect without spawning a real process, keeping the test
    /// hermetic.
    fn write_local_mcp_config(cwd: &Path, servers: &[(&str, bool, &str)]) {
        let config_dir = cwd.join(".mirri-code");
        fs::create_dir_all(&config_dir).expect("create .mirri-code dir");
        let server_entries: Vec<String> = servers
            .iter()
            .map(|(name, enabled, command)| {
                format!(
                    r#""{name}": {{"transport": {{"type": "stdio", "command": "{command}", "args": []}}, "enabled": {enabled}}}"#
                )
            })
            .collect();
        let body = format!(r#"{{"mcpServers": {{{}}}}}"#, server_entries.join(","));
        fs::write(config_dir.join("mcp.json"), body).expect("write mcp.json");
    }

    /// Write `body` as an initial `config.toml` and create a fresh watcher
    /// environment seeded the way startup seeds it: the shared config
    /// snapshot holds the parsed file (experimental flags included) and the
    /// hook engine holds the file's hooks. Callers then run
    /// [`reload_full_config`] (with `None` for `on_reload` unless the test
    /// exercises the callback) to apply later on-disk changes.
    fn config_reload_env(
        config_path: &Path,
        body: &str,
    ) -> (Arc<HookEngine>, Arc<RwLock<ServerConfig>>) {
        fs::write(config_path, body).expect("write config");
        let initial = ServerConfig::from_toml(body).expect("initial config should parse");
        let engine = Arc::new(HookEngine::new());
        engine.register_all(initial.hooks.clone().unwrap_or_default());
        (engine, Arc::new(RwLock::new(initial)))
    }

    /// Given the MCP config watcher over a counting stand-in for the
    /// engine's `mcp.sync` entry, when a config file changes on disk,
    /// then the entry is invoked (the debounced file event becomes
    /// exactly one engine reconcile call) — and the watcher holds no pool
    /// handle, so there is no second diff and no direct manager operation
    /// to get wrong (the F-2 re-line: the desired-state semantics live in
    /// the engine kernel and are asserted in the mirri-embedded tests).
    #[tokio::test]
    async fn should_invoke_the_sync_entry_when_an_mcp_config_file_changes() {
        // Given a workspace with a local mcp.json and the watcher running
        // with a sync closure that only counts its calls
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let home_dir = temp_dir.path().join("home");
        fs::create_dir_all(home_dir.join(".mirri-code")).expect("create home config dir");
        let project_root = temp_dir.path().join("project");
        fs::create_dir_all(&project_root).expect("create project dir");
        let cwd = temp_dir.path().join("cwd");
        write_local_mcp_config(&cwd, &[("alpha", false, "echo")]);

        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_for_sync = Arc::clone(&calls);
        let sync: McpSyncClosure = Arc::new(move || {
            calls_for_sync.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            futures::future::ready(()).boxed()
        });
        spawn_mcp_watcher(McpHotReloadInputs {
            home_dir,
            workspace_roots: vec![project_root, cwd.clone()],
            sync,
        });

        // When the config file changes on disk
        write_local_mcp_config(&cwd, &[("beta", false, "echo")]);

        // Then the engine sync entry was invoked once
        wait_for_sync_calls(&calls, 1).await;

        // And a later change invokes it again — watching continues for the
        // process lifetime
        tokio::time::sleep(DEBOUNCE_WINDOW + Duration::from_millis(200)).await;
        write_local_mcp_config(&cwd, &[("gamma", false, "echo")]);
        wait_for_sync_calls(&calls, 2).await;
    }

    /// Poll the sync-call counter until it reaches `expected` (panics after
    /// 5s — the debounce window is 200ms, so the entry should land far
    /// inside the budget).
    async fn wait_for_sync_calls(calls: &Arc<std::sync::atomic::AtomicUsize>, expected: usize) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if calls.load(std::sync::atomic::Ordering::SeqCst) >= expected {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("the engine sync entry was never invoked ({expected} calls expected)");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Given a configured profile root that does not exist when the watcher
    /// starts, When the root is created (with a profile file inside it)
    /// after the watch is armed, Then the hot reload registers the profile
    /// without a restart — the TS `watched_parent_dirs` coverage, where the
    /// existing parent of a not-yet-existing root is watched with the
    /// filter narrowed to the root's subtree.
    #[tokio::test]
    async fn should_hot_reload_when_a_root_is_created_after_the_watch_starts() {
        // Given a home whose brand `agents/` root does not exist yet
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let home = temp_dir.path().join("home");
        fs::create_dir_all(&home).expect("create home dir");
        let agents_dir = home.join("agents");
        let manager = Arc::new(ProfileManager::new());
        spawn_profiles_watcher(ProfilesHotReloadInputs {
            roots: vec![agents_dir.clone()],
            manager: Arc::clone(&manager),
        });

        // When the root is created after the watch started, carrying a
        // profile file
        fs::create_dir_all(&agents_dir).expect("create agents dir late");
        fs::write(
            agents_dir.join("late.md"),
            "---\nname: late\ndescription: Arrives after the watch.\n---\nYou arrive late.",
        )
        .expect("write late.md");

        // Then the profile hot-loads without a restart (the parent watch
        // observed the file creation inside the new root)
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if manager.get("late").is_some() {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("a profile in a root created after watch start never hot-loaded");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The shared reload harness: a manager, a single-root last-good state,
    /// and a fresh reload over the roots.
    struct ProfilesReloadHarness {
        manager: Arc<ProfileManager>,
        last_good: LastGoodProfiles,
        fingerprint: ProfilesFingerprint,
        roots: Vec<PathBuf>,
    }

    impl ProfilesReloadHarness {
        fn new(profiles_dir: &Path) -> Self {
            let harness = Self {
                manager: Arc::new(ProfileManager::new()),
                last_good: Arc::new(std::sync::Mutex::new(HashMap::new())),
                fingerprint: Arc::new(std::sync::Mutex::new(None)),
                roots: vec![profiles_dir.to_path_buf()],
            };
            // Mirror the spawn path: the watcher seeds its last-known-good
            // map with the boot scan before arming the watches.
            seed_last_good(&harness.roots, &harness.last_good);
            harness
        }

        async fn reload(&self) -> bool {
            reload_profiles(
                &self.manager,
                &self.roots,
                &self.last_good,
                Some(&self.fingerprint),
            )
            .await
        }
    }

    /// should report no change when a reload finds the same profiles (the
    /// no-op that lets the watcher log at debug instead of INFO)
    #[tokio::test]
    async fn should_report_no_change_when_reload_sees_identical_profiles() {
        // Given a manager already in sync with a directory holding one profile
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let profiles_dir = temp_dir.path().join("profiles");
        fs::create_dir_all(&profiles_dir).expect("create profiles dir");
        fs::write(
            profiles_dir.join("coder.toml"),
            "name = \"coder\"\nmodel = \"claude-sonnet-4-20250514\"\n",
        )
        .expect("write coder profile");

        let harness = ProfilesReloadHarness::new(&profiles_dir);
        harness
            .manager
            .load_from_dir(&profiles_dir, &LocalSyncFs)
            .expect("initial load");

        // When a reload runs (whether a spurious trigger or a genuine
        // duplicate dispatch) with no on-disk change
        let first = harness.reload().await;
        let second = harness.reload().await;

        // Then the reload is a no-op that reports no change each time
        assert!(!first, "a reload in sync with disk should report no change");
        assert!(!second, "a repeated reload should stay a no-op");
    }

    /// should report a change when the on-disk set diverges across reloads
    #[tokio::test]
    async fn should_report_change_when_a_profile_is_added_between_reloads() {
        // Given a manager loaded from a directory with one profile
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let profiles_dir = temp_dir.path().join("profiles");
        fs::create_dir_all(&profiles_dir).expect("create profiles dir");
        fs::write(
            profiles_dir.join("coder.toml"),
            "name = \"coder\"\nmodel = \"claude-sonnet-4-20250514\"\n",
        )
        .expect("write coder profile");

        let harness = ProfilesReloadHarness::new(&profiles_dir);
        harness
            .manager
            .load_from_dir(&profiles_dir, &LocalSyncFs)
            .expect("initial load");
        harness.reload().await;

        // When a new profile file is added between reloads
        fs::write(
            profiles_dir.join("scout.toml"),
            "name = \"scout\"\nmodel = \"claude-haiku-4-20250514\"\n",
        )
        .expect("write scout profile");

        // Then the reload reports a change
        assert!(
            harness.reload().await,
            "a new profile should report a change"
        );
    }

    /// should skip the full clear + re-register when the on-disk set is
    /// byte-identical to the last reload (the belt-and-braces fingerprint
    /// guard that absorbs watcher-backend spurious triggers)
    #[tokio::test]
    async fn should_skip_reload_when_fingerprint_is_unchanged() {
        // Given a manager in sync with a directory holding one profile
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let profiles_dir = temp_dir.path().join("profiles");
        fs::create_dir_all(&profiles_dir).expect("create profiles dir");
        fs::write(
            profiles_dir.join("coder.toml"),
            "name = \"coder\"\nmodel = \"claude-sonnet-4-20250514\"\n",
        )
        .expect("write coder profile");

        let harness = ProfilesReloadHarness::new(&profiles_dir);
        harness
            .manager
            .load_from_dir(&profiles_dir, &LocalSyncFs)
            .expect("initial load");

        // When the first reload establishes the fingerprint and a second
        // fires with no on-disk change
        harness.reload().await;
        let second = harness.reload().await;

        // Then the second reload is skipped (reports no change) — the
        // registry had a nonzero, identical fingerprint to compare against
        assert!(!second, "an unchanged fingerprint should skip the reload");
    }

    /// should report an untouched root set as unchanged (the guard that lets a
    /// spurious event skip the expensive full reload)
    #[test]
    fn should_report_no_change_when_fingerprint_matches() {
        // Given a directory with one profile and a fingerprint snapshot of it
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let profiles_dir = temp_dir.path().join("profiles");
        fs::create_dir_all(&profiles_dir).expect("create profiles dir");
        fs::write(
            profiles_dir.join("coder.toml"),
            "name = \"coder\"\nmodel = \"claude-sonnet-4-20250514\"\n",
        )
        .expect("write coder profile");
        let previous =
            fingerprint_profiles(std::slice::from_ref(&profiles_dir)).expect("fingerprint");

        // When asked whether the same set changed
        let changed = fingerprint_changed(std::slice::from_ref(&profiles_dir), Some(&previous));

        // Then it reports no change
        assert!(!changed, "an identical fingerprint should report no change");
    }

    /// should report a change when the on-disk set diverges from the
    /// recorded fingerprint (a real mutation reaches the reload)
    #[test]
    fn should_report_change_when_fingerprint_diverges() {
        // Given a directory with one profile and a fingerprint of it
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let profiles_dir = temp_dir.path().join("profiles");
        fs::create_dir_all(&profiles_dir).expect("create profiles dir");
        let profile_path = profiles_dir.join("coder.toml");
        fs::write(
            &profile_path,
            "name = \"coder\"\nmodel = \"claude-sonnet-4-20250514\"\n",
        )
        .expect("write coder profile");
        let previous =
            fingerprint_profiles(std::slice::from_ref(&profiles_dir)).expect("fingerprint");

        // When a profile file is added after the snapshot
        fs::write(
            profiles_dir.join("scout.toml"),
            "name = \"scout\"\nmodel = \"claude-haiku-4-20250514\"\n",
        )
        .expect("write scout profile");

        // Then the set reports a change
        let changed = fingerprint_changed(std::slice::from_ref(&profiles_dir), Some(&previous));
        assert!(changed, "an added profile should report a change");
    }

    /// should report a change when there is no previous fingerprint (the
    /// first pass or a scan failure never suppresses a reload)
    #[test]
    fn should_report_change_when_no_previous_fingerprint() {
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let profiles_dir = temp_dir.path().join("profiles");

        let changed = fingerprint_changed(std::slice::from_ref(&profiles_dir), None);

        assert!(
            changed,
            "a missing previous fingerprint should report a change"
        );
    }

    /// should refresh the R3-5 provenance and suppressed-candidates
    /// projection across a hot reload (the watcher-side counterpart of the
    /// boot load's bookkeeping)
    #[tokio::test]
    async fn should_refresh_provenance_and_suppressed_across_a_hot_reload() {
        // Given the user chain (brand agents root over the legacy profiles
        // dir) with a same-name collision, seeded like the spawn path
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let agents_dir = temp_dir.path().join("agents");
        let profiles_dir = temp_dir.path().join("profiles");
        fs::create_dir_all(&agents_dir).expect("create agents dir");
        fs::create_dir_all(&profiles_dir).expect("create profiles dir");
        fs::write(
            agents_dir.join("reviewer.md"),
            "---\nname: reviewer\ndescription: Reviews.\n---\nFrom the agents root.",
        )
        .expect("write reviewer .md");
        fs::write(
            profiles_dir.join("reviewer.toml"),
            "name = \"reviewer\"\nmodel = \"from-profiles\"\n",
        )
        .expect("write reviewer .toml");

        let manager = Arc::new(ProfileManager::new());
        let roots = vec![agents_dir.clone(), profiles_dir.clone()];
        let last_good: LastGoodProfiles = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let fingerprint: ProfilesFingerprint = Arc::new(std::sync::Mutex::new(None));
        seed_last_good(&roots, &last_good);

        // When a change dispatches a reload
        reload_profiles(&manager, &roots, &last_good, Some(&fingerprint)).await;

        // Then the winner carries its discovery provenance and the
        // cross-root loser's projection refreshed with the pass
        assert_eq!(
            manager.provenance("reviewer"),
            Some(ProfileProvenance {
                source: ProfileSource::User,
                file_path: Some(agents_dir.join("reviewer.md")),
            })
        );
        assert_eq!(
            manager.suppressed_candidates("reviewer"),
            vec![SuppressedProfileCandidate {
                source: ProfileSource::User,
                reason: SuppressedReason::Priority,
            }]
        );

        // And when the losing file disappears, the next reload drops the
        // projection entry while the winner's provenance survives
        fs::remove_file(profiles_dir.join("reviewer.toml")).expect("remove the loser");
        reload_profiles(&manager, &roots, &last_good, Some(&fingerprint)).await;
        assert!(manager.suppressed_candidates("reviewer").is_empty());
        assert_eq!(
            manager.provenance("reviewer"),
            Some(ProfileProvenance {
                source: ProfileSource::User,
                file_path: Some(agents_dir.join("reviewer.md")),
            })
        );
    }

    #[tokio::test]
    async fn should_clear_and_reload_profiles_when_profiles_directory_changes() {
        // Given a profiles directory with one profile and a manager loaded
        // from it
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let profiles_dir = temp_dir.path().join("profiles");
        fs::create_dir_all(&profiles_dir).expect("create profiles dir");
        fs::write(
            profiles_dir.join("coder.toml"),
            "name = \"coder\"\nmodel = \"claude-sonnet-4-20250514\"\n",
        )
        .expect("write coder profile");

        let harness = ProfilesReloadHarness::new(&profiles_dir);
        harness
            .manager
            .load_from_dir(&profiles_dir, &LocalSyncFs)
            .expect("initial load");
        assert_eq!(harness.manager.list(), builtin_profile_names()); // coder overrides the built-in

        // When the directory gains a second profile and loses the first
        // (create + delete in one burst)
        fs::write(
            profiles_dir.join("explore.toml"),
            "name = \"explore\"\nmodel = \"claude-haiku-4-20250514\"\n",
        )
        .expect("write explore profile");
        fs::remove_file(profiles_dir.join("coder.toml")).expect("remove coder profile");
        harness.reload().await;

        // Then the manager holds exactly the current directory contents
        // (user profiles reloaded over the built-ins; the deleted file's
        // profile is gone, the new one is in)
        assert_eq!(harness.manager.list(), builtin_profile_names());
    }

    /// should keep an essential override registered across hot reloads (D16:
    /// the file is a content override, not a suppressed candidate — it must
    /// survive the last-known-good re-registration)
    #[tokio::test]
    async fn should_keep_the_essential_override_when_a_user_file_declares_the_agent_name() {
        // Given a profiles directory whose file declares name = "agent"
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let profiles_dir = temp_dir.path().join("profiles");
        fs::create_dir_all(&profiles_dir).expect("create profiles dir");
        fs::write(
            profiles_dir.join("agent.toml"),
            "name = \"agent\"\nmodel = \"gpt-4o\"\n",
        )
        .expect("write agent-override profile");

        // And a watcher harness seeded from a boot scan over it (the
        // essential override enters the last-known-good set like any file)
        let harness = ProfilesReloadHarness::new(&profiles_dir);

        // When reloads run (the file event burst and a later one)
        harness.reload().await;
        harness.reload().await;

        // Then the override stays the registered definition — the hot
        // reload never resurrects the built-in over the user's file
        let essential = harness
            .manager
            .get("agent")
            .expect("the essential name stays registered");
        assert_eq!(essential.model, "gpt-4o");
        // And the dispatch face keeps offering the (overridden) essential
        assert!(harness.manager.list().contains(&"agent".to_string()));
    }

    /// should keep the last known good registration when a previously good
    /// file becomes malformed (the incremental R2-3 contract)
    #[tokio::test]
    async fn should_keep_last_known_good_profile_when_a_file_becomes_malformed() {
        // Given a manager loaded from a directory with one good profile
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let profiles_dir = temp_dir.path().join("profiles");
        fs::create_dir_all(&profiles_dir).expect("create profiles dir");
        fs::write(
            profiles_dir.join("custom.toml"),
            "name = \"custom\"\nmodel = \"model-good\"\n",
        )
        .expect("write custom profile");

        let harness = ProfilesReloadHarness::new(&profiles_dir);
        harness
            .manager
            .load_from_dir(&profiles_dir, &LocalSyncFs)
            .expect("initial load");
        assert_eq!(
            harness
                .manager
                .get("custom")
                .expect("custom is loaded")
                .model,
            "model-good"
        );

        // When the file is replaced with a malformed one and the reload
        // runs (an editor save gone wrong)
        fs::write(profiles_dir.join("custom.toml"), "name = [unclosed").expect("break profile");
        harness.reload().await;

        // Then the last known good registration survives the broken file —
        // the profile is neither unregistered nor reset to a built-in
        assert_eq!(
            harness
                .manager
                .get("custom")
                .expect("the last known good registration survives")
                .model,
            "model-good"
        );

        // And when the file is fixed with a new definition, the fresh
        // registration replaces the kept one
        fs::write(
            profiles_dir.join("custom.toml"),
            "name = \"custom\"\nmodel = \"model-fixed\"\n",
        )
        .expect("fix profile");
        harness.reload().await;
        assert_eq!(
            harness
                .manager
                .get("custom")
                .expect("custom is loaded")
                .model,
            "model-fixed"
        );

        // And when the file is deleted, the profile finally drops out
        fs::remove_file(profiles_dir.join("custom.toml")).expect("remove profile");
        harness.reload().await;
        assert!(harness.manager.get("custom").is_none());
    }

    /// should unregister a profile when its file is replaced by a
    /// higher-priority same-name file
    #[tokio::test]
    async fn should_replace_a_toml_registration_when_a_same_name_md_appears() {
        // Given a manager loaded from a directory with a .toml profile
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let profiles_dir = temp_dir.path().join("profiles");
        fs::create_dir_all(&profiles_dir).expect("create profiles dir");
        fs::write(
            profiles_dir.join("scout.toml"),
            "name = \"scout\"\nmodel = \"from-toml\"\n",
        )
        .expect("write scout profile");

        let harness = ProfilesReloadHarness::new(&profiles_dir);
        harness
            .manager
            .load_from_dir(&profiles_dir, &LocalSyncFs)
            .expect("initial load");
        assert_eq!(
            harness.manager.get("scout").expect("scout loads").model,
            "from-toml"
        );

        // When a same-name .md appears (higher priority) and the reload runs
        fs::write(
            profiles_dir.join("scout.md"),
            "---\nname: scout\ndescription: Scout\n---\nYou scout from markdown.",
        )
        .expect("write scout.md");
        harness.reload().await;

        // Then the .md registration wins — the stale .toml last-known-good
        // must not resurrect beside it
        let scout = harness
            .manager
            .get("scout")
            .expect("scout stays registered");
        assert_eq!(
            scout.system_prompt.as_deref(),
            Some("You scout from markdown."),
            "the .md must replace the .toml registration"
        );
    }

    #[tokio::test]
    async fn should_keep_old_profiles_when_the_root_scan_fails() {
        // Given a profiles directory with one valid profile and a manager
        // loaded from it
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let profiles_dir = temp_dir.path().join("profiles");
        fs::create_dir_all(&profiles_dir).expect("create profiles dir");
        fs::write(
            profiles_dir.join("coder.toml"),
            "name = \"coder\"\nmodel = \"claude-sonnet-4-20250514\"\n",
        )
        .expect("write coder profile");

        let harness = ProfilesReloadHarness::new(&profiles_dir);
        harness
            .manager
            .load_from_dir(&profiles_dir, &LocalSyncFs)
            .expect("initial load");
        assert_eq!(harness.manager.list(), builtin_profile_names()); // coder overrides the built-in

        // When the root itself becomes unlistable — a regular file where a
        // directory is expected — and the reload is triggered
        let not_a_dir = temp_dir.path().join("not-a-dir");
        fs::write(&not_a_dir, "occupied").expect("write blocking file");
        let failing = ProfilesReloadHarness {
            roots: vec![not_a_dir],
            ..ProfilesReloadHarness::new(&profiles_dir)
        };
        failing.reload().await;

        // Then the previously loaded profiles survive the failed scan (a
        // scan-level failure keeps everything — unlike a single bad file,
        // which skip-and-collects)
        assert_eq!(harness.manager.list(), builtin_profile_names());
    }

    #[tokio::test]
    async fn should_replace_hooks_when_config_toml_changes() {
        // Given an engine and shared config loaded from a config.toml
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let config_path = temp_dir.path().join("config.toml");
        let (engine, config_ref) = config_reload_env(
            &config_path,
            "[[hooks]]\nevent = \"SessionStart\"\ncommand = \"echo start\"\n",
        );
        reload_full_config(&config_path, &engine, &config_ref, None).await;
        assert_eq!(engine.list().len(), 1);

        // When config.toml replaces the hook and adds a second one
        fs::write(
            &config_path,
            "[[hooks]]\nevent = \"SessionEnd\"\ncommand = \"echo end\"\n\n\
             [[hooks]]\nevent = \"Stop\"\ncommand = \"echo stop\"\n",
        )
        .expect("rewrite config");
        reload_full_config(&config_path, &engine, &config_ref, None).await;

        // Then the engine holds exactly the new hooks (replaced, not
        // appended) and the shared snapshot carries the same hooks
        let hooks = engine.list();
        assert_eq!(hooks.len(), 2);
        assert_eq!(hooks[0].event, HookEventType::SessionEnd);
        assert_eq!(hooks[0].command, "echo end");
        assert_eq!(hooks[1].event, HookEventType::Stop);
        assert_eq!(
            config_ref
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .hooks
                .as_ref()
                .map(|hooks| hooks.len()),
            Some(2)
        );
    }

    #[tokio::test]
    async fn should_keep_current_hooks_when_config_toml_is_malformed() {
        // Given an engine and shared config loaded from a valid config.toml
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let config_path = temp_dir.path().join("config.toml");
        let (engine, config_ref) = config_reload_env(
            &config_path,
            "[[hooks]]\nevent = \"SessionStart\"\ncommand = \"echo start\"\n",
        );
        reload_full_config(&config_path, &engine, &config_ref, None).await;
        assert_eq!(engine.list().len(), 1);

        // When config.toml is overwritten with an invalid document
        fs::write(&config_path, "[[hooks\n").expect("write malformed config");
        reload_full_config(&config_path, &engine, &config_ref, None).await;

        // Then the previous registrations and snapshot survive the failed
        // reload (best-effort: warn, never crash)
        let hooks = engine.list();
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].command, "echo start");
        assert_eq!(
            config_ref
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .hooks
                .as_ref()
                .and_then(|hooks| hooks.first())
                .map(|hook| hook.command.as_str()),
            Some("echo start")
        );
    }

    #[tokio::test]
    async fn should_keep_registrations_unchanged_when_config_toml_hooks_are_unchanged() {
        // Given an engine holding exactly the hooks of the config on disk
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let config_path = temp_dir.path().join("config.toml");
        let config_body = "[[hooks]]\nevent = \"SessionStart\"\ncommand = \"echo start\"\n\n[model]\nid = \"gpt\"\n";
        let (engine, config_ref) = config_reload_env(&config_path, config_body);
        reload_full_config(&config_path, &engine, &config_ref, None).await;

        // When an edit touches only a non-hook section (same hooks section)
        fs::write(&config_path, config_body.replace("gpt", "gpt-5")).expect("rewrite config");
        reload_full_config(&config_path, &engine, &config_ref, None).await;

        // Then the engine keeps exactly one registration, while the
        // reloadable non-hook section is applied to the shared snapshot
        assert_eq!(engine.list().len(), 1);
        assert_eq!(
            config_ref
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .model
                .as_ref()
                .map(|model| model.id.as_str()),
            Some("gpt-5")
        );
    }

    #[tokio::test]
    async fn should_reload_providers_when_config_toml_changes() {
        // Given a shared config loaded from a config.toml with the legacy
        // [provider] section
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let config_path = temp_dir.path().join("config.toml");
        let (engine, config_ref) =
            config_reload_env(&config_path, "[provider]\napi_key = \"sk-old\"\n");
        reload_full_config(&config_path, &engine, &config_ref, None).await;

        // When config.toml replaces the legacy [provider] with a providers
        // map
        fs::write(
            &config_path,
            "[providers.anthropic]\ntype = \"anthropic\"\napi_key = \"sk-new\"\n",
        )
        .expect("rewrite config");
        reload_full_config(&config_path, &engine, &config_ref, None).await;

        // Then the shared snapshot carries the new providers entry
        let config = config_ref.read().unwrap_or_else(|e| e.into_inner());
        let providers = config.providers.as_ref().expect("providers must reload");
        let entry = providers
            .get("anthropic")
            .expect("anthropic must be registered after reload");
        assert_eq!(entry.api_key.as_deref(), Some("sk-new"));
        assert_eq!(entry.r#type.as_deref(), Some("anthropic"));
    }

    #[tokio::test]
    async fn should_reload_models_when_config_toml_changes() {
        // Given a shared config loaded from a config.toml with one model
        // alias
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let config_path = temp_dir.path().join("config.toml");
        let (engine, config_ref) = config_reload_env(
            &config_path,
            "[models.fast]\nprovider = \"anthropic\"\nmodel = \"claude-haiku-4-20250514\"\nmax_context_size = 200000\n",
        );
        reload_full_config(&config_path, &engine, &config_ref, None).await;

        // When config.toml swaps the alias for a different model
        fs::write(
            &config_path,
            "[models.turbo]\nprovider = \"openai\"\nmodel = \"gpt-4o\"\nmax_context_size = 128000\n",
        )
        .expect("rewrite config");
        reload_full_config(&config_path, &engine, &config_ref, None).await;

        // Then the shared snapshot carries the new alias and drops the old
        // one
        let config = config_ref.read().unwrap_or_else(|e| e.into_inner());
        let models = config.models.as_ref().expect("models must reload");
        assert!(models.get("fast").is_none(), "old alias must disappear");
        let alias = models
            .get("turbo")
            .expect("turbo must be registered after reload");
        assert_eq!(alias.provider, "openai");
        assert_eq!(alias.model, "gpt-4o");
    }

    #[tokio::test]
    async fn should_propagate_reloaded_config_to_model_catalog_when_config_toml_changes() {
        // Given a shared config snapshot — the `std::sync::RwLock` the
        // embedded server also consumes — wrapped by a `ConfigModelCatalog`
        // (the exact wiring `SessionSubagentHost` receives at
        // `session.create`), and a config.toml with one model alias
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let config_path = temp_dir.path().join("config.toml");
        let (engine, config_ref) = config_reload_env(
            &config_path,
            "[models.fast]\nprovider = \"anthropic\"\nmodel = \"claude-haiku-4-20250514\"\nmax_context_size = 200000\n",
        );
        let catalog = mirri_embedded::ConfigModelCatalog::new(Arc::clone(&config_ref));
        let ids = |catalog: &mirri_embedded::ConfigModelCatalog| {
            catalog
                .list_models()
                .iter()
                .map(|entry| entry.id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(ids(&catalog), vec!["fast".to_string()]);

        // When config.toml swaps the alias and the reload runs through the
        // watcher's shared config Arc
        fs::write(
            &config_path,
            "[models.turbo]\nprovider = \"openai\"\nmodel = \"gpt-4o\"\nmax_context_size = 128000\n",
        )
        .expect("rewrite config");
        reload_full_config(&config_path, &engine, &config_ref, None).await;

        // Then the catalog reflects the new config immediately: one shared
        // Arc is the source of truth for both the watcher's swap and the
        // embedded server's readers
        assert_eq!(ids(&catalog), vec!["turbo".to_string()]);
        assert!(catalog.get_by_id("turbo").is_some());
        assert!(
            catalog.get_by_id("fast").is_none(),
            "the reloaded snapshot must replace the old alias"
        );
    }

    #[tokio::test]
    async fn should_reload_thinking_config_when_config_toml_changes() {
        // Given a shared config loaded from a config.toml with a thinking
        // section
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let config_path = temp_dir.path().join("config.toml");
        let (engine, config_ref) = config_reload_env(
            &config_path,
            "[thinking]\nenabled = true\neffort = \"high\"\n",
        );
        reload_full_config(&config_path, &engine, &config_ref, None).await;

        // When config.toml changes the thinking section
        fs::write(
            &config_path,
            "[thinking]\nenabled = false\neffort = \"low\"\n",
        )
        .expect("rewrite config");
        reload_full_config(&config_path, &engine, &config_ref, None).await;

        // Then the shared snapshot carries the new thinking values
        let config = config_ref.read().unwrap_or_else(|e| e.into_inner());
        let thinking = config.thinking.as_ref().expect("thinking must reload");
        assert_eq!(thinking.enabled, Some(false));
        assert_eq!(thinking.effort.as_deref(), Some("low"));
    }

    #[tokio::test]
    async fn should_reload_permission_config_when_config_toml_changes() {
        // Given a shared config loaded from a config.toml with a permission
        // section
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let config_path = temp_dir.path().join("config.toml");
        let (engine, config_ref) =
            config_reload_env(&config_path, "[permission]\nmode = \"manual\"\n");
        reload_full_config(&config_path, &engine, &config_ref, None).await;

        // When config.toml switches the permission mode
        fs::write(&config_path, "[permission]\nmode = \"yolo\"\n").expect("rewrite config");
        reload_full_config(&config_path, &engine, &config_ref, None).await;

        // Then the shared snapshot carries the new mode
        let config = config_ref.read().unwrap_or_else(|e| e.into_inner());
        let permission = config.permission.as_ref().expect("permission must reload");
        assert_eq!(permission.mode.as_deref(), Some("yolo"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // read guards are dropped before every await; region inference over-approximates
    async fn should_not_reload_experimental_flags_when_config_toml_changes() {
        // Given a shared config whose experimental flags were loaded from
        // config.toml
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let config_path = temp_dir.path().join("config.toml");
        let (engine, config_ref) = config_reload_env(
            &config_path,
            "[providers.anthropic]\ntype = \"anthropic\"\napi_key = \"sk-1\"\n\n[experimental]\nstreaming = true\n",
        );
        reload_full_config(&config_path, &engine, &config_ref, None).await;
        assert_eq!(
            config_ref
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .experimental
                .as_ref()
                .and_then(|flags| flags.get("streaming")),
            Some(&true)
        );

        // When config.toml changes ONLY the experimental section
        fs::write(
            &config_path,
            "[providers.anthropic]\ntype = \"anthropic\"\napi_key = \"sk-1\"\n\n[experimental]\nstreaming = false\n",
        )
        .expect("rewrite config");
        reload_full_config(&config_path, &engine, &config_ref, None).await;

        // Then nothing reloads at all: providers stay registered and the
        // experimental flag keeps its previous value (restart-only)
        let config = config_ref.read().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            config
                .providers
                .as_ref()
                .and_then(|providers| providers.get("anthropic"))
                .and_then(|entry| entry.api_key.as_deref()),
            Some("sk-1")
        );
        assert_eq!(
            config
                .experimental
                .as_ref()
                .and_then(|flags| flags.get("streaming")),
            Some(&true)
        );
        drop(config);

        // When config.toml changes a reloadable section AND the experimental
        // section together
        fs::write(
            &config_path,
            "[providers.anthropic]\ntype = \"anthropic\"\napi_key = \"sk-2\"\n\n[experimental]\nstreaming = false\n",
        )
        .expect("rewrite config");
        reload_full_config(&config_path, &engine, &config_ref, None).await;

        // Then the reloadable section takes effect but the experimental flag
        // is preserved from the previous snapshot
        let config = config_ref.read().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            config
                .providers
                .as_ref()
                .and_then(|providers| providers.get("anthropic"))
                .and_then(|entry| entry.api_key.as_deref()),
            Some("sk-2")
        );
        assert_eq!(
            config
                .experimental
                .as_ref()
                .and_then(|flags| flags.get("streaming")),
            Some(&true),
            "experimental flags must survive a reload of other sections"
        );
    }

    #[tokio::test]
    async fn should_not_crash_when_config_toml_is_invalid() {
        // Given a shared config and engine loaded from a valid config.toml
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let config_path = temp_dir.path().join("config.toml");
        let (engine, config_ref) = config_reload_env(
            &config_path,
            "[model]\nid = \"gpt-4o\"\n\n[[hooks]]\nevent = \"Stop\"\ncommand = \"echo stop\"\n",
        );
        reload_full_config(&config_path, &engine, &config_ref, None).await;
        assert_eq!(engine.list().len(), 1);

        // When config.toml is overwritten with an invalid document
        fs::write(&config_path, "[model\nid = ").expect("write malformed config");
        reload_full_config(&config_path, &engine, &config_ref, None).await;

        // Then the previous snapshot and hook registrations survive
        // (best-effort: warn, never crash)
        assert_eq!(
            config_ref
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .model
                .as_ref()
                .map(|model| model.id.as_str()),
            Some("gpt-4o")
        );
        let hooks = engine.list();
        assert_eq!(hooks.len(), 1);
        assert_eq!(hooks[0].command, "echo stop");
    }

    #[tokio::test]
    async fn should_notify_on_reload_callback_when_config_toml_changes() {
        // Given a reload callback that records the permission mode of every
        // applied config
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let config_path = temp_dir.path().join("config.toml");
        let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let on_reload = {
            let seen = Arc::clone(&seen);
            Some(Arc::new(move |config: &ServerConfig| {
                seen.lock().expect("lock seen modes").push(
                    config
                        .permission
                        .as_ref()
                        .and_then(|p| p.mode.clone())
                        .unwrap_or_default(),
                );
            }) as Arc<dyn Fn(&ServerConfig) + Send + Sync>)
        };
        let (engine, config_ref) =
            config_reload_env(&config_path, "[permission]\nmode = \"manual\"\n");
        // The seeded env already holds the on-disk config; an unchanged file
        // must not notify.
        reload_full_config(&config_path, &engine, &config_ref, on_reload.as_ref()).await;
        assert!(seen.lock().expect("lock seen modes").is_empty());

        // When config.toml changes a reloadable section twice
        fs::write(&config_path, "[permission]\nmode = \"auto\"\n").expect("rewrite config");
        reload_full_config(&config_path, &engine, &config_ref, on_reload.as_ref()).await;
        fs::write(&config_path, "[permission]\nmode = \"yolo\"\n").expect("rewrite config");
        reload_full_config(&config_path, &engine, &config_ref, on_reload.as_ref()).await;

        // Then the callback ran with the applied config for each successful
        // reload, and never for a no-op
        assert_eq!(
            seen.lock().expect("lock seen modes").as_slice(),
            &["auto".to_string(), "yolo".to_string()]
        );
    }

    #[test]
    fn should_report_relevant_when_event_is_create_modify_or_remove_of_watched_file() {
        // Given a watched config file
        let watched = PathBuf::from("/tmp/mirri-test/mcp.json");

        // When events of each mutating kind touch that file
        let kinds = [
            EventKind::Create(CreateKind::File),
            EventKind::Modify(ModifyKind::Data(DataChange::Any)),
            EventKind::Remove(RemoveKind::File),
        ];
        for kind in kinds {
            let event = Event::new(kind).add_path(watched.clone());
            // Then they are all reported as relevant
            assert!(is_relevant_file_event(
                &event,
                std::slice::from_ref(&watched)
            ));
        }
    }

    #[test]
    fn should_ignore_unrelated_paths_and_access_events_when_filtering_watched_files() {
        // Given a watched config file
        let watched = PathBuf::from("/tmp/mirri-test/mcp.json");

        // When an event targets a different file in the same directory, or a
        // read/access event targets the watched file itself
        let wrong_file = Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Any)))
            .add_path(PathBuf::from("/tmp/mirri-test/other.json"));
        let access_event = Event::new(EventKind::Access(notify::event::AccessKind::Read))
            .add_path(watched.clone());

        // Then neither is reported as relevant
        assert!(!is_relevant_file_event(
            &wrong_file,
            std::slice::from_ref(&watched)
        ));
        assert!(!is_relevant_file_event(&access_event, &[watched]));
    }

    #[test]
    fn should_ignore_non_profile_files_when_filtering_profile_directory_events() {
        // When an event touches a .toml profile file and an editor swap file
        let profile_event = Event::new(EventKind::Create(CreateKind::File))
            .add_path(PathBuf::from("/tmp/mirri-test/profiles/coder.toml"));
        let swap_event = Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Any)))
            .add_path(PathBuf::from("/tmp/mirri-test/profiles/.coder.toml.swp"));

        // Then only the profile file is relevant (the root guard is the
        // profiles dir the file lives in)
        let roots = [PathBuf::from("/tmp/mirri-test/profiles")];
        assert!(is_relevant_profile_root_event(&profile_event, &roots));
        assert!(!is_relevant_profile_root_event(&swap_event, &roots));
    }

    #[test]
    #[cfg(unix)]
    fn should_match_when_paths_differ_only_by_realpath_prefix() {
        use std::os::unix::fs::symlink;

        // Given a real file reachable through two lexical paths that resolve
        // to the same canonical file (a symlinked directory alias, as when a
        // watcher-delivered event path uses `/private/tmp` for a watched
        // `/tmp` path)
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let real_dir = temp_dir.path().join("real");
        let link_dir = temp_dir.path().join("link");
        fs::create_dir_all(&real_dir).expect("create real dir");
        symlink(&real_dir, &link_dir).expect("create symlink alias");
        let file_through_real = real_dir.join("config.toml");
        let file_through_link = link_dir.join("config.toml");
        fs::write(&file_through_real, "").expect("write file");
        fs::write(real_dir.join("other.toml"), "").expect("write sibling");

        // Then the same file matches through the alias in both directions,
        // and a genuine sibling does not
        assert!(same_file(&file_through_real, &file_through_link));
        assert!(same_file(&file_through_link, &file_through_real));
        assert!(!same_file(&file_through_real, &real_dir.join("other.toml")));
    }

    #[tokio::test]
    async fn should_wait_for_quiet_period_when_events_stop_arriving() {
        // Given a receiver and a short quiet window
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<()>();
        // Keep this clone alive for the whole wait: when every sender is
        // dropped the channel closes and `recv` returns `None`, which must
        // not be mistaken for a quiet window.
        let _keep_channel_open = sender.clone();
        let window = Duration::from_millis(50);

        // When a burst of events arrives inside the window (a second event
        // 10 ms after the first)
        let started = std::time::Instant::now();
        sender.send(()).expect("send first event");
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let _ = sender.send(());
        });

        // Then the wait returns only after the receiver has been quiet for
        // the full window
        wait_for_quiet_period(&mut receiver, window).await;
        assert!(
            started.elapsed() >= window,
            "quiet period must not end early while events keep arriving"
        );
    }

    #[tokio::test]
    async fn should_reload_profiles_when_profile_file_is_created_on_disk() {
        // Given a spawned profiles watcher over a temp directory (exercises
        // the full pipeline: notify OS events -> channel bridge -> debounce
        // -> reload)
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let profiles_dir = temp_dir.path().join("profiles");
        fs::create_dir_all(&profiles_dir).expect("create profiles dir");
        let manager = Arc::new(ProfileManager::new());
        spawn_config_watchers(HotReloadInputs {
            mcp: None,
            profiles: Some(ProfilesHotReloadInputs {
                roots: vec![profiles_dir.clone()],
                manager: Arc::clone(&manager),
            }),
            config: None,
        });

        // When a profile file is created in the watched directory
        fs::write(
            profiles_dir.join("coder.toml"),
            "name = \"coder\"\nmodel = \"claude-sonnet-4-20250514\"\n",
        )
        .expect("write profile");

        // Then the manager picks it up within the OS event-delivery window
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if manager.list().contains(&"coder".to_string()) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "profile hot-reload did not pick up the new file in time"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// should hot-reload a subdirectory profile when a nested agent file is
    /// modified or created in a watched root
    #[tokio::test]
    async fn should_hot_reload_a_subdirectory_profile_when_a_nested_agent_file_changes() {
        // Given a spawned profiles watcher over a root whose subdirectory
        // already holds a profile (the recursive discovery walk loads it at
        // boot — the watch must be recursive for it to hot-reload too)
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let agents_dir = temp_dir.path().join("agents");
        let team_dir = agents_dir.join("team");
        fs::create_dir_all(&team_dir).expect("create nested agents dir");
        fs::write(
            team_dir.join("reviewer.toml"),
            "name = \"reviewer\"\nmodel = \"model-a\"\n",
        )
        .expect("write nested reviewer profile");

        let manager = Arc::new(ProfileManager::new());
        manager
            .load_from_dir(&agents_dir, &LocalSyncFs)
            .expect("initial load");
        assert_eq!(
            manager.get("reviewer").expect("nested profile loads").model,
            "model-a"
        );

        spawn_config_watchers(HotReloadInputs {
            mcp: None,
            profiles: Some(ProfilesHotReloadInputs {
                roots: vec![agents_dir.clone()],
                manager: Arc::clone(&manager),
            }),
            config: None,
        });

        // When the nested profile is modified and a second nested profile
        // is created (both inside the subdirectory, not the root itself)
        fs::write(
            team_dir.join("reviewer.toml"),
            "name = \"reviewer\"\nmodel = \"model-b\"\n",
        )
        .expect("modify nested profile");
        fs::write(
            team_dir.join("nested.md"),
            "---\nname: nested\ndescription: Nested\n---\nYou live in a subdirectory.",
        )
        .expect("create nested .md profile");

        // Then both changes are reflected within the OS event-delivery
        // window — the modified registration is replaced and the new
        // nested profile is registered
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let reviewer_ready = manager
                .get("reviewer")
                .is_some_and(|profile| profile.model == "model-b");
            let nested_ready = manager.get("nested").is_some_and(|profile| {
                profile.system_prompt.as_deref() == Some("You live in a subdirectory.")
            });
            if reviewer_ready && nested_ready {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "nested profile hot-reload did not pick up the subdirectory changes in time"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn should_debounce_rapid_config_changes() {
        // Given a spawned config.toml watcher (full pipeline: notify OS
        // events -> channel bridge -> debounce -> reload) whose reload
        // callback counts how many reloads actually ran
        let temp_dir = tempfile::tempdir().expect("tempdir should succeed");
        let config_dir = temp_dir.path().join(".mirri-code");
        fs::create_dir_all(&config_dir).expect("create config dir");
        let config_path = config_dir.join("config.toml");
        fs::write(&config_path, "[provider]\napi_key = \"sk-0\"\n").expect("write initial config");

        let engine = Arc::new(HookEngine::new());
        let config_ref = Arc::new(RwLock::new(ServerConfig::default()));
        let reload_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let on_reload = {
            let reload_count = Arc::clone(&reload_count);
            Some(Arc::new(move |_config: &ServerConfig| {
                reload_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }) as Arc<dyn Fn(&ServerConfig) + Send + Sync>)
        };
        spawn_config_watchers(HotReloadInputs {
            mcp: None,
            profiles: None,
            config: Some(ConfigHotReloadInputs {
                config_path: config_path.clone(),
                engine: Arc::clone(&engine),
                config: Arc::clone(&config_ref),
                on_reload,
            }),
        });

        // When a burst of writes lands inside the 200 ms debounce window
        fs::write(&config_path, "[provider]\napi_key = \"sk-1\"\n").expect("write burst 1");
        fs::write(&config_path, "[provider]\napi_key = \"sk-2\"\n").expect("write burst 2");
        fs::write(&config_path, "[provider]\napi_key = \"sk-3\"\n").expect("write burst 3");

        // Then the watcher converges to the final write, and the burst
        // collapses into a single reload
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let current = config_ref
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .provider
                .as_ref()
                .and_then(|provider| provider.api_key.clone());
            if current.as_deref() == Some("sk-3") {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "config hot-reload did not converge to the final write in time"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(
            reload_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a burst of config changes must collapse into a single reload"
        );
    }
}
