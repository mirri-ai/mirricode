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
use mirri_core::{
    PermissionMode, ProfileManager, ServerConfig, SessionManager, SkillSource, WorkspaceId,
    WorkspaceScope, WorkspaceScopeConfig, WorkspaceTrustLevel, encode_work_dir_key, migrate_config,
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
    /// config, and the session persistence root. `None` means every home
    /// consumer resolves through the env-based
    /// [`mirri_home_dir`](mirri_core::config::mirri_home_dir) exactly as
    /// before the injection existed.
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
    // Auto-discover and load config.toml from the Mirri home directory
    // (migrate → `${VAR}` expand → env overrides; see
    // `load_config_with_migration`).
    let (config, config_path) = load_config_from_home(home_dir.as_deref());

    // The MCP user-layer base, resolved ONCE here and carried in the shared
    // services: the pool load, the config read/write surface, and the
    // reconcile kernel must all address the same file. An explicit base
    // (`mirri serve` passes `$HOME`) wins; otherwise see
    // [`resolve_mcp_home_base`].
    let mcp_home_base = mcp_home_dir
        .clone()
        .or_else(|| resolve_mcp_home_base(home_dir.as_deref()));

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
    let registry = WorkspaceRegistry::new(Arc::new(NoopResolver), builder);
    if let Some(home) = &home_dir {
        registry.install_session_index(home.clone());
    }

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
        workspace_registry: Arc::new(registry),
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
}

/// Replay the complete skill discovery sequence onto one built workspace
/// (B8): clear → builtin bundle (TS priority 0, ADR-B3) → plugin roots
/// (TS priority 5, item G) → (explicit skillDirs non-empty ? replacement
/// user discovery : config `extra_skill_dirs` → default roots + user-brand)
/// → the user-level disabled-set seed.
///
/// The order IS the priority: `SkillCatalog` discovery is later-wins, so the
/// sequence runs **lowest priority first** — the builtin bundle (0), then
/// plugin (5), then extra (10), then the user chain (20: default
/// `$HOME/.agents/skills` + user-brand), then the workspace/project roots
/// (30). Every higher source therefore overrides a same-name plugin skill,
/// while a plugin-only skill still registers — the plugin floor above the
/// builtin bundle (0). Note: when explicit skillDirs are non-empty they
/// replace the default discovery AND the config's extra dirs are skipped
/// with them (the v1/v2 SDK replacement contract — the TS v2
/// `ExtraFileSkillSource` is not suppressed there; the divergence is
/// recorded in skills-context §9.4); the plugin roots and the builtin
/// bundle ride both branches (builtins are App-level constants, not
/// user/project defaults — the TS `builtin(0)` semantics).
/// `resolve_extra_skill_dir` mirrors the TS `skillRoots.ts` path resolution
/// (`~` expansion / absolute / else relative to the workspace root).
pub(crate) fn apply_skill_discovery(
    workspace: &WorkspaceScope,
    config: &ServerConfig,
    home_dir: Option<&Path>,
    plugin_manager: Option<&mirri_plugin_host::PluginManager>,
    skill_dirs: &[PathBuf],
) {
    let skills = workspace.skills();
    skills.clear();
    // Builtin skills (design 13 §3.3 / ADR-B3): the six workflow skills
    // with compile-time-embedded bodies, registered FIRST — right after the
    // clear and before every other channel — so the later-wins rule makes
    // them the lowest-priority source (the TS `builtin(0)` floor). The
    // bundle rides both the explicit-skillDirs replacement branch and the
    // default discovery branch unchanged.
    skills.register_builtin(mirri_core::skill::builtin::builtin_skills());
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
            if let Err(e) = skills.discover_plugin(&root.path, &ctx) {
                tracing::warn!(%e, ?root.path, "failed to discover plugin skills");
            }
        }
    }
    if !skill_dirs.is_empty() {
        for dir in skill_dirs {
            if let Err(e) = skills.discover(dir, SkillSource::User) {
                tracing::warn!(%e, dir = %dir.display(), "failed to discover explicit skill dirs");
            }
        }
    } else {
        for entry in config.extra_skill_dirs.iter().flatten() {
            let dir = resolve_extra_skill_dir(entry, workspace.cwd());
            if let Err(e) = skills.discover(&dir, SkillSource::Plugin) {
                tracing::warn!(%e, dir = %dir.display(), "extra skill dir discovery failed");
            }
        }
        workspace.discover_default_skill_roots();
        if let Ok(mirri_home) = resolve_mirri_home(home_dir) {
            let user_brand_dir = mirri_home.join("skills");
            if let Err(e) = skills.discover(&user_brand_dir, SkillSource::User) {
                tracing::warn!(path = %user_brand_dir.display(), %e, "user-brand skill discovery failed");
            }
        }
    }
    skills.set_user_disabled(config.disabled_skills.clone().unwrap_or_default());
}

/// TS `skillRoots.ts` alignment: `~` (and `~/…`) expansion via `$HOME` /
/// absolute path / anything else relative to the workspace root.
fn resolve_extra_skill_dir(entry: &str, workspace_root: &Path) -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
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
    // clears and replays it, so the result is identical plus the
    // config-driven pieces.
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
    let session_manager = match &shared.home_dir {
        Some(home) => SessionManager::with_persistence_dir(
            workspace,
            home.join("v3_sessions"),
            Arc::new(mirri_os::LocalFs::new()),
        ),
        None => SessionManager::new(workspace),
    }
    .with_server_config(Arc::clone(&shared.config));
    let session_manager = Arc::new(session_manager);

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

    Ok(WorkspaceRuntime {
        workspace_id: reg.id.clone(),
        scope: Arc::clone(session_manager.workspace()),
        manager: session_manager,
        mcp_manager,
        pending_mcp_configs,
        trusted: std::sync::atomic::AtomicBool::new(trusted),
        mcp_connect_spawned: std::sync::atomic::AtomicBool::new(false),
        base_hooks,
    })
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
                        (Arc::new(RwLock::new(ServerConfig::default())), Some(config_path))
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
    let Ok(home_dir) = resolve_mirri_home(injected_home) else {
        return Vec::new();
    };
    let os_home = std::env::var_os("HOME").map(PathBuf::from);
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
    injected_home
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
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

