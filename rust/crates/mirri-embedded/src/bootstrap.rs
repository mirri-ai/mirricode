//! The shared composition root: builds the fully-wired [`EngineParts`] for
//! both engine entry points — `mirri serve`'s bootstrap (in `mirri-cli`) and
//! [`EmbeddedServer::for_workspace_root`](crate::EmbeddedServer::for_workspace_root)
//! / `for_cwd` (the ACP / NAPI consumers).
//!
//! [`assemble`] is the single place where the workspace scope, session
//! manager, config snapshot, agent profiles, MCP connection manager,
//! plugins, LLM factory and the local `FetchURL` provider are constructed.
//! The [`WorkspacePolicy`] parameter selects how the workspace scope is
//! derived from the loaded config:
//!
//! - [`WorkspacePolicy::UntrustedDefault`] — `WorkspaceScopeConfig::default()`
//!   (Untrusted, Manual, no hooks); the historical `for_workspace_root`
//!   behavior, used by ACP / `for_cwd` / NAPI.
//! - [`WorkspacePolicy::TrustedFromConfig`] — Trusted + permission mode +
//!   hooks read from the loaded config.toml; the historical `mirri serve`
//!   behavior.
//!
//! Everything else is shared between the policies: `mirri serve` gained the
//! funnel's plugins and local `FetchURL` provider, and the funnel gained
//! serve's one-shot config migrations — the config-load order is upgraded to
//! `load_without_env` → `migrate_config` → `expand_env_vars` → `apply_env`
//! (see [`load_config_with_migration`]).

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use chrono::Utc;
use mirri_core::cron::WorkspaceCronManager;
use mirri_core::{
    PermissionMode, ProfileManager, ServerConfig, SessionManager, SkillRootChannel, SkillSource,
    WorkspaceId, WorkspaceScope, WorkspaceScopeConfig, WorkspaceTrustLevel, dedupe_skill_roots,
    default_skill_root_channels, encode_work_dir_key, migrate_config,
};

use crate::providers::{ConfigAwareLlmFactory, LocalFetchUrlProvider};
use crate::workspace_registry::{
    NoopResolver, RuntimeBuilder, WorkspaceRegistration, WorkspaceRegistry, WorkspaceRuntime,
};
use crate::workspace_trust::WorkspaceTrustStore;

/// Policy for the workspace scope config the root builds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspacePolicy {
    /// `for_workspace_root` behavior: `WorkspaceScopeConfig::default()`
    /// (Untrusted, Manual, no hooks). Used by ACP / for_cwd / NAPI.
    UntrustedDefault,
    /// `serve` behavior: Trusted + permission mode + hooks derived from the
    /// loaded config.toml. Used by `mirri serve`.
    TrustedFromConfig,
}

/// Every engine component a boot path needs after assembly.
///
/// [`assemble`] produces one of these; `mirri serve` wires it into
/// `ServerState`, and [`EmbeddedServer::from_parts`](crate::EmbeddedServer::from_parts)
/// consumes it to build the in-process engine.
pub struct EngineParts {
    /// The session manager every RPC method operates on.
    pub session_manager: Arc<SessionManager>,
    /// The current server configuration snapshot (env-expanded, migrated),
    /// shared with hot-reload watchers and `config.get` / `config.set`.
    pub config: Arc<RwLock<ServerConfig>>,
    /// Path of the config.toml the config was loaded from, when one was
    /// found and parsed. `None` when the config fell back to defaults
    /// (missing HOME or missing/unparseable config.toml).
    pub config_path: Option<PathBuf>,
    /// Agent profiles resolved from the user roots chain (brand agents dir,
    /// generic agents dir, legacy `<mirri-home>/profiles`); enablement comes
    /// from each file's own `enabled` flag (R3-3).
    pub profile_manager: Arc<ProfileManager>,
    /// MCP connection manager for the **workspace pool** (the project layers'
    /// connections plus plugin-contributed servers). `None` when no MCP
    /// config exists — the server runs fine without MCP.
    pub mcp_manager: Option<Arc<mirri_mcp::McpConnectionManager>>,
    /// The app-level **user pool** for the user config layer's connections,
    /// when a user layer was discovered. Assembled engines get a private
    /// pool (test isolation, single-workspace processes); the NAPI
    /// composition root swaps in the process-level shared pool
    /// (`mirri_embedded::shared_user_mcp_pool_for_home`).
    pub user_mcp_pool: Option<Arc<crate::user_mcp_pool::UserMcpPool>>,
    /// MCP server configs discovered at startup with each layer's
    /// provenance kept (the pool-routing view — the user layer feeds the
    /// user pool, the project layers the workspace manager). The actual
    /// `connect_all` is deferred until the first engine call (which is
    /// guaranteed to run inside a Tokio runtime, unlike `assemble`, which
    /// may execute before the runtime exists).
    pub pending_mcp_configs: Option<mirri_mcp::config::LayeredMcpConfig>,
    /// Plugin manager, when the Mirri home has a `plugins/` directory.
    pub plugin_manager: Option<Arc<mirri_plugin_host::PluginManager>>,
    /// The LLM factory powering engine turns: model ids resolve through the
    /// configured `[models.<alias>]` → `[providers.<id>]` chain (the M1 two
    /// forms — alias key or `provider/model` composite; anything else is
    /// the R10 not-configured error listing the available models, never a
    /// bare-name environment fallback) — [`ConfigAwareLlmFactory`].
    pub llm_factory: Arc<dyn mirri_core::LlmFactory>,
    /// The local SSRF-guarded FetchURL provider (registers the `FetchURL`
    /// builtin tool); `WebSearch` stays unregistered until managed-search
    /// auth is wired.
    pub fetch_url_provider: Option<Arc<dyn mirri_core::tools::FetchUrlProvider>>,
    /// The explicitly injected Mirri home directory, when one was given —
    /// the directory holding `config.toml`, `profiles/`, `plugins/`, MCP
    /// config, and the session persistence root (`v3_sessions/` and
    /// `session-index.db`). `None` means every home consumer resolves
    /// through the env-based
    /// [`mirri_home_dir`](mirri_core::config::mirri_home_dir) exactly as
    /// before the injection existed; a home that cannot be resolved at
    /// all is a hard error at assembly (ADR-006 — the metadata store
    /// needs its database, the no-home shape is out of the product
    /// surface).
    pub home_dir: Option<PathBuf>,
    /// The SDK's explicit skill directories (v1/v2 `skillDirs`), as given to
    /// [`assemble`] — replayed onto every built workspace's skill catalog by
    /// [`apply_skill_discovery`] (at assembly and on config-write resync).
    pub skill_dirs: Vec<PathBuf>,
    /// The base directory whose `/.mirri-code/mcp.json` forms the MCP user
    /// layer — the **exact** base the pool load resolved here (`mcp_home_dir`
    /// when given, else [`resolve_mcp_home_base`]), so RPC-time consumers
    /// (`mcp.config.*` reads/writes, the reconcile kernel, the tool-filter
    /// merge) address the very file the running pools loaded instead of
    /// re-resolving and risking a divergent path (the
    /// `~/.mirri-code/.mirri-code/mcp.json` double-nesting class of bug).
    /// `None` when no base could be resolved at all (no MCP config either).
    pub mcp_home_base: Option<PathBuf>,
    /// The workspace registry this assembly produced: the default workspace
    /// registered and built, every restored `workspaces.json` row registered
    /// but left lazy (D5), and the session routing seam (`manager_for`)
    /// resolving against the active table / index fallback. `session_manager`
    /// above is this registry's default runtime manager.
    pub workspace_registry: Arc<WorkspaceRegistry>,
    /// The home-level workspace trust store (W7 D6 / W8): the same handle
    /// the registry's runtime builder consults at build time, handed to the
    /// engine so the `workspace.setTrust` RPC face reads/writes the very
    /// file the running gate was seeded from. `None` without an injected
    /// home — every non-default workspace then stays untrusted and
    /// `workspace.setTrust` answers invalid_params (nothing persists).
    pub trust_store: Option<WorkspaceTrustStore>,
}

/// Assemble the fully-wired engine parts for a workspace.
///
/// Extracted from [`EmbeddedServer::for_workspace_root`](crate::EmbeddedServer::for_workspace_root)
/// with `policy` parameterizing the workspace-scope construction; the
/// `UntrustedDefault` path is byte-identical to the historical funnel, and
/// `TrustedFromConfig` reproduces `mirri serve`'s scope derivation
/// (Trusted + permission mode + hooks from config.toml).
///
/// `mcp_home_dir` is the base directory whose `/.mirri-code/mcp.json`
/// forms the MCP global layer: `mirri serve` passes `$HOME` (the MCP
/// crate's documented contract); `None` falls back to the injected home
/// (the NAPI/ACP temp-dir convention), then to `$HOME` verbatim, then to
/// the resolved Mirri home — so a real (un-injected) user's
/// `~/.mirri-code/mcp.json` is discovered, never a double-nested
/// `~/.mirri-code/.mirri-code/mcp.json`.
///
/// `skill_dirs` carries the SDK's explicit skill directories (v1/v2
/// `skillDirs`, the CLI's `--skills-dir`): when non-empty they **replace**
/// the default user/project skill discovery — the workspace catalog is
/// reset and each directory is indexed as the user skill source, exactly
/// the v1/v2 SDK contract. Empty keeps the ambient discovery
/// (`<root>/.agents/skills` + `~/.agents/skills`). The replacement runs
/// after plugin skills are discovered, so the explicit user-priority dirs
/// override a same-name plugin skill (the TS explicit source sits at user
/// priority 20, above the plugin's 5 — item G).
///
/// Note: deliberately **not** `async` — ACP/NAPI construct engines through
/// [`EmbeddedServer::for_workspace_root`](crate::EmbeddedServer::for_workspace_root)
/// before a Tokio runtime exists, and every component here is synchronous.
pub fn assemble(
    workspace_id: WorkspaceId,
    workspace_root: PathBuf,
    home_dir: Option<PathBuf>,
    mcp_home_dir: Option<PathBuf>,
    policy: WorkspacePolicy,
    skill_dirs: Vec<PathBuf>,
) -> EngineParts {
    // ADR-006 (fix round 1, ruling on the None-home shape): the metadata
    // store is the single source of truth, so an assembly MUST have a
    // home to root `<home>/session-index.db` in. An injected home wins;
    // `None` falls back to the env-based resolution
    // (`MIRRICODE_HOME`, then `$HOME/.mirri-code`) — the same chain
    // `load_config_from_home` below already applies, so this only makes
    // the requirement explicit where it previously degraded to the
    // in-memory fixture. A home that cannot be resolved at all is out of
    // the product surface: the engine refuses to serve (no silent
    // in-memory degrade, which would silently drop every session row on
    // restart).
    // The RAW parameter is kept aside: the MCP user-layer base resolution
    // must see the caller's original injection state (fix round 2 — R1),
    // NOT the resolved mirri home. Feeding the resolved home down
    // `resolve_mcp_home_base` double-nests the user layer
    // (`<home>/.mirri-code/.mirri-code/mcp.json`) for every entry that
    // injected no home (ACP, `EngineBridge::new`, `for_cwd`), silently
    // hiding the user's real `~/.mirri-code/mcp.json`.
    let raw_home_dir = home_dir.clone();
    let home_dir: Option<PathBuf> = match resolve_mirri_home(home_dir.as_deref()) {
        Ok(home) => Some(home),
        Err(error) => panic!(
            "mirri home directory is required — the metadata store is the single source \
             of truth (ADR-006); pass a home or set MIRRICODE_HOME (underlying \
             resolution error: {error})"
        ),
    };

    // Auto-discover and load config.toml from the Mirri home directory
    // (migrate → `${VAR}` expand → env overrides; see
    // `load_config_with_migration`).
    let (config, config_path) = load_config_from_home(home_dir.as_deref());

    // The MCP user-layer base, resolved ONCE here and carried in the shared
    // services: the pool load, the config read/write surface, and the
    // reconcile kernel must all address the same file. An explicit base
    // (`mirri serve` passes `$HOME`) wins; otherwise see
    // [`resolve_mcp_home_base`] — over the RAW injection state, so an
    // un-injected entry falls through to `$HOME` verbatim (the user's
    // actual `~/.mirri-code/mcp.json`), never the resolved mirri home
    // (which would double-nest).
    let mcp_home_base = mcp_home_dir
        .clone()
        .or_else(|| resolve_mcp_home_base(raw_home_dir.as_deref()));

    // Auto-discover and load plugins from the Mirri home directory. Loaded
    // BEFORE the profile manager: the plugin agent roots (item H) join the
    // same profile-discovery pass as the user roots.
    let plugin_manager = load_plugin_manager_for_home(home_dir.as_deref());

    // Agent profiles from the user roots chain (brand agents dir, generic
    // agents dir, legacy <mirri-home>/profiles) plus the enabled plugins'
    // agent roots (item H) — the same loading `mirri serve` performs at
    // startup, extended with the plugin contribution.
    let profile_manager = load_profile_manager(
        &config.read().unwrap_or_else(|e| e.into_inner()),
        home_dir.as_deref(),
        plugin_manager.as_ref(),
    );

    // The config-aware LLM factory: model aliases known to the loaded
    // config resolve through their configured provider (the M1 two forms:
    // alias key or `provider/model` composite); anything else is the R10
    // not-configured error listing the available models — no bare-name
    // environment fallback (spec 11 ADR-004). The env-synthesized
    // `[models]` entries the config layer seeds from `MIRRICODE_MODEL_*`
    // are configured aliases and keep resolving; provider credentials
    // (apiKey/oauth, or the type's env var) are the provider-resolution
    // layer and stay env-injectable.
    // Before the factory bakes anything: migrate config.toml
    // `providers.*.api_key` fields into the credential store (spec E —
    // E.6 re-checked at every server start) so the factory's very first
    // resolution already runs on the new credential chain. A migration
    // failure never blocks the boot (F.4's spirit — the server keeps
    // running with the world as it was) and the log carries counts only,
    // never key material (F.5).
    if let Some(home) = home_dir.as_deref() {
        let config_path = home.join("config.toml");
        if let Ok(store) = mirri_credentials::CredentialStore::open(home) {
            match mirri_credentials::migrate_config_api_keys(&store, &config_path) {
                Ok(report) if !report.migrated.is_empty() || !report.cleaned.is_empty() => {
                    tracing::info!(
                        migrated = report.migrated.len(),
                        cleaned = report.cleaned.len(),
                        skipped = report.skipped.len(),
                        "credential migration"
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(%error, "credential migration failed (non-fatal)");
                }
            }
        }
    }
    let llm_factory: Arc<dyn mirri_core::LlmFactory> = Arc::new(ConfigAwareLlmFactory::new(
        Arc::clone(&config),
        home_dir.clone(),
    ));

    // The user-layer MCP pool is home-scoped, not root-scoped: probe it
    // through the default root's discovery once and share that pool across
    // every workspace runtime of this assembly (instead of each
    // `build_workspace_runtime` call receiving the fresh private pool
    // `load_mcp_manager_for_root` creates). The pool stays private to the
    // assembly — the NAPI composition root still swaps in the
    // process-level shared pool (`shared_user_mcp_pool_for_home`).
    let user_mcp_pool = load_mcp_manager_for_root(
        &workspace_root,
        mcp_home_base.as_deref(),
        home_dir.as_deref(),
        true,
    )
    .map(|(_, pool, _)| pool);

    // The default workspace is the passed-in root: its registry id is the
    // persistence bucket key, and its scope keeps the caller-supplied
    // `WorkspaceId` (W1 zero-drift — `workspace.*` id outputs stay
    // byte-identical; lazy workspaces derive their scope id from the
    // registry id).
    let default_registration_id = encode_work_dir_key(&workspace_root);
    // The home-level trust store (W7, design 08 §5 D6): which registered
    // workspaces the user has trusted. `None` without an injected home —
    // every non-default workspace then stays untrusted.
    let trust_store = WorkspaceTrustStore::open(home_dir.as_deref());
    let shared = Arc::new(SharedEngineServices {
        config: Arc::clone(&config),
        profile_manager: Arc::clone(&profile_manager),
        user_mcp_pool: user_mcp_pool.clone(),
        plugin_manager: plugin_manager.clone(),
        llm_factory: Arc::clone(&llm_factory),
        home_dir: home_dir.clone(),
        mcp_home_base: mcp_home_base.clone(),
        policy,
        skill_dirs,
        trust_store,
        default_registration_id: Some(default_registration_id.clone()),
        default_scope_id: Some(workspace_id),
        workspace_registry: std::sync::OnceLock::new(),
        metadata_store: std::sync::OnceLock::new(),
    });
    let builder: RuntimeBuilder = {
        let shared = Arc::clone(&shared);
        Arc::new(move |reg| build_workspace_runtime(reg, &shared))
    };
    // The index fallback lives on the registry itself (W3 unification): the
    // composition root installs the home and the registry lazily opens one
    // shared `<home>/session-index.db` connection — served to both the
    // `manager_for` fallback and the embedded server's own index reads, so
    // the two never open the file twice. Without a home there is nothing to
    // fall back to: resolution goes active table → bucket probe → not-found.
    let registry = Arc::new(WorkspaceRegistry::new(Arc::new(NoopResolver), builder));
    // The home is required (resolved above): every assembly roots its
    // session index and its metadata store in `<home>`.
    let home = home_dir
        .as_ref()
        .expect("the resolved home is always present at the wiring point");
    registry.install_session_index(home.clone());
    // Task 5b — the production metadata-store wiring: open (or create)
    // `<home>/session-index.db` through the startup-integrity check and
    // share the port with every runtime the builder later assembles.
    // A corrupt database or a future schema floor must NOT degrade to
    // the in-memory fixture (ADR-006: no silent degrade) — the engine
    // refuses to serve with a loud error instead.
    let db_path = home.join("session-index.db");
    match mirri_session_index::SessionIndex::open_with_check(&db_path) {
        Ok(mirri_session_index::OpenOutcome::Ok(index)) => {
            let _ = shared.metadata_store.set(Some(Arc::new(
                mirri_session_index::SessionIndexPort(index),
            )));
        }
        // The recovery pyramid (ADR-006 Task 11): a corrupt library
        // recovers instead of refusing to boot — tier 1 restores the
        // rolling backup, tier 2 half-rebuilds every row from the wire
        // logs. Only the recovery's own failure (or a future schema
        // floor) remains a loud boot refusal.
        Ok(mirri_session_index::OpenOutcome::Corrupt(diagnosis)) => {
            tracing::warn!(
                %diagnosis,
                db = %db_path.display(),
                "the session metadata database failed its startup integrity check; \
                 the recovery pyramid runs (backup restore, then half-rebuild)"
            );
            let sessions_root = home.join("v3_sessions");
            // `assemble` is sync (its callers may run before any Tokio
            // runtime exists — for_cwd); the recovery is async, so it
            // rides a dedicated current-thread runtime thread (the ACP
            // migration bridge precedent).
            let recovery_home = home.clone();
            let recovered = std::thread::spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .build()
                    .expect("building the recovery runtime")
                    .block_on(crate::embedded_server::recovery_v3::recover_library(
                        &recovery_home,
                        &sessions_root,
                    ))
            })
            .join()
            .unwrap_or_else(|join_error| {
                panic!("the recovery pyramid thread panicked: {join_error:?}")
            })
            .unwrap_or_else(|recovery_error| {
                panic!(
                    "the session metadata database {} is corrupt ({diagnosis}) and the \
                     recovery failed: {recovery_error}",
                    db_path.display()
                )
            });
            let _ = shared.metadata_store.set(Some(Arc::new(
                mirri_session_index::SessionIndexPort(recovered),
            )));
        }
        Ok(mirri_session_index::OpenOutcome::SchemaFloor { db_version }) => {
            panic!(
                "the session metadata database {} uses a newer format (v{db_version}) \
                 than this build understands; upgrade the app to open it",
                db_path.display()
            );
        }
        Err(error) => {
            panic!(
                "failed to open the session metadata database {}: {error}",
                db_path.display()
            );
        }
    }
    // The runtime builder reaches the registry through `shared` (the
    // registry is constructed after `shared`, so the Arc lands here the
    // moment it exists — before any runtime is ever built). This is what
    // lets every runtime's cron manager mount the SAME shared
    // `<home>/cron-index.db` connection.
    let _ = shared.workspace_registry.set(Arc::clone(&registry));

    // Restore the cross-engine registry rows (`<home>/workspaces.json`, the
    // TS engine's source of truth): register every row, leave every runtime
    // lazy except the default (D5). Rows whose root cannot be represented
    // or no longer exists are kept with a warning and never built until a
    // touch re-creates the root. A missing or corrupt registry file reads as
    // empty — the current single-workspace behavior.
    if let Some(home) = &home_dir {
        let registry_path = home.join(mirri_session_index::WORKSPACE_REGISTRY_FILENAME);
        for row in mirri_session_index::read_workspace_registry(&registry_path) {
            let Some(root) = row.root else {
                tracing::warn!(
                    workspace = %row.id,
                    "workspaces.json row has no root; skipping the row"
                );
                continue;
            };
            let root = PathBuf::from(root);
            if !root.is_dir() {
                tracing::warn!(
                    workspace = %row.id,
                    root = %root.display(),
                    "workspaces.json row root no longer exists; keeping the row \
                     registered but unbuilt"
                );
            }
            registry.register(WorkspaceRegistration {
                id: row.id,
                root,
                name: row.name,
                created_at: row.created_at.to_rfc3339(),
                last_opened_at: row.last_opened_at.to_rfc3339(),
            });
        }
    }

    // Register (touch when a restored row already carries the key) and
    // build the default workspace — the passed-in root, and the only
    // runtime assembled eagerly.
    let now = Utc::now().to_rfc3339();
    let default_registration = match registry.registration(&default_registration_id) {
        Some(mut existing) => {
            // Touch: keep the restored row's metadata, root the runtime at
            // the passed-in root (lexically equivalent to the row's by
            // construction — the id is the key — so the scope cwd stays
            // byte-identical to previous batches).
            existing.root = workspace_root.clone();
            existing.last_opened_at = now;
            registry.register(existing.clone());
            existing
        }
        None => {
            let registration = WorkspaceRegistration {
                id: default_registration_id.clone(),
                root: workspace_root.clone(),
                name: None,
                created_at: now.clone(),
                last_opened_at: now,
            };
            registry.register(registration.clone());
            registration
        }
    };
    let default_runtime = registry
        .register_and_build(default_registration)
        .expect("the default workspace runtime build cannot fail in this batch");
    assert!(
        registry.set_default(&default_registration_id),
        "the default workspace was just registered"
    );

    EngineParts {
        session_manager: Arc::clone(&default_runtime.manager),
        config,
        config_path,
        profile_manager: Arc::clone(&shared.profile_manager),
        mcp_manager: default_runtime.mcp_manager.clone(),
        user_mcp_pool: shared.user_mcp_pool.clone(),
        pending_mcp_configs: default_runtime.pending_mcp_configs.clone(),
        plugin_manager: shared.plugin_manager.clone(),
        llm_factory: Arc::clone(&shared.llm_factory),
        fetch_url_provider: Some(Arc::new(LocalFetchUrlProvider::new())),
        home_dir,
        skill_dirs: shared.skill_dirs.clone(),
        mcp_home_base,
        workspace_registry: registry,
        trust_store: shared.trust_store.clone(),
    }
}

/// The process-level engine services one assembly shares across every
/// workspace runtime (design §2 — the App-layer singletons): built once in
/// [`assemble`], handed to [`build_workspace_runtime`] per workspace.
pub(crate) struct SharedEngineServices {
    /// The live config snapshot (env-expanded, migrated), shared with
    /// hot-reload watchers and `config.get` / `config.set`.
    pub(crate) config: Arc<RwLock<ServerConfig>>,
    /// Agent profiles resolved from the user roots chain.
    pub(crate) profile_manager: Arc<ProfileManager>,
    /// The user-layer MCP pool — home-scoped, one per assembly, reused by
    /// every workspace runtime (see [`assemble`]).
    pub(crate) user_mcp_pool: Option<Arc<crate::user_mcp_pool::UserMcpPool>>,
    /// The plugin manager, when the Mirri home has a `plugins/` directory.
    pub(crate) plugin_manager: Option<Arc<mirri_plugin_host::PluginManager>>,
    /// The config-aware LLM factory powering engine turns.
    pub(crate) llm_factory: Arc<dyn mirri_core::LlmFactory>,
    /// The explicitly injected Mirri home directory, when one was given.
    pub(crate) home_dir: Option<PathBuf>,
    /// The MCP user-layer base the pool loads addressed.
    pub(crate) mcp_home_base: Option<PathBuf>,
    /// The workspace-scope policy (UntrustedDefault / TrustedFromConfig).
    pub(crate) policy: WorkspacePolicy,
    /// The SDK's explicit skill directories (v1/v2 `skillDirs`), applied to
    /// every built workspace.
    pub(crate) skill_dirs: Vec<PathBuf>,
    /// The home-level workspace trust store (W7, D6): consulted by
    /// [`build_workspace_runtime`] for every non-default workspace — the
    /// default (the serve / embedded pin root) is trusted by construction.
    pub(crate) trust_store: Option<WorkspaceTrustStore>,
    /// The default workspace's registry id — the one registration whose
    /// scope keeps the caller-supplied `WorkspaceId` (W1 zero-drift).
    pub(crate) default_registration_id: Option<String>,
    /// The caller-supplied scope id of the default workspace (see
    /// `default_registration_id`).
    pub(crate) default_scope_id: Option<WorkspaceId>,
    /// The registry the runtime builder mounts the shared cron-index
    /// connection from (the builder closure sees only `shared`, and the
    /// registry is constructed AFTER `shared` — it lands here the moment it
    /// exists, before any runtime is ever built).
    pub(crate) workspace_registry: std::sync::OnceLock<Arc<WorkspaceRegistry>>,
    /// The shared metadata store port over `<home>/session-index.db`
    /// (ADR-006's production wiring — Task 5b): every built runtime's
    /// `SessionManager` receives this through `with_metadata_store`, so
    /// the metadata rows of ALL workspaces live in the one home-level
    /// database (the same connection the index fallback and the listing
    /// read). Set by `assemble`, which resolves a home unconditionally
    /// (ADR-006 fix round 1: an unresolvable home is a hard error — the
    /// no-home shape is out of the product surface); the `None` state is
    /// only the pre-wiring instant and hand-built test `SharedEngineServices`.
    pub(crate) metadata_store:
        std::sync::OnceLock<Option<Arc<mirri_session_index::SessionIndexPort>>>,
}

impl SharedEngineServices {
    /// The shared session-index connection behind the metadata-store port
    /// (the same `<home>/session-index.db` every row write flows through),
    /// when the assembly wired one. The legacy-session migration's index
    /// backfill uses it — one connection, one file, no second open.
    pub(crate) fn session_index_conn(&self) -> Option<Arc<mirri_session_index::SessionIndex>> {
        self.metadata_store
            .get()?
            .as_ref()
            .map(|port| Arc::clone(&port.0))
    }
}

/// Replay the complete skill discovery sequence onto one built workspace
/// (B8), building the fresh tables off to the side and swapping them in
/// atomically (ADR-010 decision 3): scratch catalog → builtin bundle (TS
/// priority 0, ADR-B3) → plugin roots (TS priority 5, item G) → (explicit
/// skillDirs non-empty ? replacement user discovery : config
/// `extra_skill_dirs` → default roots + user-brand) → `replace_all` into
/// the live catalog → the user-level disabled-set seed (kept on the live
/// catalog — runtime toggle state, not discovered content).
///
/// The order IS the priority: `SkillCatalog` discovery is later-wins, so the
/// sequence runs **lowest priority first** — the builtin bundle (0), then
/// plugin (5), then extra (10), then the user chain (20: default
/// `$HOME/.agents/skills` + user-brand), then the workspace/project roots
/// (30). Every higher source therefore overrides a same-name plugin skill,
/// while a plugin-only skill still registers — the plugin floor above the
/// builtin bundle (0). The default-root branch carries the full four-channel
/// ADR-011 list (the three construction-time channels + the user brand dir)
/// deduped as one list by normalized path — same physical directory scanned
/// once, by the later channel (user tier), so a workspace rooted at $HOME
/// no longer produces per-skill shadow warnings from a re-scanned brand
/// dir. Note: when explicit skillDirs are non-empty they replace the
/// default discovery AND the config's extra dirs are skipped with them
/// (the v1/v2 SDK replacement contract — the TS v2
/// `ExtraFileSkillSource` is not suppressed there; the divergence is
/// recorded in skills-context §9.4); the plugin roots and the builtin
/// bundle ride both branches (builtins are App-level constants, not
/// user/project defaults — the TS `builtin(0)` semantics).
/// `resolve_extra_skill_dir` mirrors the TS `skillRoots.ts` path resolution
/// (`~` expansion / absolute / else relative to the workspace root).
///
/// The atomic swap (build-scratch + `replace_all`) replaced the historical
/// clear-then-discover replay so readers of the live catalog never observe
/// a mid-rescan empty table or a transient not-found — the write lock
/// covers only the swap instant. The assembly's first build runs the same
/// function (the first swap lands the same tables the historical
/// clear-then-discover sequence produced).
pub(crate) fn apply_skill_discovery(
    workspace: &WorkspaceScope,
    config: &ServerConfig,
    home_dir: Option<&Path>,
    plugin_manager: Option<&mirri_plugin_host::PluginManager>,
    skill_dirs: &[PathBuf],
) {
    // The scratch catalog the whole discovery sequence replays onto — the
    // live catalog stays readable until the single `replace_all` swap.
    let scratch = mirri_core::SkillCatalog::new();
    scratch.register_builtin(mirri_core::skill::builtin::builtin_skills());
    // Item G/J — plugin skill roots carry the TS `SkillPluginContext`
    // attribution (`{id, instructions}`); `discover_plugin` stamps it onto
    // every indexed skill so skill-prompt rendering can prepend the
    // `skillInstructions` and the session-start lookup can resolve by
    // plugin id. Discovered second, after the builtin registration: the TS
    // plugin priority is 5, below every other discovery channel (extra 10,
    // user 20, workspace 30) but above the builtin floor (0) — a later
    // higher-priority source overrides a same-name plugin skill, and the
    // plugin skill beats a same-name builtin.
    if let Some(plugin_mgr) = plugin_manager {
        for root in plugin_mgr.plugin_skill_roots() {
            let ctx = mirri_core::skill::SkillPluginContext {
                id: root.plugin.id.clone(),
                instructions: root.plugin.instructions.clone(),
            };
            if let Err(e) = scratch.discover_plugin(&root.path, &ctx) {
                tracing::warn!(%e, ?root.path, "failed to discover plugin skills");
            }
        }
    }
    if !skill_dirs.is_empty() {
        for dir in skill_dirs {
            if let Err(e) = scratch.discover(dir, SkillSource::User) {
                tracing::warn!(%e, dir = %dir.display(), "failed to discover explicit skill dirs");
            }
        }
    } else {
        for entry in config.extra_skill_dirs.iter().flatten() {
            let dir = resolve_extra_skill_dir(entry, workspace.cwd());
            if let Err(e) = scratch.discover(&dir, SkillSource::Plugin) {
                tracing::warn!(%e, dir = %dir.display(), "extra skill dir discovery failed");
            }
        }
        // ADR-011 + ADR-010 decision 3: the full four-channel default-root
        // list (the three construction-time channels + the user brand dir),
        // deduped by normalized path as ONE list — the replay must see the
        // same channel set construction saw, plus the brand dir it owns. A
        // workspace rooted at $HOME makes the project brand dir collide
        // with the user brand dir; the dedupe scans the directory once (as
        // the user channel, the later-wins keeper) instead of once per
        // channel with per-skill shadow warnings. The channels land on the
        // SCRATCH catalog — the atomic-swap rescan's staging table.
        let mut channels = default_skill_root_channels(workspace.cwd());
        if let Ok(mirri_home) = resolve_mirri_home(home_dir) {
            channels.push(SkillRootChannel {
                source: SkillSource::User,
                dir: mirri_home.join("skills"),
            });
        }
        for channel in dedupe_skill_roots(channels) {
            if let Err(e) = scratch.discover(&channel.dir, channel.source) {
                tracing::warn!(path = %channel.dir.display(), %e, "default skill root discovery failed");
            }
        }
    }
    // The atomic swap (ADR-010 decision 3): the scratch tables land in the
    // live catalog under single write locks — the write covers only the
    // swap instant. The builtin-content table swaps with the skills table
    // so a builtin dropped by the replay stops serving its stale body.
    let skills = workspace.skills();
    skills.replace_all(scratch.snapshot(), scratch.snapshot_builtin_contents());
    // The user-level disabled-set seed stays on the LIVE catalog: it is
    // runtime toggle state (seeded from config, mutated through
    // `set_enabled`), not discovered content — `replace_all` deliberately
    // leaves it untouched.
    skills.set_user_disabled(config.disabled_skills.clone().unwrap_or_default());
}