/// Test-only `$HOME` synchronization for this crate: the harness runs all
/// tests of the binary in parallel in one process, and `$HOME` is
/// process-global — every test that pins `HOME` (bootstrap home isolation,
/// home-expansion assertions) must hold this lock across its whole
/// read/write window, or a concurrently running test can observe or clobber
/// the pinned value.
#[cfg(test)]
pub(crate) mod test_env {
    use std::sync::{Mutex, MutexGuard, OnceLock};

    /// Acquire the crate-wide `$HOME` lock (RAII — hold for the whole
    /// pinned window).
    pub fn home_lock() -> MutexGuard<'static, ()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mirri_core::HookEventType;

    /// should expand `~` and `~/…` extra-skill-dir entries against $HOME
    /// (B8: the TS skillRoots path-resolution parity)
    #[test]
    fn should_expand_home_prefixes_when_resolving_extra_skill_dirs() {
        let root = PathBuf::from("/ws/root");
        let fake_home = tempfile::tempdir().expect("fake home tempdir");
        let saved_home = std::env::var_os("HOME");
        unsafe {
            std::env::set_var("HOME", fake_home.path());
        }
        let resolved = [
            resolve_extra_skill_dir("~/pdf-skills", &root),
            resolve_extra_skill_dir("~", &root),
        ];
        if let Some(home) = saved_home {
            unsafe {
                std::env::set_var("HOME", home);
            }
        }

        // Then `~/x` expands under the home and bare `~` is the home itself
        assert_eq!(resolved[0], fake_home.path().join("pdf-skills"));
        assert_eq!(resolved[1], fake_home.path());
        // And absolute entries pass through, relative ones anchor at the root
        assert_eq!(
            resolve_extra_skill_dir("/abs/skills", &root),
            PathBuf::from("/abs/skills")
        );
        assert_eq!(
            resolve_extra_skill_dir("rel-skills", &root),
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
        // crate-wide home lock serializes the pin against the other
        // HOME-reading tests (the env is process-global).
        let isolated_home = tempfile::tempdir().expect("isolated home tempdir");
        let env_name = "HOME";
        let _home_lock = test_env::home_lock();
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
        let _home_lock = test_env::home_lock();
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
        // SAFETY: unique var name owned by this test; no other code reads it.
        unsafe { std::env::set_var(env_name, "expanded") };

        // When the engine parts are assembled through the funnel path
        let parts = assemble(
            WorkspaceId::new("migration-test"),
            workspace.path().to_path_buf(),
            Some(home.path().to_path_buf()),
            None,
            WorkspacePolicy::UntrustedDefault,
            Vec::new(),
        );
        // SAFETY: unique var name owned by this test; see the set_var above.
        unsafe { std::env::remove_var(env_name) };

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
        let _home_lock = test_env::home_lock();
        let previous = std::env::var_os(env_name);
        // SAFETY: the HOME env var is restored immediately after assembly,
        // and the crate-wide home lock serializes this window against the
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
        match previous {
            Some(value) => unsafe { std::env::set_var(env_name, value) },
            None => unsafe { std::env::remove_var(env_name) },
        }

        // Then the global MCP layer is discovered at <$HOME>/.mirri-code/
        // mcp.json — the user's actual file — not double-nested under
        // <$HOME>/.mirri-code/.mirri-code/mcp.json
        assert!(
            parts.mcp_manager.is_some(),
            "the user's global mcp.json must be discovered from $HOME"
        );
        assert!(
            parts.pending_mcp_configs.is_some(),
            "the discovered server configs must be queued for lazy connect"
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
    fn write_plugin_skill_source(
        source_dir: &tempfile::TempDir,
        skills: &[(&str, &str)],
    ) {
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
    async fn should_order_plugin_skill_discovery_below_user_extra_and_project_roots_when_replaying() {
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
        let _home_lock = test_env::home_lock();
        let previous_home = std::env::var_os("HOME");
        // SAFETY: restored immediately after the replay, under the crate
        // home lock (the existing bootstrap tests' pin pattern).
        unsafe { std::env::set_var("HOME", isolated_home.path()) };
        apply_skill_discovery(&workspace, &config, Some(home.path()), Some(&manager), &[]);
        // SAFETY: restore the previous $HOME (see the set_var above).
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
        let only = skills.get("plugin-only").expect("plugin-only skill present");
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
        write_plugin_skill_source(&source_dir, &[("shared", "plugin-shared"), ("plugin-only", "plugin-only")]);
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
        let _home_lock = test_env::home_lock();
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
        let only = skills.get("plugin-only").expect("plugin-only skill present");
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
        let _home_lock = test_env::home_lock();
        let previous_home = std::env::var_os("HOME");
        // SAFETY: restored immediately after `f`, under the crate home lock
        // (the existing bootstrap tests' pin pattern).
        unsafe { std::env::set_var("HOME", isolated) };
        let out = f();
        // SAFETY: restore the previous $HOME (see the set_var above).
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
}