/// TS `skillRoots.ts` alignment: `~` (and `~/…`) expansion via `$HOME` /
/// absolute path / anything else relative to the workspace root. Exposed
/// (as the `_pub` twin) for the skill watcher's listen-set computation —
/// the same resolution rule the discovery replay uses for
/// `extra_skill_dirs`.
fn resolve_extra_skill_dir(entry: &str, workspace_root: &Path) -> PathBuf {
    resolve_extra_skill_dir_from(entry, workspace_root, std::env::var_os("HOME"))
}

/// The pure resolver behind [`resolve_extra_skill_dir`], parameterised
/// over the os-home so `~` expansion is testable without mutating the
/// process environment (the `resolve_*_from` convention of
/// `mirri-os/src/home.rs`): tests pass the fake home directly instead of
/// pinning `$HOME`.
fn resolve_extra_skill_dir_from(
    entry: &str,
    workspace_root: &Path,
    os_home: Option<std::ffi::OsString>,
) -> PathBuf {
    if let Some(home) = os_home {
        if entry == "~" {
            return PathBuf::from(home);
        }
        if let Some(rest) = entry.strip_prefix("~/") {
            return Path::new(&home).join(rest);
        }
    }
    let path = PathBuf::from(entry);
    if path.is_absolute() {
        path
    } else {
        workspace_root.join(path)
    }
}

/// The cross-module twin of [`resolve_extra_skill_dir`] — the skill
/// watcher's listen-set computation needs the same resolution rule
/// outside this module. Behaviorally identical by delegation.
pub(crate) fn resolve_extra_skill_dir_pub(
    entry: &str,
    workspace_root: &Path,
) -> PathBuf {
    resolve_extra_skill_dir(entry, workspace_root)
}

/// Build one workspace's runtime (scope + manager + per-root workspace-pool
/// MCP manager) — the registry's lazy builder for registered rows and the
/// eager default build in [`assemble`].
///
/// Scope construction is policy-dependent (the same derivation the old
/// `assemble` performed); every per-workspace piece moves here: skill
/// discovery (user-brand, explicit skill-dirs replacement, plugin skills)
/// and the per-root MCP discovery. The user layer's pool is the shared one
/// from [`SharedEngineServices`] — the private pool
/// [`load_mcp_manager_for_root`] returns is discarded (W1: only the default
/// runtime's configs are ever consumed, through `EngineParts`; W7 connects
/// the rest).
///
/// Infallible for every current code path (all construction is
/// non-fallible); the `Result` is the plan's signature, reserved for the
/// W7 trust gate.
pub(crate) fn build_workspace_runtime(
    reg: &WorkspaceRegistration,
    shared: &SharedEngineServices,
) -> anyhow::Result<WorkspaceRuntime> {
    // The W7 trust gate (design `08-multi-workspace.md` §5 D6 — the
    // dynamic successor of W2's N4 transition): the default workspace (the
    // serve / embedded pin root the user launched the process in) is
    // trusted by construction and keeps the assembly's policy; every other
    // registered workspace's trust comes from the home-level trust store —
    // an unknown id answers untrusted (D6 default). There is no
    // "registered but policy undecided" window: the store's answer decides
    // the scope policy AND whether the project layers (MCP config +
    // AGENTS.md project section) load at all.
    let is_default = shared.default_registration_id.as_deref() == Some(reg.id.as_str());
    let trusted = is_default
        || shared
            .trust_store
            .as_ref()
            .is_some_and(|store| store.is_trusted(&reg.id));
    let policy = if is_default {
        shared.policy
    } else if trusted {
        WorkspacePolicy::TrustedFromConfig
    } else {
        WorkspacePolicy::UntrustedDefault
    };
    let scope_config = match policy {
        WorkspacePolicy::UntrustedDefault => WorkspaceScopeConfig::default(),
        WorkspacePolicy::TrustedFromConfig => {
            let read = shared.config.read().unwrap_or_else(|e| e.into_inner());
            let permission_mode = permission_mode_from_config(
                read.permission
                    .as_ref()
                    .and_then(|permission| permission.mode.as_deref()),
            );
            WorkspaceScopeConfig {
                trust_level: WorkspaceTrustLevel::Trusted,
                permission_mode,
                hooks: read.hooks.clone().unwrap_or_default(),
                extra_directories: Vec::new(),
                load_project_agents_md: true,
            }
        }
    };
    // The load gate's second half (the scope config above is the first):
    // an untrusted workspace's agents never see the project AGENTS.md
    // sections (D6). The default workspace always keeps them (the
    // historical behavior of both policies — `UntrustedDefault` scopes
    // under ACP/NAPI still inject their pinned root's instructions).
    let mut scope_config = scope_config;
    scope_config.load_project_agents_md = trusted;

    // Item E — merge the enabled plugins' hooks into the workspace hook
    // set, AFTER the user config's hooks: the TS
    // `ExternalHooksRunnerService.load` indexes `[...configured,
    // ...pluginHooks]` (config first, plugin second), and the workspace
    // engine's registration order is the trigger order. The hooks arrive
    // already decorated by the manager (cwd = plugin root, env =
    // `MIRRICODE_HOME`/`MIRRI_PLUGIN_ROOT`), so the engine inherits the
    // runtime facts. Zero-IO contract 6: the plugin data crosses as plain
    // values assembled here. The pre-merge base is captured on the runtime
    // (item C4) so a plugin reload can re-derive the merged set without a
    // rebuild.
    let base_hooks = scope_config.hooks.clone();
    if let Some(plugin_manager) = &shared.plugin_manager {
        scope_config.hooks.extend(plugin_manager.enabled_hooks());
    }

    // The default workspace keeps the caller-supplied scope id (see
    // [`SharedEngineServices`]); every other workspace derives its scope id
    // from the registry id (the cross-engine identity).
    let scope_id = if shared.default_registration_id.as_deref() == Some(reg.id.as_str()) {
        shared
            .default_scope_id
            .clone()
            .unwrap_or_else(|| WorkspaceId::new(&reg.id))
    } else {
        WorkspaceId::new(&reg.id)
    };
    let workspace = Arc::new(WorkspaceScope::new(
        scope_id,
        reg.root.clone(),
        scope_config,
    ));

    // Replay the full skill discovery sequence for this workspace (B8):
    // the builtin bundle, default roots + user-brand (or the explicit
    // skillDirs replacement), the config's `extra_skill_dirs`, plugin
    // roots, and the user-level disabled-set seed. `WorkspaceScope::new`
    // already ran the default-root discovery; `apply_skill_discovery`
    // replays the complete sequence onto a scratch catalog and swaps it
    // in atomically (ADR-010 decision 3), so the result is identical plus
    // the config-driven pieces.
    {
        let config_snapshot = shared
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        apply_skill_discovery(
            &workspace,
            &config_snapshot,
            shared.home_dir.as_deref(),
            shared.plugin_manager.as_deref(),
            &shared.skill_dirs,
        );
    }

    // With an injected home, session persistence roots under the home
    // (ADR-006-R2: the rust engine's own `v3_sessions/` layout) instead of
    // the workspace-local `<root>/.mirri/v3_sessions`.
    // (The `None` arm below is the test-only shape — `assemble` resolves
    // a home unconditionally since ADR-006 fix round 1; the direct
    // `build_workspace_runtime` surface keeps the fallback so hand-built
    // fixtures with no shared services stay constructible.)
    let mut session_manager = match &shared.home_dir {
        Some(home) => SessionManager::with_persistence_dir(
            workspace,
            home.join("v3_sessions"),
            Arc::new(mirri_os::LocalFs::new()),
        ),
        None => SessionManager::new(workspace),
    };
    // Task 5b — the metadata-store port injection: the runtime's manager
    // writes its metadata rows through the shared home-level SQLite port
    // (ADR-006's "唯一事实源" landing). The rows file under the
    // REGISTRATION id (the routing seam's index fallback resolves
    // row → workspace_id → registration; a cwd-derived key would
    // mis-file whenever the two differ).
    if let Some(Some(store)) = shared.metadata_store.get() {
        session_manager = session_manager
            .with_metadata_store(Arc::clone(store) as Arc<_>)
            .with_metadata_workspace_id(reg.id.clone());
    }
    let session_manager = session_manager.with_server_config(Arc::clone(&shared.config));
    let session_manager = Arc::new(session_manager);

    // ADR-006 fix round 2 (R2a) — the legacy workspace-local session
    // migration: a pre-ADR-006 engine rooted its sessions at
    // `<root>/.mirri/v3_sessions/` (the no-home ACP / direct-constructor
    // shape); the home-rooted assembly must adopt them or they vanish
    // from every listing. On the FIRST build of a workspace whose
    // legacy root exists, every session directory moves into the home's
    // corresponding bucket and an index row is minted from its
    // `state.json` (still a legal read point during the migration
    // window) so the routing seam resolves it. Idempotent by
    // construction: the source directory is removed once emptied, so a
    // later build sees no legacy root and skips (a partially-migrated
    // set re-runs over the remainder only). Best-effort: a failure logs
    // and never blocks the runtime build.
    if let Some(home) = shared.home_dir.as_deref()
        && let Some(index) = shared.session_index_conn()
    {
        migrate_workspace_local_sessions(&reg.root, home, &index);
    }

    // Per-root workspace-pool MCP discovery: the manager (project layers +
    // plugin servers) and the layered config view. The returned private
    // user pool is discarded — the assembly shares `shared.user_mcp_pool`.
    // An untrusted workspace's discovery loads the USER LAYER ONLY (the
    // D6 red line: its project layer files are never read) — the manager
    // still exists so a later `mcp.config.set` reconcile has a pool to
    // address, but it starts and stays empty of project servers.
    let (mcp_manager, pending_mcp_configs) = match load_mcp_manager_for_root(
        &reg.root,
        shared.mcp_home_base.as_deref(),
        shared.home_dir.as_deref(),
        trusted,
    ) {
        Some((manager, _private_user_pool, layered)) => (Some(manager), Some(layered)),
        None => (None, None),
    };

    // (Plugin-provided skills were already discovered by
    // `apply_skill_discovery` above — it replays the complete sequence
    // including plugin roots and the disabled-set seed.)

    // The workspace's cron manager (stage-2 T5, ADR-020 since): one per
    // runtime, attached only on the home-rooted layout — a workspace-local
    // persistence root (no injected home) keeps `None` and the scheduler's
    // workspace arm skips that runtime. Home-rooted runtimes mount the
    // REGISTRY's shared cron-index connection (the same `Arc<CronIndex>`
    // every runtime and the `cron.listAll` face read), so all writes land
    // in the one `<home>/cron-index.db`.
    let cron = shared.home_dir.as_ref().and_then(|home| {
        shared
            .workspace_registry
            .get()
            .and_then(|registry| registry.shared_cron_index())
            .map(|index| {
                std::sync::Arc::new(WorkspaceCronManager::open_with_index(
                    &reg.id,
                    &home.join("v3_sessions").join(&reg.id),
                    // The CronPersistence seam: wrap the shared connection
                    // in the embedded-layer adapter (the carrier crate stays
                    // out of mirri-core's dependency tree — zero-IO gate).
                    std::sync::Arc::new(crate::cron_persistence::SqliteCronPersistence(
                        index,
                    )) as std::sync::Arc<dyn mirri_core::cron::CronPersistence + Send + Sync>,
                ))
            })
    });

    Ok(WorkspaceRuntime {
        workspace_id: reg.id.clone(),
        scope: Arc::clone(session_manager.workspace()),
        manager: session_manager,
        mcp_manager,
        pending_mcp_configs,
        trusted: std::sync::atomic::AtomicBool::new(trusted),
        mcp_connect_spawned: std::sync::atomic::AtomicBool::new(false),
        base_hooks,
        cron,
    })
}

/// Migrate a workspace's legacy workspace-local sessions into the home
/// root (ADR-006 fix round 2 / R2a): a pre-ADR-006 engine rooted its
/// sessions at `<root>/.mirri/v3_sessions/<bucket>/<id>/` (the no-home
/// ACP / direct-constructor shape); the home-rooted assembly adopts them
/// by moving every session directory into the home's corresponding
/// bucket and minting an index row per session from its `state.json`
/// (still a legal read point during the migration window — Task 7
/// retires it for the new write paths, not for reading legacy files).
///
/// Idempotent by construction: a bucket that no longer exists under the
/// legacy root contributes nothing, and the legacy root itself is
/// removed once every bucket under it is empty — a second run sees no
/// legacy root and skips. A session that already exists in the home's
/// bucket is NOT overwritten (the home's copy wins — it is the newer
/// source of truth); the legacy copy is removed.
///
/// Best-effort: any failure logs and skips that session; the runtime
/// build never blocks on the migration. The bucket's `cron/` manifest
/// directory is deliberately left in place (the session migration moves
/// sessions only; the cron domain's own one-time import — Task 8, the
/// registry's `import_legacy_cron_once` — reads the frozen archive where
/// the legacy engine wrote it).
fn migrate_workspace_local_sessions(
    workspace_root: &Path,
    home: &Path,
    index: &mirri_session_index::SessionIndex,
) {
    let legacy_root = workspace_root.join(".mirri").join("v3_sessions");
    let Ok(buckets) = std::fs::read_dir(&legacy_root) else {
        return; // no legacy root → nothing to migrate (the idempotent skip)
    };
    let home_root = home.join("v3_sessions");
    let mut rows: Vec<mirri_session_index::SessionRow> = Vec::new();
    for bucket in buckets.flatten() {
        let bucket_path = bucket.path();
        if !bucket_path.is_dir() {
            continue;
        }
        let Some(bucket_key) = bucket.file_name().to_str().map(str::to_string) else {
            continue;
        };
        let Ok(sessions) = std::fs::read_dir(&bucket_path) else {
            continue;
        };
        for session in sessions.flatten() {
            let session_path = session.path();
            if !session_path.is_dir() {
                continue;
            }
            // The workspace cron manifest lives alongside the session
            // directories (`<bucket>/cron/tasks.json`): it is NOT a
            // session — moving it would steal the frozen archive out from
            // under the cron domain's own one-time import (Task 8), and a
            // target-side collision would DELETE it (the duplicate-drop
            // below fires on any existing target). Left in place, the
            // cron import reads it where the legacy engine wrote it.
            if session.file_name() == "cron" {
                continue;
            }
            let Some(session_id) = session.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let target = home_root.join(&bucket_key).join(&session_id);
            if target.exists() {
                // The home already carries this session (a previous
                // partial run, or a same-id re-create): the home's copy
                // is the source of truth — drop the legacy duplicate.
                if let Err(error) = std::fs::remove_dir_all(&session_path) {
                    tracing::warn!(
                        session_id = %session_id,
                        %error,
                        "legacy session migration: removing the duplicated legacy copy failed"
                    );
                }
                continue;
            }
            if let Err(error) = move_dir(&session_path, &target) {
                tracing::warn!(
                    session_id = %session_id,
                    from = %session_path.display(),
                    to = %target.display(),
                    %error,
                    "legacy session migration: moving the session failed; it stays put"
                );
                continue;
            }
            // Mint the index row from the migrated session's state.json
            // (the metadata read is async — a dedicated current-thread
            // runtime, the `skill_reload::mark_live_sessions` bridge
            // shape, safe in every caller context).
            // Mint the index row from the migrated session's state.json
            // (the metadata read is async — a dedicated current-thread
            // runtime, the `skill_reload::mark_live_sessions` bridge
            // shape, safe in every caller context).
            let row = {
                let persistence = mirri_core::SessionPersistence::new(
                    home_root.clone(),
                    Arc::new(mirri_os::LocalFs::new()),
                )
                .with_workspace_key(bucket_key.clone());
                let session_id = session_id.clone();
                let bucket_key = bucket_key.clone();
                let workspace_root = workspace_root.to_path_buf();
                std::thread::spawn(move || {
                    tokio::runtime::Builder::new_current_thread()
                        .build()
                        .expect("legacy session migration runtime")
                        .block_on(async move {
                            let sid = mirri_core::SessionId::new(&session_id);
                            let info = persistence.read_persisted_info(&sid).await?;
                            Some(mirri_session_index::SessionRow {
                                id: session_id,
                                workspace_id: bucket_key,
                                cwd: info.metadata.work_dir.clone().or_else(|| {
                                    Some(workspace_root.to_string_lossy().into_owned())
                                }),
                                title: Some(info.metadata.title.clone()),
                                created_at: info.metadata.created_at,
                                updated_at: info.metadata.updated_at,
                                archived: info.metadata.archived,
                                tags: info.metadata.tags.iter().cloned().collect(),
                                custom: None,
                                last_prompt: info.metadata.last_prompt.clone(),
                                state_mtime: None,
                                archived_at: info.archived_at,
                                compressed: info.compressed,
                                compressed_at: info.compressed_at,
                                pinned: false,
                                pinned_at: None,
                                pin_color: None,
                                last_used_at: info.metadata.last_used_at,
                                pre_compressed_size: info
                                    .pre_compressed_size
                                    .map(|size| size as i64),
                            })
                        })
                })
                .join()
                .unwrap_or_else(|panic| {
                    tracing::warn!(
                        panic = ?panic,
                        "legacy session migration: the state.json reader panicked"
                    );
                    None
                })
            };
            match row {
                Some(row) => rows.push(row),
                None => tracing::warn!(
                    session_id = %session_id,
                    "legacy session migration: the session moved but its state.json \
                     could not be read (no index row minted; the disk scan still \
                     finds it)"
                ),
            }
        }
        // A moved-clean bucket disappears; a skipped-session bucket
        // stays for the next run (the idempotent remainder).
        let _ = std::fs::remove_dir(&bucket_path); // fails when non-empty — intended
    }
    // The legacy root disappears when its every bucket did (the natural
    // completion marker — no sentinel file).
    let _ = std::fs::remove_dir(&legacy_root); // fails when non-empty — intended
    if !rows.is_empty() {
        if let Err(error) = index.bulk_upsert_sessions(&rows) {
            tracing::warn!(
                %error,
                count = rows.len(),
                "legacy session migration: the index backfill failed (the sessions \
                 moved; the rows rebuild on the next reconcile)"
            );
        } else {
            tracing::info!(
                count = rows.len(),
                "legacy session migration: workspace-local sessions adopted into the home root"
            );
        }
    }
}

/// Move a directory, falling back to a recursive copy + delete when the
/// rename crosses a filesystem boundary (`EXDEV`).
fn move_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to.parent().expect("the target has a parent"))?;
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(_) => {
            copy_dir_recursive(from, to)?;
            std::fs::remove_dir_all(from)
        }
    }
}

fn copy_dir_recursive(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let source = entry.path();
        let target = to.join(entry.file_name());
        if source.is_dir() {
            copy_dir_recursive(&source, &target)?;
        } else {
            std::fs::copy(&source, &target)?;
        }
    }
    Ok(())
}

/// Resolve the Mirri home directory for server construction.
///
/// An explicitly injected home (the NAPI `createEngine(homeDir)` path) wins;
/// `None` falls back to the env-based [`mirri_core::config::mirri_home_dir`]
/// resolution (`MIRRICODE_HOME`, then `$HOME/.mirri-code`) — the exact
/// pre-injection behavior. Explicit injection instead of `set_var` keeps
/// parallel Tokio tests free of process-wide env races.
fn resolve_mirri_home(injected: Option<&Path>) -> Result<PathBuf, mirri_core::EngineError> {
    match injected {
        Some(home) => Ok(home.to_path_buf()),
        None => mirri_core::config::mirri_home_dir(),
    }
}

/// The MCP OAuth token store directory — `<mirri home>/mcp-tokens` (the
/// `mirri_mcp::oauth::TokenStore` layout), with the same home resolution
/// [`resolve_mirri_home`] applies. `None` when no home can be resolved:
/// OAuth servers then behave as if no tokens were cached.
pub fn mcp_oauth_token_dir(injected_home: Option<&Path>) -> Option<PathBuf> {
    resolve_mirri_home(injected_home)
        .ok()
        .map(|home| home.join("mcp-tokens"))
}

/// Auto-discover and load `config.toml` from the Mirri home directory,
/// returning the loaded config (or defaults) and the path it came from.
///
/// The path is ALWAYS resolved when the home directory is — the
/// well-known `<home>/config.toml`, whether or not the file exists yet
/// (the TypeScript `resolveConfigPath` contract): a fresh environment's
/// first `config.set`/`provider.set` write then creates the file instead
/// of failing with "no config file path". `load_without_env` returns the
/// default config for the missing file, so a patch merges onto defaults.
///
/// Never fails: a missing HOME, or an unparseable file all fall back to
/// `ServerConfig::default()` with a logged warning, so server startup is
/// never blocked by configuration problems. An unparseable file yields
/// `None` — the config surface then reports it cannot write (the user
/// must fix the file first, matching the TS strict-read write path).
fn load_config_from_home(
    injected_home: Option<&Path>,
) -> (Arc<RwLock<ServerConfig>>, Option<PathBuf>) {
    match resolve_mirri_home(injected_home) {
        Ok(home_dir) => {
            let config_path = home_dir.join("config.toml");
            match load_config_with_migration(&config_path, &home_dir) {
                Ok(config) => {
                    tracing::info!(
                        path = %config_path.display(),
                        "loaded config.toml"
                    );
                    (Arc::new(RwLock::new(config)), Some(config_path))
                }
                Err(error) => {
                    tracing::warn!(
                        path = %config_path.display(),
                        %error,
                        "failed to parse config.toml; using defaults"
                    );
                    if config_path.exists() {
                        // A file exists but cannot be parsed: writes must
                        // not clobber it — report no path so the write
                        // surfaces demand a fix first.
                        (Arc::new(RwLock::new(ServerConfig::default())), None)
                    } else {
                        // No file: the well-known path is writable (the
                        // fresh-environment boot).
                        (
                            Arc::new(RwLock::new(ServerConfig::default())),
                            Some(config_path),
                        )
                    }
                }
            }
        }
        Err(error) => {
            tracing::warn!(%error, "cannot locate mirri home dir; using default config");
            (Arc::new(RwLock::new(ServerConfig::default())), None)
        }
    }
}

/// Load `config.toml` with the upgraded M5 load order:
/// `load_without_env` → `migrate_config` → `expand_env_vars` → `apply_env`.
///
/// This unifies `mirri serve`'s historical order (`load_without_env` →
/// `migrate_config` → `apply_env`, no `${VAR}` expansion) with the embedded
/// funnel's (`load_without_env` + expand + env, no migration): serve gains
/// `${VAR}` expansion, and the ACP/NAPI funnel gains the marker-file-driven
/// one-shot config migrations (idempotent — a migration already marked in
/// `<home>/<marker-file>` is skipped on the next boot). The parse runs
/// **without** environment overrides first because a config migration may
/// rewrite config.toml, and the rewrite must persist exactly what the file
/// held — never env-merged values (e.g. an injected `MIRRI_API_KEY` must
/// not be baked into config.toml). `${VAR}` expansion runs before
/// `apply_env` so that a whole-field env override still wins over an inline
/// `${...}` reference in the file.
fn load_config_with_migration(
    config_path: &Path,
    home_dir: &Path,
) -> mirri_core::EngineResult<ServerConfig> {
    let mut config = ServerConfig::load_without_env(config_path, &mirri_os::LocalSyncFs)?;

    // One-shot config migrations (marker-file driven, mirroring the
    // TypeScript mechanism). Runs only on a successfully parsed file, never
    // blocks startup: a failure is logged and the marker stays absent, so
    // the migration is retried on the next start.
    if let Err(error) = migrate_config(&mut config, config_path, home_dir, &mirri_os::LocalSyncFs) {
        tracing::warn!(
            %error,
            "config migration failed; using the in-memory migrated config for this \
             run, migration will be retried on next start"
        );
    }

    config.expand_env_vars();

    if let Err(error) = config.apply_env() {
        tracing::warn!(?error, "failed to apply environment model config");
    }
    Ok(config)
}

/// Build a [`ProfileManager`] from the user-level agent profile roots plus
/// the enabled plugins' agent roots — the same loading `mirri serve`
/// performs at startup, extended with the plugin contribution (item H).
///
/// The user roots chain mirrors the TS `userAgentRoots` (`packages/agent-profile/
/// src/roots.ts`): the brand directory `<mirri-home>/agents` and the generic
/// `<$HOME>/.agents/agents` (each probed for existence, brand first), plus
/// the legacy `<mirri-home>/profiles/` read-only for existing `.toml`/
/// `.json` users — the directory the profile CRUD *cleans* after writing a
/// `.md` (R4-1: the write channel persists under `<mirri-home>/agents/`).
/// Plugin roots (the TS `pluginAgentRoots` channel) join the same discovery
/// pass tagged [`ProfileSource::Plugin`] (priority 5) — the mixed pass
/// decides same-name winners by source priority, so a user profile always
/// beats a plugin profile of the same name (item H).
/// Project roots (`.mirri-code/agents` under a project root) are R3/R4
/// scope: the rust boot path has no project-root notion feeding profile
/// discovery yet, and none is invented here.
///
/// The config's `disabled_agents` key no longer seeds anything (R3-3 — the
/// file-level `enabled` flag is the sole enablement source); a config still
/// carrying a non-empty key gets the one-time deprecation warning here, the
/// single boot path for both `mirri serve` and the embedded server.
///
/// Never fails: an unresolvable home directory or an unreadable root yields
/// an empty (or partially loaded) manager, so server startup is never
/// blocked. Caveat: the generic root probes the real `$HOME` (not the
/// injected Mirri home) exactly like the TS engine — hosts that need
/// hermetic isolation must ensure `$HOME` carries no `.agents/agents`.
fn load_profile_manager(
    config: &ServerConfig,
    injected_home: Option<&Path>,
    plugin_manager: Option<&Arc<mirri_plugin_host::PluginManager>>,
) -> Arc<ProfileManager> {
    if let Some(notice) = config.disabled_agents_deprecation_notice() {
        tracing::warn!("{notice}");
    }
    let manager = Arc::new(ProfileManager::new());
    let Ok(home_dir) = resolve_mirri_home(injected_home) else {
        return manager;
    };
    // The write channel requires the agents directory (R4-1: the CRUD
    // persists `<home>/agents/<name>.md`).
    let agents_dir = home_dir.join("agents");
    if let Err(error) = std::fs::create_dir_all(&agents_dir) {
        tracing::warn!(
            dir = %agents_dir.display(),
            %error,
            "cannot create agents directory; agent profiles are disabled"
        );
        return manager;
    }
    let roots = profile_discovery_roots(
        injected_home,
        plugin_manager.map(|manager| manager.as_ref()),
    );
    match manager.load_from_source_roots(&roots, &mirri_os::LocalSyncFs) {
        Ok(loaded) => {
            tracing::info!(?roots, loaded, "agent profiles loaded");
        }
        Err(error) => {
            tracing::warn!(
                ?roots,
                %error,
                "failed to load agent profiles; subagent profiles are unavailable"
            );
        }
    }
    manager
}

/// The discovery roots for the profile manager: the user chain (brand
/// agents dir, generic `~/.agents/agents` — only those that exist, the TS
/// `userAgentRoots` — plus the legacy `<home>/profiles`), in priority
/// order, followed by the enabled plugins' agent roots tagged
/// [`ProfileSource::Plugin`] (item H). The single root list both the boot
/// (`load_profile_manager`) and the plugin-reload refresh
/// (`EmbeddedServer`'s profile resync, item C4) use, so the same-name
/// merge never depends on which path populated the manager.
pub(crate) fn profile_discovery_roots(
    injected_home: Option<&Path>,
    plugin_manager: Option<&mirri_plugin_host::PluginManager>,
) -> Vec<mirri_core::profile::ProfileSourceRoot> {
    profile_discovery_roots_from(injected_home, std::env::var_os("HOME"), plugin_manager)
}

/// The pure resolver behind [`profile_discovery_roots`], parameterised
/// over the os-home (the `resolve_*_from` convention of
/// `mirri-os/src/home.rs`) so the generic `~/.agents/agents` root is
/// testable without pinning `$HOME`.
pub(crate) fn profile_discovery_roots_from(
    injected_home: Option<&Path>,
    os_home: Option<std::ffi::OsString>,
    plugin_manager: Option<&mirri_plugin_host::PluginManager>,
) -> Vec<mirri_core::profile::ProfileSourceRoot> {
    let Ok(home_dir) = resolve_mirri_home(injected_home) else {
        return Vec::new();
    };
    let os_home = os_home.map(PathBuf::from);
    let mut roots: Vec<mirri_core::profile::ProfileSourceRoot> =
        mirri_core::profile::user_profile_root_candidates(&home_dir, os_home.as_deref())
            .into_iter()
            .filter(|root| root.is_dir())
            .map(Into::into)
            .collect();
    roots.push(mirri_core::profile::legacy_profile_root(&home_dir).into());
    if let Some(plugin_mgr) = plugin_manager {
        roots.extend(plugin_mgr.plugin_agent_roots().into_iter().map(|root| {
            mirri_core::profile::ProfileSourceRoot {
                path: root.path,
                source: mirri_core::profile::ProfileSource::Plugin,
            }
        }));
    }
    roots
}

/// Auto-discover MCP server configs from the Mirri home directory and the
/// given project root, and return both pools' managers plus the layered
/// configs without spawning any async work: the workspace-pool manager
/// (project layers + plugin servers), a fresh **private** user pool (the
/// composition root may swap in a shared one), and the pool-routing config
/// view.
///
/// Returns `None` when no MCP config is found or the home directory cannot
/// be resolved — the server runs fine without MCP.
///
/// The actual `connect_all` is **not** spawned here because this function
/// may be called outside a Tokio runtime (via
/// [`EmbeddedServer::for_workspace_root`](crate::EmbeddedServer::for_workspace_root)
/// → `EngineBridge::new()` where the runtime is created *after* the server
/// is constructed). Instead, the configs are stored in
/// [`EngineParts::pending_mcp_configs`] and the connection is lazily
/// spawned on the first engine call, which is guaranteed to run inside the
/// runtime.
fn load_mcp_manager_for_root(
    project_root: &Path,
    mcp_home_base: Option<&Path>,
    injected_home: Option<&Path>,
    load_project_layers: bool,
) -> Option<(
    Arc<mirri_mcp::McpConnectionManager>,
    Arc<crate::user_mcp_pool::UserMcpPool>,
    mirri_mcp::config::LayeredMcpConfig,
)> {
    // The MCP crate's global layer is `<base>/.mirri-code/mcp.json`, and
    // `base` is contractually `$HOME`. An explicit base (`mirri serve` passes
    // `$HOME`) wins; otherwise see [`resolve_mcp_home_base`].
    let home_dir = match mcp_home_base {
        Some(base) => base.to_path_buf(),
        None => resolve_mcp_home_base(injected_home)?,
    };
    // The W7 trust gate: an untrusted workspace discovers the user layer
    // only — its project layer files are never read (design 08 §5 D6),
    // so nothing from an untrusted tree can reach a connection.
    let layered = if load_project_layers {
        mirri_mcp::config::load_mcp_config_layered(&home_dir, project_root, project_root).ok()?
    } else {
        mirri_mcp::config::load_mcp_config_user_layered(&home_dir).ok()?
    };
    // The pools are attached even when the discovered config is empty: a
    // later `mcp.config.set` reconcile (spec §4.3) connects into the same
    // pools without a restart — an empty pool costs nothing (no
    // connections, no spawns), a missing one would strand config writes
    // until the next process start.
    //
    // Both pools' managers share the OAuth token store (plans/07 T8):
    // `<mirri home>/mcp-tokens`, resolved through the same chain the
    // config base used.
    let oauth_token_dir = mcp_oauth_token_dir(injected_home);
    let manager = match &oauth_token_dir {
        Some(dir) => Arc::new(mirri_mcp::McpConnectionManager::with_oauth_token_dir(
            dir.clone(),
        )),
        None => Arc::new(mirri_mcp::McpConnectionManager::new()),
    };
    Some((
        manager,
        Arc::new(crate::user_mcp_pool::UserMcpPool::with_oauth_token_dir(
            oauth_token_dir,
        )),
        layered,
    ))
}

/// Resolve the base directory whose `/.mirri-code/mcp.json` forms the MCP
/// user layer, with the fallback chain [`load_mcp_manager_for_root`]
/// applies when no explicit base is given:
///
/// 1. the injected home (the temp-dir convention of NAPI/ACP tests —
///    `<home>/.mirri-code/mcp.json`), then
/// 2. `$HOME` verbatim (the actual user home), then
/// 3. the resolved Mirri home as a last resort when even `$HOME` is
///    unset (`MIRRICODE_HOME`, else `$HOME/.mirri-code`).
///
/// Skipping (2) would produce `<home>/.mirri-code/.mirri-code/mcp.json`
/// for real (un-injected) users — the user's actual file would never be
/// found. Exported so composition roots (the NAPI bridge) can key the
/// shared user pool by the same home the loader reads.
pub fn resolve_mcp_home_base(injected_home: Option<&Path>) -> Option<PathBuf> {
    resolve_mcp_home_base_from(injected_home, std::env::var_os("HOME"))
}

/// The pure resolver behind [`resolve_mcp_home_base`], parameterised over
/// the os-home (the `resolve_*_from` convention of `mirri-os/src/home.rs`)
/// so the `$HOME`-verbatim tier is testable without pinning the process
/// env. `None` when no home can be resolved from either input.
pub fn resolve_mcp_home_base_from(
    injected_home: Option<&Path>,
    os_home: Option<std::ffi::OsString>,
) -> Option<PathBuf> {
    injected_home
        .map(Path::to_path_buf)
        .or_else(|| os_home.map(PathBuf::from))
        .or_else(|| resolve_mirri_home(None).ok())
}

/// Auto-discover plugins from the Mirri home directory's `plugins/`
/// folder, load their manifests, and return a [`PluginManager`].
///
/// Returns `None` when the home directory cannot be resolved or the
/// `plugins/` directory does not exist — the server runs fine without
/// plugins.
fn load_plugin_manager_for_home(
    injected_home: Option<&Path>,
) -> Option<Arc<mirri_plugin_host::PluginManager>> {
    let home_dir = resolve_mirri_home(injected_home).ok()?;
    let plugins_dir = home_dir.join("plugins");
    if !plugins_dir.exists() {
        return None;
    }

    let manager = mirri_plugin_host::PluginManager::new(plugins_dir);
    match manager.load_all() {
        Ok(()) => {
            let plugins = manager.list();
            if plugins.is_empty() {
                tracing::debug!("no plugins found in plugins directory");
            } else {
                tracing::info!(plugin_count = plugins.len(), "plugins loaded");
            }
        }
        // Even on load failure we return the manager — an empty manager
        // still allows `plugin.install` to recover, and `plugin.list` will
        // simply return an empty list.
        Err(error) => {
            tracing::warn!(%error, "failed to load plugins; continuing without plugin support");
        }
    }
    Some(Arc::new(manager))
}

/// Map a `config.toml` permission mode string (`"manual"`, `"yolo"`,
/// `"auto"`) onto a [`PermissionMode`].
///
/// A missing value falls back to the safe default (Manual) silently — the
/// default path is the common case, not an error. An unrecognised value also
/// falls back to Manual but is logged, so a stale or hand-edited config can
/// never escalate permissions silently.
///
/// Moved here from `mirri-cli`'s `serve` bootstrap when the scope
/// derivation ported into [`assemble`] (M5).
fn permission_mode_from_config(mode: Option<&str>) -> PermissionMode {
    match mode {
        Some("manual") => PermissionMode::Manual,
        Some("auto") => PermissionMode::Auto,
        Some("yolo") => PermissionMode::Yolo,
        None => PermissionMode::Manual,
        Some(unrecognised) => {
            tracing::warn!(
                mode = ?unrecognised,
                "config.toml has an unrecognised permission mode; defaulting to manual"
            );
            PermissionMode::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mirri_core::HookEventType;
    use std::sync::{Arc, Mutex};

    /// should expand `~` and `~/…` extra-skill-dir entries against $HOME
    /// (B8: the TS skillRoots path-resolution parity) — through the
    /// injected-home variant, so no process-env pin is involved.
    #[test]
    fn should_expand_home_prefixes_when_resolving_extra_skill_dirs() {
        let root = PathBuf::from("/ws/root");
        let fake_home = tempfile::tempdir().expect("fake home tempdir");
        let injected = Some(std::ffi::OsString::from(fake_home.path()));
        let resolved = [
            resolve_extra_skill_dir_from("~/pdf-skills", &root, injected.clone()),
            resolve_extra_skill_dir_from("~", &root, injected),
        ];

        // Then `~/x` expands under the home and bare `~` is the home itself
        assert_eq!(resolved[0], fake_home.path().join("pdf-skills"));
        assert_eq!(resolved[1], fake_home.path());
        // And absolute entries pass through, relative ones anchor at the root
        assert_eq!(
            resolve_extra_skill_dir_from("/abs/skills", &root, None),
            PathBuf::from("/abs/skills")
        );
        assert_eq!(
            resolve_extra_skill_dir_from("rel-skills", &root, None),
            root.join("rel-skills")
        );
    }

    #[test]
    fn should_build_default_scope_when_policy_is_untrusted_default() {
        // Given a throwaway workspace and home with no config.toml
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let home = tempfile::tempdir().expect("home tempdir");

        // When the engine parts are assembled with the funnel policy
        let parts = assemble(
            WorkspaceId::new("policy-test"),
            workspace.path().to_path_buf(),
            Some(home.path().to_path_buf()),
            None,
            WorkspacePolicy::UntrustedDefault,
            Vec::new(),
        );

        // Then the workspace scope carries the safe default: Untrusted,
        // Manual, no hooks — exactly what for_workspace_root built before
        let scope = parts.session_manager.workspace();
        assert_eq!(
            scope.config().trust_level,
            WorkspaceTrustLevel::Untrusted,
            "UntrustedDefault must stay untrusted"
        );
        assert_eq!(
            scope.config().permission_mode,
            PermissionMode::Manual,
            "UntrustedDefault must stay manual"
        );
        assert!(
            scope.config().hooks.is_empty(),
            "UntrustedDefault must derive no hooks from absent config"
        );
    }

    #[test]
    fn should_build_new_registrations_with_untrusted_default_when_the_assembly_policy_is_trusted() {
        // Given an assembly built with the `mirri serve` policy
        // (TrustedFromConfig)
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let home = tempfile::tempdir().expect("home tempdir");
        let parts = assemble(
            WorkspaceId::new("policy-test"),
            workspace.path().to_path_buf(),
            Some(home.path().to_path_buf()),
            None,
            WorkspacePolicy::TrustedFromConfig,
            Vec::new(),
        );
        let registry = parts.workspace_registry;

        // When a NEW workspace is registered and built on first touch
        let other_root = tempfile::tempdir().expect("other workspace tempdir");
        let now = chrono::Utc::now().to_rfc3339();
        let id = mirri_core::encode_work_dir_key(other_root.path());
        registry.register(WorkspaceRegistration {
            id: id.clone(),
            root: other_root.path().to_path_buf(),
            name: None,
            created_at: now.clone(),
            last_opened_at: now,
        });
        let runtime = registry
            .ensure_built(&id)
            .expect("the row is registered")
            .expect("the build succeeds");

        // Then the untrusted workspace (no trust-store entry — D6 default
        // false) builds Untrusted with the project layers gated OFF, while
        // the default pin keeps the assembly's Trusted policy
        assert_eq!(
            runtime.scope.config().trust_level,
            WorkspaceTrustLevel::Untrusted,
            "a workspace with no trust entry must build untrusted (D6 default)"
        );
        assert!(
            !runtime.scope.config().load_project_agents_md,
            "the untrusted workspace's project AGENTS.md sections must be gated off"
        );
        assert!(!runtime.trusted(), "the runtime records the gate verdict");
        let default = registry
            .default_runtime()
            .expect("the default runtime is built");
        assert_eq!(
            default.scope.config().trust_level,
            WorkspaceTrustLevel::Trusted,
            "the default pin keeps the serve policy"
        );
        assert!(
            default.scope.config().load_project_agents_md,
            "the default pin keeps the historical AGENTS.md injection"
        );
    }

    // Given an assembly whose home carries a trust entry for a registered
    // workspace, when that workspace is lazily built, then the W7 dynamic
    // policy applies: the scope derives Trusted (like the serve pin) and
    // the project layers load (superseding W2's
    // UntrustedDefault-for-everything-new transition).
    #[test]
    fn should_build_a_trusted_registration_with_the_serve_policy_and_full_layers() {
        // Given a serve-shaped assembly and a second workspace the trust
        // store marks trusted
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let home = tempfile::tempdir().expect("home tempdir");
        let other_root = tempfile::tempdir().expect("other workspace tempdir");
        let other_key = mirri_core::encode_work_dir_key(other_root.path());
        crate::workspace_trust::WorkspaceTrustStore::open(Some(home.path()))
            .expect("the store opens")
            .set_trusted(&other_key, true)
            .expect("the trust grant persists");

        let parts = assemble(
            WorkspaceId::new("trust-test"),
            workspace.path().to_path_buf(),
            Some(home.path().to_path_buf()),
            None,
            WorkspacePolicy::TrustedFromConfig,
            Vec::new(),
        );
        let registry = parts.workspace_registry;
        let now = chrono::Utc::now().to_rfc3339();
        registry.register(WorkspaceRegistration {
            id: other_key.clone(),
            root: other_root.path().to_path_buf(),
            name: None,
            created_at: now.clone(),
            last_opened_at: now,
        });

        // When the trusted workspace is built on first touch
        let runtime = registry
            .ensure_built(&other_key)
            .expect("the row is registered")
            .expect("the build succeeds");

        // Then it carries the trusted treatment: a Trusted scope, the
        // project AGENTS.md gate open, and the recorded verdict
        assert_eq!(
            runtime.scope.config().trust_level,
            WorkspaceTrustLevel::Trusted
        );
        assert!(runtime.scope.config().load_project_agents_md);
        assert!(runtime.trusted());
    }

    #[test]
    fn should_load_md_profiles_from_the_brand_agents_root_when_assembling() {
        // Given a Mirri home whose brand `agents/` directory holds a `.md`
        // agent file and whose legacy `profiles/` holds a `.toml` one
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let home = tempfile::tempdir().expect("home tempdir");
        std::fs::create_dir_all(home.path().join("agents")).expect("create agents dir");
        std::fs::create_dir_all(home.path().join("profiles")).expect("create profiles dir");
        std::fs::write(
            home.path().join("agents").join("reviewer.md"),
            "---\nname: reviewer\ndescription: Reviews code\n---\nYou review code.",
        )
        .expect("write reviewer.md");
        std::fs::write(
            home.path().join("profiles").join("planner.toml"),
            "name = \"planner\"\nmodel = \"claude-sonnet-4-20250514\"\n",
        )
        .expect("write planner.toml");
        // The profile roots chain probes the real `$HOME` for the generic
        // `~/.agents/agents` root; pin it to an isolated empty home so the
        // test cannot pick up the actual user's profiles (hermetic). The
        // workspace env lock serializes the pin against the other
        // HOME-reading tests (the env is process-global).
        let isolated_home = tempfile::tempdir().expect("isolated home tempdir");
        let env_name = "HOME";
        let _home_lock = mirri_test_support::env::lock(); // env_lock: workspace-wide HOME pin guard
        let previous = std::env::var_os(env_name);
        // SAFETY: the HOME env var is restored immediately after assembly,
        // and no other bootstrap test reads it (they all inject homes), so
        // the short window cannot cross-contaminate parallel tests.
        unsafe { std::env::set_var(env_name, isolated_home.path()) };

        // When the engine parts are assembled (the `mirri serve` funnel)
        let parts = assemble(
            WorkspaceId::new("agents-root-test"),
            workspace.path().to_path_buf(),
            Some(home.path().to_path_buf()),
            None,
            WorkspacePolicy::TrustedFromConfig,
            Vec::new(),
        );
        // SAFETY: restore the previous value; see the set_var above.
        // env_lock: restore under the workspace env lock guard above
        match previous {
            Some(value) => unsafe { std::env::set_var(env_name, value) },
            None => unsafe { std::env::remove_var(env_name) },
        }

        // Then profiles from both roots are registered — the `.md` through
        // the brand agents root, the legacy `.toml` through profiles/
        assert!(
            parts
                .profile_manager
                .get("reviewer")
                .is_some_and(|profile| profile.system_prompt.as_deref() == Some("You review code.")),
            "the .md profile from <mirri-home>/agents must load"
        );
        assert!(
            parts.profile_manager.get("planner").is_some(),
            "the legacy .toml profile from <mirri-home>/profiles must keep loading"
        );
    }

    #[test]
    fn should_build_trusted_scope_from_config_when_policy_is_trusted_from_config() {
        // Given a Mirri home whose config.toml sets permission mode and hooks
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let home = tempfile::tempdir().expect("home tempdir");
        std::fs::write(
            home.path().join("config.toml"),
            "[permission]\nmode = \"auto\"\n\n[[hooks]]\nevent = \"SessionStart\"\ncommand = \"echo start\"\n",
        )
        .expect("write config.toml");

        // When the engine parts are assembled with the serve policy
        let parts = assemble(
            WorkspaceId::new("serve-policy-test"),
            workspace.path().to_path_buf(),
            Some(home.path().to_path_buf()),
            None,
            WorkspacePolicy::TrustedFromConfig,
            Vec::new(),
        );

        // Then the workspace scope derives Trusted + the configured
        // permission mode and hooks — exactly what `mirri serve` built before
        let scope = parts.session_manager.workspace();
        assert_eq!(scope.config().trust_level, WorkspaceTrustLevel::Trusted);
        assert_eq!(scope.config().permission_mode, PermissionMode::Auto);
        assert_eq!(
            scope.config().hooks.len(),
            1,
            "the SessionStart hook must ride"
        );
        assert_eq!(scope.config().hooks[0].event, HookEventType::SessionStart);
    }

    #[test]
    fn should_build_trusted_scope_when_serve_boots_without_config() {
        // Given a throwaway home with no config.toml (serve's fallback path)
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let home = tempfile::tempdir().expect("home tempdir");

        // When assembled with the serve policy
        let parts = assemble(
            WorkspaceId::new("serve-no-config"),
            workspace.path().to_path_buf(),
            Some(home.path().to_path_buf()),
            None,
            WorkspacePolicy::TrustedFromConfig,
            Vec::new(),
        );

        // Then the scope is still Trusted with the safe Manual mode — the
        // config-derived parts fall back to engine defaults, exactly like
        // serve's missing-config handling
        let scope = parts.session_manager.workspace();
        assert_eq!(scope.config().trust_level, WorkspaceTrustLevel::Trusted);
        assert_eq!(scope.config().permission_mode, PermissionMode::Manual);
    }

    /// Given an assembly whose home carries a user agent profile and an
    /// installed plugin whose agent root defines the same name plus a
    /// plugin-only name (item H), when the engine parts are assembled, then
    /// both are loaded — the plugin one with `plugin` provenance (priority
    /// 5) and the same-name user profile beating it (priority 10).
    #[tokio::test]
    async fn should_load_plugin_agent_profiles_with_plugin_provenance_when_assembling() {
        // Given a home with a user agent profile and an installed plugin
        // whose agents dir defines the same name plus a plugin-only name
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let home = tempfile::tempdir().expect("home tempdir");
        let isolated_home = tempfile::tempdir().expect("isolated $HOME tempdir");

        std::fs::create_dir_all(home.path().join("agents")).expect("agents dir");
        std::fs::write(
            home.path().join("agents").join("planner.md"),
            "---\nname: planner\ndescription: User planner\n---\nYou plan for the user.",
        )
        .expect("user planner.md");

        let source_dir = tempfile::tempdir().expect("plugin source tempdir");
        std::fs::create_dir_all(source_dir.path().join("agents")).expect("plugin agents dir");
        std::fs::write(
            source_dir.path().join("agents").join("planner.md"),
            "---\nname: planner\ndescription: Plugin planner\n---\nYou plan for the plugin.",
        )
        .expect("plugin planner.md");
        std::fs::write(
            source_dir.path().join("agents").join("reviewer.md"),
            "---\nname: reviewer\ndescription: Reviews code\n---\nYou review code.",
        )
        .expect("plugin reviewer.md");
        std::fs::write(
            source_dir.path().join("mirri.plugin.json"),
            serde_json::json!({
                "name": "demo-plugin",
                "agents": ["./agents"],
            })
            .to_string(),
        )
        .expect("plugin manifest");

        let plugins_dir = home.path().join("plugins");
        std::fs::create_dir_all(&plugins_dir).expect("plugins dir");
        let manager = mirri_plugin_host::PluginManager::new(plugins_dir);
        manager
            .install(&format!("local:{}", source_dir.path().display()))
            .await
            .expect("plugin install");

        // When assembled over that home (the assembly's own plugin manager
        // load_all picks the installed plugin back up, and the profile
        // manager discovers the plugin agent roots) — under the crate
        // $HOME pin (sync only; the async install ran unpinned)
        let _home_lock = mirri_test_support::env::lock(); // env_lock: workspace-wide HOME pin guard
        let previous_home = std::env::var_os("HOME");
        // SAFETY: restored after assembly (see the pin at line ~1190).
        unsafe { std::env::set_var("HOME", isolated_home.path()) };
        let parts = assemble(
            WorkspaceId::new("plugin-profiles-test"),
            workspace.path().to_path_buf(),
            Some(home.path().to_path_buf()),
            None,
            WorkspacePolicy::UntrustedDefault,
            Vec::new(),
        );
        // SAFETY: restore the previous $HOME (see the set_var above).
        // env_lock: restore under the workspace env lock guard above
        match previous_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        drop(_home_lock);

        // Then the same-name user profile wins and the plugin-only profile
        // registers with `plugin` provenance
        let pm = &parts.profile_manager;
        let planner = pm.get("planner").expect("planner profile present");
        assert_eq!(
            planner.system_prompt.as_deref().unwrap_or_default(),
            "You plan for the user.",
            "the user (10) profile must beat the same-name plugin (5) profile"
        );
        assert_eq!(
            pm.provenance("planner")
                .expect("planner provenance present")
                .source,
            mirri_core::profile::ProfileSource::User
        );
        let reviewer = pm.get("reviewer").expect("reviewer profile present");
        assert_eq!(
            reviewer.system_prompt.as_deref().unwrap_or_default(),
            "You review code."
        );
        let reviewer_provenance = pm
            .provenance("reviewer")
            .expect("reviewer provenance present");
        assert_eq!(
            reviewer_provenance.source,
            mirri_core::profile::ProfileSource::Plugin,
            "the plugin-only profile must register with the plugin source"
        );
        assert_eq!(reviewer_provenance.source.as_str(), "plugin");
    }

    #[test]
    fn should_apply_migrations_then_expand_env_when_config_is_loaded() {
        // Given a config.toml carrying a migration candidate (`max` effort)
        // and a model id referencing an environment variable
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let home = tempfile::tempdir().expect("home tempdir");
        std::fs::write(
            home.path().join("config.toml"),
            "[thinking]\neffort = \"max\"\n\n[model]\nid = \"m5-${MIRRI_M5_BOOTSTRAP_EXPANSION}\"\n",
        )
        .expect("write config.toml");
        let env_name = "MIRRI_M5_BOOTSTRAP_EXPANSION";
        // SAFETY: unique var name owned by this test; no other code reads
        // it — set under the workspace env lock anyway (the iron rules:
        // no bare env mutation even for uniquely named vars).
        let mut _env = mirri_test_support::env::EnvGuard::new();
        _env.set(env_name, "expanded");

        // When the engine parts are assembled through the funnel path
        let parts = assemble(
            WorkspaceId::new("migration-test"),
            workspace.path().to_path_buf(),
            Some(home.path().to_path_buf()),
            None,
            WorkspacePolicy::UntrustedDefault,
            Vec::new(),
        );
        drop(_env); // restore the uniquely named var (EnvGuard Drop)

        // Then the one-shot migration ran BEFORE env expansion: the loaded
        // config carries the migrated effort AND the expanded model id
        let config = parts.config.read().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            config.thinking.as_ref().and_then(|t| t.effort.as_deref()),
            Some("high"),
            "migration must rewrite thinking.effort max -> high"
        );
        assert_eq!(
            config.model.as_ref().map(|m| m.id.as_str()),
            Some("m5-expanded"),
            "env-var expansion must run after migration and before apply_env"
        );

        // And the marker file was persisted, so a second boot skips the
        // migration (idempotent) — the same marker serve's boot writes
        // (`MIGRATIONS_MARKER_FILE` is private to mirri-core; the literal
        // name is the stable contract `migrate_config` persists).
        assert!(
            home.path().join("migrations-effort.json").exists(),
            "the migration marker must be recorded under the home"
        );
    }

    #[test]
    fn should_discover_global_mcp_layer_under_base_home_when_assembling_with_serve_home() {
        // Given a home whose `/.mirri-code/mcp.json` declares a server, and
        // serve-style assembly pointing the MCP base at that home itself
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let base_home = tempfile::tempdir().expect("base home tempdir");
        let mirri_home = base_home.path().join(".mirri-code");
        std::fs::create_dir_all(&mirri_home).expect("create mirri home");
        std::fs::write(
            mirri_home.join("mcp.json"),
            r#"{"mcpServers": {"test": {"command": "npx", "args": ["-y", "server"]}}}"#,
        )
        .expect("write mcp.json");

        // When the engine parts are assembled with an explicit MCP base
        // (`mirri serve` passes $HOME) and the Mirri home next to it
        let parts = assemble(
            WorkspaceId::new("mcp-base-test"),
            workspace.path().to_path_buf(),
            Some(mirri_home.clone()),
            Some(base_home.path().to_path_buf()),
            WorkspacePolicy::TrustedFromConfig,
            Vec::new(),
        );

        // Then the global MCP layer is discovered at <base>/.mirri-code/
        // mcp.json — not double-nested under the Mirri home itself
        assert!(
            parts.mcp_manager.is_some(),
            "the serve global layer must resolve under the base home, not the Mirri home"
        );
        assert!(
            parts.pending_mcp_configs.is_some(),
            "the discovered server configs must be queued for lazy connect"
        );
    }

    #[test]
    fn should_discover_global_mcp_layer_under_real_home_when_assembling_without_injected_home() {
        // Given a temp `$HOME` whose `/.mirri-code/mcp.json` declares a
        // server, and NO injected home nor explicit MCP base (the
        // `for_workspace_root`-style funnel: ACP/NAPI without a home)
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let real_home = tempfile::tempdir().expect("real home tempdir");
        let mirri_home = real_home.path().join(".mirri-code");
        std::fs::create_dir_all(&mirri_home).expect("create mirri home");
        std::fs::write(
            mirri_home.join("mcp.json"),
            r#"{"mcpServers": {"test": {"command": "npx", "args": ["-y", "server"]}}}"#,
        )
        .expect("write mcp.json");
        let env_name = "HOME";
        let _home_lock = mirri_test_support::env::lock(); // env_lock: workspace-wide HOME pin guard
        let previous = std::env::var_os(env_name);
        // SAFETY: the HOME env var is restored immediately after assembly,
        // and the workspace env lock serializes this window against the
        // other HOME-reading tests (the env is process-global and the
        // harness runs the binary's tests in parallel).
        unsafe { std::env::set_var(env_name, real_home.path()) };

        // When the engine parts are assembled with no injected home and no
        // explicit MCP base
        let parts = assemble(
            WorkspaceId::new("mcp-real-home-test"),
            workspace.path().to_path_buf(),
            None,
            None,
            WorkspacePolicy::UntrustedDefault,
            Vec::new(),
        );
        // SAFETY: restore the previous value; see the set_var above.
        // env_lock: restore under the workspace env lock guard above
        match previous {
            Some(value) => unsafe { std::env::set_var(env_name, value) },
            None => unsafe { std::env::remove_var(env_name) },
        }

        // Then the global MCP layer is discovered at <$HOME>/.mirri-code/
        // mcp.json — the user's actual file — not double-nested under
        // <$HOME>/.mirri-code/.mirri-code/mcp.json. The seeded server is
        // the proof: the user layer must actually CONTAIN it (a
        // double-nested resolve finds nothing and leaves the user layer
        // empty — the fix-round-1 regression this test guards against).
        let pending = parts
            .pending_mcp_configs
            .as_ref()
            .expect("the discovered server configs must be queued for lazy connect");
        assert!(
            pending.user.contains_key("test"),
            "the user layer must load the seeded server from \
             <$HOME>/.mirri-code/mcp.json, got user keys {:?} \
             (double-nested resolve reads <$HOME>/.mirri-code/.mirri-code/mcp.json \
             and finds nothing)",
            pending.user.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn should_replace_default_skill_discovery_when_assembling_with_explicit_skill_dirs() {
        // Given a workspace with a project skill (`<root>/.agents/skills`) and
        // an explicit skill directory carrying a different skill
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let home = tempfile::tempdir().expect("home tempdir");
        let project_skill = workspace.path().join(".agents/skills/project-skill");
        std::fs::create_dir_all(&project_skill).expect("create project skill dir");
        std::fs::write(project_skill.join("SKILL.md"), "# Project\n").expect("write SKILL.md");
        let custom_root = tempfile::tempdir().expect("custom skills tempdir");
        let custom_skill = custom_root.path().join("custom-skill");
        std::fs::create_dir_all(&custom_skill).expect("create custom skill dir");
        std::fs::write(custom_skill.join("SKILL.md"), "# Custom\n").expect("write SKILL.md");
        // Plant a user-brand skill under the injected home too (`<home>/
        // skills`, the TS USER_BRAND_DIRS parity): the replacement must
        // wipe it along with the project/user defaults. No HOME pinning
        // needed (and none is allowed — the MCP-real-home test below owns
        // the HOME window): the replacement clears the catalog AFTER the
        // default discovery ran, so whatever the ambient discovery indexed
        // is wiped regardless.
        let brand_skill = home.path().join("skills/brand-skill");
        std::fs::create_dir_all(&brand_skill).expect("create brand skill dir");
        std::fs::write(brand_skill.join("SKILL.md"), "# Brand\n").expect("write SKILL.md");

        // When the engine parts are assembled with the explicit skill dirs
        let parts = assemble(
            WorkspaceId::new("skill-dirs-test"),
            workspace.path().to_path_buf(),
            Some(home.path().to_path_buf()),
            None,
            WorkspacePolicy::UntrustedDefault,
            vec![custom_root.path().to_path_buf()],
        );

        // Then the catalog carries the explicit skill (as the user source —
        // the v1/v2 SDK `skillDirs` contract) plus the builtin bundle
        // (App-level constants the replacement does not touch — design 13
        // §3.3): the project skill and the user-brand skill the default
        // discovery indexed are both replaced
        let catalog = parts.session_manager.workspace().skills();
        let names: Vec<String> = catalog.list().into_iter().map(|s| s.name).collect();
        let mut expected: Vec<String> = mirri_core::skill::builtin::builtin_skills()
            .into_iter()
            .map(|skill| skill.def.name)
            .collect();
        expected.push("custom-skill".into());
        expected.sort();
        assert_eq!(names, expected);
        assert_eq!(
            catalog.get("custom-skill").unwrap().source,
            SkillSource::User
        );
        assert!(catalog.get("project-skill").is_none());
        assert!(catalog.get("brand-skill").is_none());
    }

    #[test]
    fn should_discover_user_brand_skill_under_mirri_home_when_assembling() {
        // Given a Mirri home carrying a user-brand skill (`<home>/skills`,
        // the TS scanner's USER_BRAND_DIRS parity) and no explicit skill dirs
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let home = tempfile::tempdir().expect("home tempdir");
        let brand_skill = home.path().join("skills/brand-skill");
        std::fs::create_dir_all(&brand_skill).expect("create brand skill dir");
        std::fs::write(brand_skill.join("SKILL.md"), "# Brand\n").expect("write SKILL.md");

        // When the engine parts are assembled with the injected home
        let parts = assemble(
            WorkspaceId::new("user-brand-test"),
            workspace.path().to_path_buf(),
            Some(home.path().to_path_buf()),
            None,
            WorkspacePolicy::UntrustedDefault,
            Vec::new(),
        );

        // Then the user-brand skill is indexed as a user skill
        let catalog = parts.session_manager.workspace().skills();
        assert_eq!(
            catalog
                .get("brand-skill")
                .expect("brand skill discovered")
                .source,
            SkillSource::User
        );
    }

    #[test]
    fn should_keep_default_skill_discovery_when_skill_dirs_are_empty() {
        // Given a workspace with a project skill and no explicit skill dirs
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let home = tempfile::tempdir().expect("home tempdir");
        let project_skill = workspace.path().join(".agents/skills/project-skill");
        std::fs::create_dir_all(&project_skill).expect("create project skill dir");
        std::fs::write(project_skill.join("SKILL.md"), "# Project\n").expect("write SKILL.md");
        // No HOME pinning (the MCP-real-home test below owns the HOME
        // window), so the real user's ~/.agents/skills may ride along —
        // the assertions are inclusive, not exhaustive.

        // When the engine parts are assembled with no skill dirs
        let parts = assemble(
            WorkspaceId::new("skill-dirs-empty-test"),
            workspace.path().to_path_buf(),
            Some(home.path().to_path_buf()),
            None,
            WorkspacePolicy::UntrustedDefault,
            Vec::new(),
        );

        // Then the default discovery stands: the project skill is indexed
        // with its project source
        let catalog = parts.session_manager.workspace().skills();
        let names: Vec<String> = catalog.list().into_iter().map(|s| s.name).collect();
        assert!(
            names.contains(&"project-skill".to_string()),
            "the project skill must stay discovered, got {names:?}"
        );
        assert_eq!(
            catalog.get("project-skill").unwrap().source,
            SkillSource::Project
        );
    }

    #[test]
    fn should_parse_permission_mode_when_config_value_is_known() {
        // Given config permission mode strings in each supported spelling
        let known_modes = [
            ("manual", PermissionMode::Manual),
            ("auto", PermissionMode::Auto),
            ("yolo", PermissionMode::Yolo),
        ];

        // When mapping each known mode string
        for (mode_str, expected) in known_modes {
            // Then the matching PermissionMode is produced
            assert_eq!(permission_mode_from_config(Some(mode_str)), expected);
        }
    }

    #[test]
    fn should_default_to_manual_when_config_permission_mode_is_unknown_or_absent() {
        // Given an absent or unrecognised permission mode string
        // When mapping it
        // Then the safe default (Manual) is used
        assert_eq!(permission_mode_from_config(None), PermissionMode::Manual);
        assert_eq!(
            permission_mode_from_config(Some("ask-everything")),
            PermissionMode::Manual
        );
    }

    // Given a home whose workspaces.json registers three workspace rows
    // (two foreign, one exactly the assembled root), when the engine parts
    // are assembled with that home, then all three rows are registered but
    // only the default workspace runtime is built (D5 lazy build) — and the
    // parts' session manager is that default runtime's manager.
    #[test]
    fn should_register_every_row_and_build_only_the_default_when_workspaces_json_has_three_rows() {
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let home = tempfile::tempdir().expect("home tempdir");
        let other_a = tempfile::tempdir().expect("workspace a tempdir");
        let other_b = tempfile::tempdir().expect("workspace b tempdir");
        let default_key = encode_work_dir_key(workspace.path());
        let now = "2026-09-11T00:00:00.000Z";
        std::fs::write(
            home.path()
                .join(mirri_session_index::WORKSPACE_REGISTRY_FILENAME),
            serde_json::json!({
                "version": 1,
                "workspaces": {
                    "wd_row_a_aaaaaaaaaaaa": {
                        "root": other_a.path(),
                        "name": "row-a",
                        "created_at": now,
                        "last_opened_at": now,
                    },
                    "wd_row_b_bbbbbbbbbbbb": {
                        "root": other_b.path(),
                        "name": "row-b",
                        "created_at": now,
                        "last_opened_at": now,
                    },
                    default_key.clone(): {
                        "root": workspace.path(),
                        "name": "default",
                        "created_at": now,
                        "last_opened_at": now,
                    },
                },
                "deleted_workspace_ids": [],
            })
            .to_string(),
        )
        .expect("write workspaces.json");

        // When the engine parts are assembled with that home
        let parts = assemble(
            WorkspaceId::new("restore-test"),
            workspace.path().to_path_buf(),
            Some(home.path().to_path_buf()),
            None,
            WorkspacePolicy::UntrustedDefault,
            Vec::new(),
        );

        // Then all three rows are registered, only the default is built,
        // and the parts' session manager IS the default runtime's manager
        let registry = &parts.workspace_registry;
        assert_eq!(registry.registrations().len(), 3);
        assert_eq!(registry.built_count(), 1);
        let default_runtime = registry.default_runtime().expect("default built");
        assert_eq!(default_runtime.workspace_id, default_key);
        assert!(Arc::ptr_eq(
            &parts.session_manager,
            &default_runtime.manager
        ));
    }

    // Given a workspaces.json row whose root directory no longer exists,
    // when the engine parts are assembled with that home, then the row is
    // kept in the registry with a warning (registered-but-unbuilt — the
    // plan's "skip+warn+保留行") and startup proceeds with only the default
    // built.
    #[test]
    fn should_keep_a_row_whose_root_is_gone_when_restoring_from_workspaces_json() {
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let home = tempfile::tempdir().expect("home tempdir");
        let gone_root = home.path().join("gone-workspace");
        let now = "2026-09-11T00:00:00.000Z";
        std::fs::write(
            home.path()
                .join(mirri_session_index::WORKSPACE_REGISTRY_FILENAME),
            serde_json::json!({
                "version": 1,
                "workspaces": {
                    "wd_gone_cccccccccccc": {
                        "root": gone_root,
                        "name": "gone",
                        "created_at": now,
                        "last_opened_at": now,
                    },
                },
                "deleted_workspace_ids": [],
            })
            .to_string(),
        )
        .expect("write workspaces.json");

        // When the engine parts are assembled with that home
        let parts = assemble(
            WorkspaceId::new("restore-gone-test"),
            workspace.path().to_path_buf(),
            Some(home.path().to_path_buf()),
            None,
            WorkspacePolicy::UntrustedDefault,
            Vec::new(),
        );

        // Then the vanished-root row stays registered (never built), the
        // default workspace is still built, and the session manager keeps
        // the assembled root
        let registry = &parts.workspace_registry;
        let kept = registry
            .registration("wd_gone_cccccccccccc")
            .expect("the vanished-root row must be kept");
        assert_eq!(kept.root, gone_root);
        assert_eq!(registry.built_count(), 1);
        assert_eq!(parts.session_manager.workspace().cwd(), workspace.path());
    }

    // Given a corrupt workspaces.json, when the engine parts are assembled
    // with that home, then the read degrades to empty and the pre-W1
    // single-workspace shape is unchanged: exactly the default workspace
    // registered and built for the assembled root.
    #[test]
    fn should_fall_back_to_a_single_workspace_when_workspaces_json_is_corrupt() {
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let home = tempfile::tempdir().expect("home tempdir");
        std::fs::write(
            home.path()
                .join(mirri_session_index::WORKSPACE_REGISTRY_FILENAME),
            "{ not json",
        )
        .expect("write workspaces.json");

        // When the engine parts are assembled with that home
        let parts = assemble(
            WorkspaceId::new("restore-corrupt-test"),
            workspace.path().to_path_buf(),
            Some(home.path().to_path_buf()),
            None,
            WorkspacePolicy::UntrustedDefault,
            Vec::new(),
        );

        // Then exactly the default workspace is registered and built, with
        // the assembled root as its cwd
        let registry = &parts.workspace_registry;
        assert_eq!(registry.registrations().len(), 1);
        assert_eq!(registry.built_count(), 1);
        assert_eq!(
            registry.default_runtime().unwrap().workspace_id,
            encode_work_dir_key(workspace.path())
        );
        assert_eq!(parts.session_manager.workspace().cwd(), workspace.path());
    }

    /// Write an installable plugin source declaring `skills: ["./skills"]`
    /// with one `SKILL.md` per `(name, frontmatter description)` entry —
    /// the fixture shape for the skill-priority replay tests.
    fn write_plugin_skill_source(source_dir: &tempfile::TempDir, skills: &[(&str, &str)]) {
        for (name, description) in skills {
            std::fs::create_dir_all(source_dir.path().join("skills").join(name))
                .expect("skill dir must be creatable");
            std::fs::write(
                source_dir.path().join("skills").join(name).join("SKILL.md"),
                format!("---\ndescription: {description}\n---\n# {name}\n"),
            )
            .expect("SKILL.md must be writable");
        }
        std::fs::write(
            source_dir.path().join("mirri.plugin.json"),
            serde_json::json!({
                "name": "demo-plugin",
                "skills": ["./skills"],
            })
            .to_string(),
        )
        .expect("manifest must be writable");
    }

    /// Given a workspace whose project/extra/user discovery sources and an
    /// installed plugin define overlapping skill names, when the full
    /// discovery sequence replays (item G), then the TS priority order
    /// holds: plugin skills (priority 5) lose same-name conflicts to
    /// user/extra/project skills (higher priorities), while a plugin-only
    /// skill still registers — the plugin floor above the builtin
    /// bundle (0).
    #[tokio::test]
    async fn should_order_plugin_skill_discovery_below_user_extra_and_project_roots_when_replaying()
    {
        // Given a pinned $HOME (hermetic default roots), a workspace with a
        // project skill root, a config with an extra skill dir, a user-brand
        // `<home>/skills`, and an installed plugin whose skill root defines
        // the same names plus one plugin-only name
        let workspace_root = tempfile::tempdir().expect("workspace tempdir");
        let home = tempfile::tempdir().expect("home tempdir");
        let isolated_home = tempfile::tempdir().expect("isolated $HOME tempdir");

        let project_skills = workspace_root.path().join(".agents/skills/proj-skill");
        std::fs::create_dir_all(&project_skills).expect("project skill dir");
        std::fs::write(
            project_skills.join("SKILL.md"),
            "---\ndescription: project-target\n---\n# Project\n",
        )
        .expect("project SKILL.md");
        let extra_dir = workspace_root.path().join("extra-skills");
        std::fs::create_dir_all(extra_dir.join("extra-skill")).expect("extra skill dir");
        std::fs::write(
            extra_dir.join("extra-skill").join("SKILL.md"),
            "---\ndescription: extra-target\n---\n# Extra\n",
        )
        .expect("extra SKILL.md");
        let brand_skill = home.path().join("skills/user-skill");
        std::fs::create_dir_all(&brand_skill).expect("brand skills dir");
        std::fs::write(
            brand_skill.join("SKILL.md"),
            "---\ndescription: user-target\n---\n# User\n",
        )
        .expect("brand SKILL.md");

        let source_dir = tempfile::tempdir().expect("plugin source tempdir");
        write_plugin_skill_source(
            &source_dir,
            &[
                ("proj-skill", "plugin-proj"),
                ("extra-skill", "plugin-extra"),
                ("user-skill", "plugin-user"),
                ("plugin-only", "plugin-only"),
            ],
        );
        let plugins_dir = tempfile::tempdir().expect("plugins tempdir");
        let manager = mirri_plugin_host::PluginManager::new(plugins_dir.path().to_path_buf());
        // The install is async and HOME-independent — run it BEFORE the
        // $HOME pin so the lock is never held across an await.
        manager
            .install(&format!("local:{}", source_dir.path().display()))
            .await
            .expect("plugin install");

        let config = ServerConfig {
            extra_skill_dirs: Some(vec![extra_dir.display().to_string()]),
            ..ServerConfig::default()
        };

        let workspace = WorkspaceScope::new(
            WorkspaceId::new("skill-priority-ws"),
            workspace_root.path().to_path_buf(),
            WorkspaceScopeConfig::default(),
        );

        // When the discovery replay runs over every source — under the crate
        // $HOME pin (sync only; the async install above ran unpinned)
        let _home_lock = mirri_test_support::env::lock(); // env_lock: workspace-wide HOME pin guard
        let previous_home = std::env::var_os("HOME");
        // SAFETY: restored immediately after the replay, under the crate
        // home lock (the existing bootstrap tests' pin pattern).
        unsafe { std::env::set_var("HOME", isolated_home.path()) };
        apply_skill_discovery(&workspace, &config, Some(home.path()), Some(&manager), &[]);
        // SAFETY: restore the previous $HOME (see the set_var above).
        // env_lock: restore under the workspace env lock guard above
        match previous_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        drop(_home_lock);

        // Then every higher-priority source beats the plugin's same-name
        // skill, and the plugin-only skill is registered (plugin 5 above
        // the builtin bundle 0 — the builtin channel is the engine's
        // lowest discovery floor)
        let skills = workspace.skills();
        let proj = skills.get("proj-skill").expect("project skill present");
        assert_eq!(
            proj.source,
            mirri_core::skill::SkillSource::Project,
            "the workspace (30) skill must beat the same-name plugin (5) skill"
        );
        let extra = skills.get("extra-skill").expect("extra skill present");
        assert_eq!(
            extra.description.as_str(),
            "extra-target",
            "the extra (10) skill must beat the same-name plugin (5) skill"
        );
        let user = skills.get("user-skill").expect("user skill present");
        assert_eq!(
            user.source,
            mirri_core::skill::SkillSource::User,
            "the user-brand (20) skill must beat the same-name plugin (5) skill"
        );
        let only = skills
            .get("plugin-only")
            .expect("plugin-only skill present");
        assert_eq!(
            only.source,
            mirri_core::skill::SkillSource::Plugin,
            "the plugin-only skill must register (the plugin floor above builtin)"
        );
    }

    /// Given explicit `skill_dirs` (the v1/v2 SDK replacement channel — the
    /// TS explicit source at user priority) and an installed plugin defining
    /// the same skill name, when the discovery replay runs, then the
    /// explicit directory beats the plugin's same-name skill (item G) and a
    /// plugin-only skill still registers.
    #[tokio::test]
    async fn should_let_explicit_skill_dirs_beat_plugin_skills_when_replaying() {
        // Given a pinned $HOME, an explicit skill dir defining "shared",
        // and a plugin defining the same name plus a plugin-only name
        let workspace_root = tempfile::tempdir().expect("workspace tempdir");
        let home = tempfile::tempdir().expect("home tempdir");
        let isolated_home = tempfile::tempdir().expect("isolated $HOME tempdir");

        let explicit_dir = tempfile::tempdir().expect("explicit skill dir tempdir");
        std::fs::create_dir_all(explicit_dir.path().join("shared")).expect("explicit skill dir");
        std::fs::write(
            explicit_dir.path().join("shared").join("SKILL.md"),
            "---\ndescription: explicit-target\n---\n# Explicit\n",
        )
        .expect("explicit SKILL.md");

        let source_dir = tempfile::tempdir().expect("plugin source tempdir");
        write_plugin_skill_source(
            &source_dir,
            &[("shared", "plugin-shared"), ("plugin-only", "plugin-only")],
        );
        let plugins_dir = tempfile::tempdir().expect("plugins tempdir");
        let manager = mirri_plugin_host::PluginManager::new(plugins_dir.path().to_path_buf());
        manager
            .install(&format!("local:{}", source_dir.path().display()))
            .await
            .expect("plugin install");

        let workspace = WorkspaceScope::new(
            WorkspaceId::new("skill-explicit-ws"),
            workspace_root.path().to_path_buf(),
            WorkspaceScopeConfig::default(),
        );

        // When the replay runs with the explicit skill dirs (replacement
        // discovery — the plugin roots still ride the sequence), under the
        // crate $HOME pin (sync only; the async install ran unpinned)
        let _home_lock = mirri_test_support::env::lock(); // env_lock: workspace-wide HOME pin guard
        let previous_home = std::env::var_os("HOME");
        // SAFETY: restored immediately after the replay (see the pin above).
        unsafe { std::env::set_var("HOME", isolated_home.path()) };
        apply_skill_discovery(
            &workspace,
            &ServerConfig::default(),
            Some(home.path()),
            Some(&manager),
            &[explicit_dir.path().to_path_buf()],
        );
        // SAFETY: restore the previous $HOME (see the set_var above).
        // env_lock: restore under the workspace env lock guard above
        match previous_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        drop(_home_lock);

        // Then the explicit dir (user priority 20) beat the plugin's
        // same-name skill, and the plugin-only skill is still present
        let skills = workspace.skills();
        let shared = skills.get("shared").expect("explicit skill present");
        assert_eq!(
            shared.description.as_str(),
            "explicit-target",
            "the explicit skill dir (user priority) must beat the same-name plugin (5) skill"
        );
        let only = skills
            .get("plugin-only")
            .expect("plugin-only skill present");
        assert_eq!(
            only.source,
            mirri_core::skill::SkillSource::Plugin,
            "the plugin-only skill must register alongside the explicit dirs"
        );
    }

    // ===== builtin skill registration channel (design 13 §3.3, M1 data plane) ==
    //
    // These tests exercise `apply_skill_discovery`'s builtin first-insert
    // (ADR-B3): the builtin bundle registers right after `clear()` and
    // before the plugin roots, so the later-wins sequence makes it the
    // lowest-priority discovery channel (TS builtin(0)).

    /// Pin `$HOME` to `isolated` for the duration of `f` under the crate
    /// home lock — the hermetic-discovery pattern of the skill-priority
    /// tests above, factored for the builtin-channel tests.
    fn with_pinned_home<T>(isolated: &Path, f: impl FnOnce() -> T) -> T {
        let _home_lock = mirri_test_support::env::lock(); // env_lock: workspace-wide HOME pin guard
        let previous_home = std::env::var_os("HOME");
        // SAFETY: restored immediately after `f`, under the workspace env lock
        // (the existing bootstrap tests' pin pattern).
        unsafe { std::env::set_var("HOME", isolated) };
        let out = f();
        // SAFETY: restore the previous $HOME (see the set_var above).
        // env_lock: restore under the workspace env lock guard above
        match previous_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        out
    }

    /// Given a workspace with no user/project/extra/plugin skills, when the
    /// discovery sequence replays, then all eight builtin entries register
    /// with the builtin source, the TS-literal `builtin://` pseudo paths
    /// (slash-separated for the sub-skill family), and
    /// the TS gate flags per entry (V1 — the data plane), and their bodies
    /// are served through the builtin content table.
    #[test]
    fn should_register_eight_builtin_entries_when_replaying_the_discovery_sequence() {
        // Given empty workspace / home / $HOME roots (nothing to shadow or
        // overlap the builtin bundle)
        let workspace_root = tempfile::tempdir().expect("workspace tempdir");
        let home = tempfile::tempdir().expect("home tempdir");
        let isolated_home = tempfile::tempdir().expect("isolated $HOME tempdir");
        let workspace = WorkspaceScope::new(
            WorkspaceId::new("builtin-v1-ws"),
            workspace_root.path().to_path_buf(),
            WorkspaceScopeConfig::default(),
        );

        // When the replay runs under the pinned $HOME
        with_pinned_home(isolated_home.path(), || {
            apply_skill_discovery(
                &workspace,
                &ServerConfig::default(),
                Some(home.path()),
                None,
                &[],
            );
        });

        // Then all eight builtin entries are present with source, pseudo
        // path, and the exact TS gate-flag override points (V1)
        let skills = workspace.skills();
        let expected: &[(&str, &str, bool, bool)] = &[
            ("mcp-config", "builtin://mcp-config", true, false),
            (
                "import-from-cc-codex",
                "builtin://import-from-cc-codex",
                true,
                false,
            ),
            ("update-config", "builtin://update-config", false, false),
            ("custom-theme", "builtin://custom-theme", true, false),
            ("write-goal", "builtin://write-goal", false, false),
            ("sub-skill", "builtin://sub-skill", true, false),
            ("sub-skill.review", "builtin://sub-skill/review", true, true),
            (
                "sub-skill.consolidate",
                "builtin://sub-skill/consolidate",
                true,
                true,
            ),
        ];
        for (name, pseudo_path, disable_model_invocation, is_sub_skill) in expected {
            let skill = skills
                .get(name)
                .unwrap_or_else(|| panic!("builtin {name} present"));
            assert_eq!(skill.source, SkillSource::Builtin, "{name}");
            assert_eq!(
                skill.path,
                PathBuf::from(*pseudo_path),
                "{name}: the TS-literal pseudo path"
            );
            assert_eq!(
                skill.disable_model_invocation, *disable_model_invocation,
                "{name}"
            );
            assert_eq!(skill.is_sub_skill, *is_sub_skill, "{name}");
            assert!(!skill.description.is_empty(), "{name}: description parsed");
        }
        assert_eq!(
            skills.list().len(),
            8,
            "an empty user directory assembles to exactly the builtin bundle"
        );
        // And the embedded bodies are served through read_content (the
        // content-table supply — ADR-B1)
        let content = skills.read_content("write-goal").expect("embedded body");
        assert!(content.contains("# Write a good goal (write-goal)"));
        let sub_content = skills
            .read_content("sub-skill.review")
            .expect("embedded body");
        assert!(sub_content.contains("# Review sub-skills (`sub-skill.review`)"));
    }

    /// Given a user-brand skill named like a builtin, when the sequence
    /// replays, then the user skill shadows the builtin (V2) — read_content
    /// falls back to the disk copy — and once the user skill is removed and
    /// the sequence replays again, the builtin returns with its embedded
    /// body.
    #[test]
    fn should_restore_a_shadowed_builtin_when_the_user_skill_is_removed_and_resynced() {
        // Given a user-brand `<home>/skills/write-goal` shadowing the
        // builtin of the same name
        let workspace_root = tempfile::tempdir().expect("workspace tempdir");
        let home = tempfile::tempdir().expect("home tempdir");
        let isolated_home = tempfile::tempdir().expect("isolated $HOME tempdir");
        let user_skill = home.path().join("skills/write-goal");
        std::fs::create_dir_all(&user_skill).expect("user skill dir");
        std::fs::write(
            user_skill.join("SKILL.md"),
            "---\ndescription: user override\n---\n# User write-goal\n",
        )
        .expect("user SKILL.md");
        let workspace = WorkspaceScope::new(
            WorkspaceId::new("builtin-v2-ws"),
            workspace_root.path().to_path_buf(),
            WorkspaceScopeConfig::default(),
        );

        // When the replay runs
        with_pinned_home(isolated_home.path(), || {
            apply_skill_discovery(
                &workspace,
                &ServerConfig::default(),
                Some(home.path()),
                None,
                &[],
            );
        });

        // Then the user skill wins the same-name conflict (V2) and its
        // disk body is what read_content serves
        let skills = workspace.skills();
        let shadowed = skills.get("write-goal").expect("write-goal present");
        assert_eq!(
            shadowed.source,
            SkillSource::User,
            "the user skill shadows the builtin"
        );
        assert_eq!(shadowed.path, user_skill);
        assert!(
            skills
                .read_content("write-goal")
                .expect("user body readable")
                .contains("# User write-goal"),
            "a shadowed builtin reads from disk, not the content table"
        );

        // When the user skill is removed and the sequence replays (the
        // config-write resync shape)
        std::fs::remove_dir_all(home.path().join("skills")).expect("user skills removed");
        with_pinned_home(isolated_home.path(), || {
            apply_skill_discovery(
                &workspace,
                &ServerConfig::default(),
                Some(home.path()),
                None,
                &[],
            );
        });

        // Then the builtin returns with its pseudo path and embedded body
        let restored = skills
            .get("write-goal")
            .expect("builtin write-goal restored");
        assert_eq!(restored.source, SkillSource::Builtin);
        assert_eq!(restored.path, PathBuf::from("builtin://write-goal"));
        assert!(
            skills
                .read_content("write-goal")
                .expect("embedded body readable")
                .contains("# Write a good goal (write-goal)"),
            "the restored builtin serves the embedded body again"
        );
    }

    /// Given non-empty explicit `skill_dirs` (the v1/v2 SDK replacement
    /// channel), when the sequence replays, then the builtin bundle is still
    /// registered alongside them — builtins are App-level constants the
    /// explicit dirs only replace the user/project defaults (the TS
    /// `builtin(0)` semantics).
    #[test]
    fn should_keep_builtin_entries_when_explicit_skill_dirs_replace_default_discovery() {
        // Given an explicit skill dir carrying one user skill
        let workspace_root = tempfile::tempdir().expect("workspace tempdir");
        let home = tempfile::tempdir().expect("home tempdir");
        let isolated_home = tempfile::tempdir().expect("isolated $HOME tempdir");
        let explicit_dir = tempfile::tempdir().expect("explicit skill dir tempdir");
        std::fs::create_dir_all(explicit_dir.path().join("explicit-skill")).expect("explicit dir");
        std::fs::write(
            explicit_dir.path().join("explicit-skill").join("SKILL.md"),
            "---\ndescription: explicit\n---\n# Explicit\n",
        )
        .expect("explicit SKILL.md");
        let workspace = WorkspaceScope::new(
            WorkspaceId::new("builtin-explicit-ws"),
            workspace_root.path().to_path_buf(),
            WorkspaceScopeConfig::default(),
        );

        // When the replay runs with the non-empty explicit dirs (the
        // replacement branch — extra dirs and default roots are skipped)
        with_pinned_home(isolated_home.path(), || {
            apply_skill_discovery(
                &workspace,
                &ServerConfig::default(),
                Some(home.path()),
                None,
                &[explicit_dir.path().to_path_buf()],
            );
        });

        // Then the explicit skill is registered and every builtin entry is
        // still present (builtin survives the replacement branch)
        let skills = workspace.skills();
        let explicit = skills
            .get("explicit-skill")
            .expect("explicit skill present");
        assert_eq!(explicit.source, SkillSource::User);
        let builtin_count = skills
            .list()
            .iter()
            .filter(|skill| skill.source == SkillSource::Builtin)
            .count();
        assert_eq!(
            builtin_count, 8,
            "the builtin bundle rides the explicit branch"
        );
        assert_eq!(
            skills.list().len(),
            9,
            "8 builtins + the one explicit skill"
        );
        assert_eq!(
            skills.get("mcp-config").expect("mcp-config present").path,
            PathBuf::from("builtin://mcp-config")
        );
    }

    /// Given a replayed discovery sequence, when it replays a second time
    /// (the config-write resync shape), then the builtin entries survive
    /// without duplicates (V9 — resync idempotency).
    #[test]
    fn should_keep_builtin_entries_without_duplicates_when_replaying_the_sequence_twice() {
        // Given an empty workspace whose sequence has already been applied
        let workspace_root = tempfile::tempdir().expect("workspace tempdir");
        let home = tempfile::tempdir().expect("home tempdir");
        let isolated_home = tempfile::tempdir().expect("isolated $HOME tempdir");
        let workspace = WorkspaceScope::new(
            WorkspaceId::new("builtin-resync-ws"),
            workspace_root.path().to_path_buf(),
            WorkspaceScopeConfig::default(),
        );

        // When the sequence replays a second time (clear → builtin → …)
        with_pinned_home(isolated_home.path(), || {
            apply_skill_discovery(
                &workspace,
                &ServerConfig::default(),
                Some(home.path()),
                None,
                &[],
            );
            apply_skill_discovery(
                &workspace,
                &ServerConfig::default(),
                Some(home.path()),
                None,
                &[],
            );
        });

        // Then the builtin bundle is neither lost nor duplicated (V8)
        let skills = workspace.skills();
        let listed = skills.list();
        assert_eq!(
            listed.len(),
            8,
            "no duplicate registration after the resync"
        );
        assert!(
            listed
                .iter()
                .all(|skill| skill.source == SkillSource::Builtin),
            "every entry is a builtin (none lost, none foreign)"
        );
        let names: Vec<&str> = listed.iter().map(|skill| skill.name.as_str()).collect();
        let unique: std::collections::HashSet<&str> = names.iter().copied().collect();
        assert_eq!(names.len(), unique.len(), "all eight names are distinct");
    }

    // ===== default-root four-channel dedupe (ADR-011) ==
    //
    // The replay's default-root branch carries the full four-channel list
    // (the three construction-time channels + the user brand dir) and dedupes
    // it by normalized path before scanning, so a workspace rooted at $HOME
    // no longer re-scans the same directory per channel with per-skill
    // shadow warnings.

    /// A shared byte buffer usable as a `tracing` writer, so a test can
    /// capture what the replay logs (the mirri-acp `SharedBuf` pattern).
    #[derive(Clone, Default)]
    struct SharedLogBuf(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for SharedLogBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedLogBuf {
        type Writer = SharedLogBuf;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Given a workspace rooted at $HOME itself (so the project brand dir
    /// `cwd/.mirri-code/skills` and the user brand dir `<mirri home>/skills`
    /// collide when mirri home is $HOME/.mirri-code), when the discovery
    /// replay runs, then the skill is discovered once as the user channel
    /// and no shadowed warning is logged (ADR-011 — the replay dedupes the
    /// full four-channel list).
    #[test]
    fn should_not_warn_shadowed_when_replay_runs_with_collided_brand_dir() {
        let home = tempfile::tempdir().expect("home tempdir");
        // mirri home = <home>/.mirri-code; user brand dir = <home>/.mirri-code/skills
        // project brand dir = cwd/.mirri-code/skills — cwd IS home, so same dir
        let skills_dir = home.path().join(".mirri-code/skills/brand-dup");
        std::fs::create_dir_all(&skills_dir).expect("create skill dir");
        std::fs::write(skills_dir.join("SKILL.md"), "# BrandDup\n").expect("write SKILL.md");

        let isolated_home = tempfile::tempdir().expect("isolated $HOME tempdir");
        let workspace = WorkspaceScope::new(
            WorkspaceId::new("ws-brand-collision"),
            home.path().to_path_buf(),
            WorkspaceScopeConfig::default(),
        );

        // When the replay runs under a WARN-capturing subscriber and the
        // crate $HOME pin (mirri home injected explicitly, so it does not
        // depend on MIRRICODE_HOME env)
        let buf = SharedLogBuf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            with_pinned_home(isolated_home.path(), || {
                apply_skill_discovery(
                    &workspace,
                    &ServerConfig::default(),
                    Some(home.path().join(".mirri-code").as_path()),
                    None,
                    &[],
                );
            });
        });

        // Then the skill is present once as the user channel (the later-wins
        // winner of the collided brand dir), and the captured log carries no
        // shadowed warning — the directory was scanned a single time.
        let skill = workspace
            .skills()
            .get("brand-dup")
            .expect("brand skill discovered");
        assert_eq!(
            skill.source,
            SkillSource::User,
            "the replay must scan the collided brand dir once, as the user channel"
        );
        let guard = buf.0.lock().unwrap();
        let captured = String::from_utf8_lossy(&guard);
        assert!(
            !captured.contains("shadowed"),
            "the deduped replay must not warn about shadowed skills, got: {captured}"
        );
    }

    /// Given a home whose config.toml carries a typed
    /// `providers.*.api_key`, when the engine is assembled, then the
    /// start-up migration moves the key into the credential store and
    /// strips the config field before the factory ever resolves a
    /// provider (E.6 — re-checked at every server start).
    #[test]
    fn should_migrate_config_api_keys_into_the_store_when_assembled() {
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let home = tempfile::tempdir().expect("home tempdir");
        std::fs::write(
            home.path().join("config.toml"),
            r#"
[providers.openai]
type = "openai"
api_key = "sk-from-config"
"#,
        )
        .expect("seed config write");

        let parts = assemble(
            WorkspaceId::new("migration-test"),
            workspace.path().to_path_buf(),
            Some(home.path().to_path_buf()),
            None,
            WorkspacePolicy::UntrustedDefault,
            Vec::new(),
        );
        let _ = parts; // (the assembly's own invariants are pinned elsewhere)

        // Then the key lives in the store, not the config file.
        let store = mirri_credentials::CredentialStore::open(home.path())
            .expect("the store opens");
        assert_eq!(
            store.reveal("openai").as_deref(),
            Some("sk-from-config"),
            "the assembly migrated the typed api_key into the store"
        );
        let text = std::fs::read_to_string(home.path().join("config.toml"))
            .expect("config re-read");
        assert!(
            !text.contains("sk-from-config"),
            "the compensating cleanup stripped the field: {text}"
        );
    }

    // ===== Injectable home resolution (16-test-execution-strategy §4.2:
    // production HOME reads reachable from lib test targets expose a
    // `resolve_*_from` variant so tests pass the value instead of pinning
    // the process env) =====

    /// Given an injected fake os-home, when extra-skill-dir entries are
    /// resolved, then `~` and `~/…` expand under that home — no
    /// process-env mutation involved (the `resolve_extra_skill_dir_from`
    /// variant; the env-based `resolve_extra_skill_dir` keeps the
    /// production behavior).
    #[test]
    fn should_expand_tilde_against_the_injected_os_home_when_resolving_extra_skill_dirs() {
        let root = PathBuf::from("/ws/root");
        let fake_home = tempfile::tempdir().expect("fake home tempdir");

        let tilde = resolve_extra_skill_dir_from(
            "~",
            &root,
            Some(std::ffi::OsString::from(fake_home.path())),
        );
        let nested = resolve_extra_skill_dir_from(
            "~/pdf-skills",
            &root,
            Some(std::ffi::OsString::from(fake_home.path())),
        );

        // Then `~` is the fake home itself and `~/x` expands under it
        assert_eq!(tilde, fake_home.path().to_path_buf());
        assert_eq!(nested, fake_home.path().join("pdf-skills"));
        // And no injected home leaves the entry un-expanded but still
        // structurally resolved (the absolute/relative rules still apply)
        assert_eq!(
            resolve_extra_skill_dir_from("/abs/skills", &root, None),
            PathBuf::from("/abs/skills")
        );
        assert_eq!(
            resolve_extra_skill_dir_from("rel-skills", &root, None),
            root.join("rel-skills")
        );
    }

    /// Given an injected fake os-home, when the profile discovery roots are
    /// built, then the generic `~/.agents/agents` root points under that
    /// home — no process-env mutation involved.
    #[test]
    fn should_list_the_generic_agents_root_under_the_injected_os_home() {
        let mirri_home = tempfile::tempdir().expect("mirri home tempdir");
        let os_home = tempfile::tempdir().expect("os home tempdir");
        std::fs::create_dir_all(os_home.path().join(".agents").join("agents"))
            .expect("generic agents root");
        // The legacy root must exist to appear (the chain filters on is_dir).
        std::fs::create_dir_all(mirri_home.path().join("profiles"))
            .expect("legacy profiles root");

        let roots = profile_discovery_roots_from(
            Some(mirri_home.path()),
            Some(std::ffi::OsString::from(os_home.path())),
            None,
        );

        // Then the roots carry the generic root under the injected os-home
        // and the legacy `<mirri home>/profiles` — none of which required a
        // `$HOME` pin.
        let paths: Vec<std::path::PathBuf> = roots.into_iter().map(|root| root.path).collect();
        assert!(
            paths.contains(&os_home.path().join(".agents").join("agents")),
            "the generic root resolves under the injected os home, got: {paths:?}"
        );
        assert!(
            paths.contains(&mirri_home.path().join("profiles")),
            "the legacy root resolves under the mirri home, got: {paths:?}"
        );
    }

    /// Given an injected fake os-home, when the MCP home base is resolved
    /// with no explicit home, then `$HOME` (the fake) wins — no
    /// process-env mutation involved.
    #[test]
    fn should_use_the_injected_os_home_when_resolving_the_mcp_home_base() {
        let fake_home = tempfile::tempdir().expect("fake home tempdir");

        let base = resolve_mcp_home_base_from(
            None,
            Some(std::ffi::OsString::from(fake_home.path())),
        );

        // Then the base is the fake os-home (the `$HOME` tier of the chain)
        assert_eq!(base, Some(fake_home.path().to_path_buf()));
    }

    /// Given no home anywhere, when the MCP home base is resolved, then the
    /// chain answers `None` (no `$HOME`, no `MIRRICODE_HOME`) — the
    /// injected variant makes the previously untestable tail reachable.
    /// The developer shell's `MIRRICODE_HOME`/`HOME` are cleared for the
    /// assertion window under the workspace env lock.
    #[test]
    fn should_answer_no_mcp_home_base_when_no_home_can_be_resolved() {
        let mut _env = mirri_test_support::env::EnvGuard::new();
        _env.remove("MIRRICODE_HOME");
        _env.remove("HOME");
        assert_eq!(
            resolve_mcp_home_base_from(None, None),
            None,
            "with no $HOME and no resolvable Mirri home the base is None"
        );
    }

    /// Given a workspace carrying a LEGACY workspace-local session
    /// (`<root>/.mirri/v3_sessions/<bucket>/<id>/` with state.json +
    /// wire.jsonl — the pre-ADR-006 no-home shape), when an engine is
    /// assembled over a home, then the session directory moved into the
    /// home's corresponding bucket, an index row was minted from its
    /// state.json, and the legacy root is gone — and a SECOND assembly
    /// over the same home is a no-op (the idempotent skip).
    #[tokio::test]
    async fn should_migrate_workspace_local_sessions_into_the_home_when_assembling() {
        // Given a workspace with one legacy workspace-local session
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        let home = tempfile::tempdir().expect("home tempdir");
        let bucket = encode_work_dir_key(workspace.path());
        let session_dir = workspace
            .path()
            .join(".mirri")
            .join("v3_sessions")
            .join(&bucket)
            .join("sess-legacy-1");
        std::fs::create_dir_all(&session_dir).expect("create the legacy session dir");
        std::fs::write(
            session_dir.join("state.json"),
            r#"{"version": 3, "title": "Legacy Session", "createdAt": "2026-01-01T00:00:00Z",
                "updatedAt": "2026-01-02T00:00:00Z", "workDir": null, "last_prompt": "old hello"}"#,
        )
        .expect("write the legacy state.json");
        std::fs::write(session_dir.join("wire.jsonl"), "{}\n").expect("write the legacy wire log");

        // When the engine parts are assembled over the home (the default
        // workspace build runs the migration)
        let parts = assemble(
            WorkspaceId::new("legacy-migration-ws"),
            workspace.path().to_path_buf(),
            Some(home.path().to_path_buf()),
            None,
            WorkspacePolicy::UntrustedDefault,
            Vec::new(),
        );

        // Then the session directory moved into the home's bucket
        let migrated = home
            .path()
            .join("v3_sessions")
            .join(&bucket)
            .join("sess-legacy-1");
        assert!(
            migrated.join("wire.jsonl").is_file(),
            "the legacy session must live under the home's bucket: {}",
            migrated.display()
        );
        // And the legacy root is gone (the natural completion marker)
        assert!(
            !workspace.path().join(".mirri").join("v3_sessions").exists(),
            "the emptied legacy root must be removed"
        );
        // And the index row was minted from the legacy state.json
        let index = parts
            .session_manager
            .metadata_store();
        let record = index
            .get(&mirri_core::SessionId::new("sess-legacy-1"))
            .await
            .expect("the migrated session's row exists");
        assert_eq!(record.title, "Legacy Session");
        assert_eq!(record.last_prompt.as_deref(), Some("old hello"));

        // And a second assembly over the same home is a no-op: the row is
        // untouched (the idempotent skip — no legacy root to re-migrate)
        let workspace_b = tempfile::tempdir().expect("workspace-b tempdir");
        let parts_b = assemble(
            WorkspaceId::new("legacy-migration-ws"),
            workspace_b.path().to_path_buf(),
            Some(home.path().to_path_buf()),
            None,
            WorkspacePolicy::UntrustedDefault,
            Vec::new(),
        );
        let record_b = parts_b
            .session_manager
            .metadata_store()
            .get(&mirri_core::SessionId::new("sess-legacy-1"))
            .await
            .expect("the row survives the second assembly");
        assert_eq!(record_b.title, "Legacy Session");
    }
}
