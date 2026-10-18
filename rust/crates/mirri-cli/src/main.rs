//! `mirri` — the Mirri Code server binary entry point.
//!
//! Parses the command line ([`Cli`](mirri_cli::Cli)) and boots the full
//! transport stack — HTTP + WebSocket + optional Unix-socket IPC — through
//! [`mirri_server::serve`] until a graceful shutdown is requested.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use futures::FutureExt;
use mirri_cli::watcher::{
    ConfigHotReloadInputs, HotReloadInputs, McpHotReloadInputs, ProfilesHotReloadInputs,
};
use mirri_cli::{Cli, Commands, commands, doctor};
// `AgentScopeConfig` is used by the crate-local tests only.
#[cfg(test)]
use mirri_core::AgentScopeConfig;
use mirri_core::{SessionManager, WorkspaceId};
use mirri_embedded::EmbeddedServer;
use mirri_embedded::bootstrap::{WorkspacePolicy, assemble};
use mirri_embedded::workspace_registry::WorkspaceRegistry;
use mirri_server::ServerState;

mod logging;
mod migrate;
mod print;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    run(cli).await
}

/// Dispatch the parsed CLI to the requested subcommand.
async fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Serve {
            host,
            port,
            ipc_path,
            workspace,
            log_level,
            auth_token,
            dangerous_bypass_auth,
            web_dir,
        } => {
            serve(
                &host,
                port,
                ipc_path.map(PathBuf::from),
                workspace.map(PathBuf::from),
                &log_level,
                auth_token,
                dangerous_bypass_auth,
                web_dir.map(PathBuf::from),
                false,
                false,
            )
            .await?;
        }
        Commands::Web {
            host,
            port,
            workspace,
            log_level,
            auth_token,
            dangerous_bypass_auth,
            web_dir,
            no_open,
        } => {
            // `web` runs the same server as `serve`, with front-end-friendly
            // defaults: a workspace that survives the packaged app's `/` cwd,
            // a SPA source resolved from `--web-dir` → the copy embedded in
            // the binary → a directory staged next to the executable
            // (Electron resources layout), and a browser opened at the
            // served URL unless `--no-open` disables it.
            let workspace =
                workspace.unwrap_or_else(|| web_workspace_default().to_string_lossy().into_owned());
            let web_dir = resolve_web_dir(
                web_dir.map(PathBuf::from),
                mirri_server::web_embed::embedded_web_available(),
                probe_web_dir_from_exe(),
            );
            serve(
                &host,
                port,
                None,
                Some(PathBuf::from(workspace)),
                &log_level,
                auth_token,
                dangerous_bypass_auth,
                web_dir,
                !no_open,
                true,
            )
            .await?;
        }
        Commands::Migrate { workspace, dry_run } => {
            migrate::run_migrate(workspace.as_deref(), dry_run).await?;
        }
        Commands::SessionList { workspace } => {
            commands::run_session_list(&workspace).await?;
        }
        Commands::ConfigShow { workspace } => {
            commands::run_config_show(&workspace).await?;
        }
        Commands::Print {
            prompt,
            model,
            provider,
        } => {
            print::run_print(prompt, model, provider).await?;
        }
        Commands::Doctor { config } => {
            // Dispatching docs: the `mirri doctor` run is wired in task 4.
            doctor::run_doctor(config).await?;
        }
    }
    Ok(())
}

/// Run the `serve` subcommand: initialize logging, build the engine scopes and
/// session manager, then serve HTTP/WS (plus IPC when a socket path is given)
/// until a graceful shutdown is requested.
///
/// `launch_browser` opens the system browser at the served URL once the
/// listener is bound (the `web` subcommand default); `warn_missing_web_ui`
/// prints the lite-build hint when no SPA source resolves — `serve` is
/// API-first and stays silent.
#[allow(clippy::too_many_arguments)]
async fn serve(
    host: &str,
    port: u16,
    ipc_path: Option<PathBuf>,
    workspace_root: Option<PathBuf>,
    log_level: &str,
    auth_token: Option<String>,
    dangerous_bypass_auth: bool,
    web_dir: Option<PathBuf>,
    launch_browser: bool,
    warn_missing_web_ui: bool,
) -> Result<()> {
    // 1. Initialize logging before anything else emits a trace. The dual
    //    sink (console at the operator's `--log-level`, file at a fixed
    //    `info` floor under `<mirri-home>/logs/`) needs the Mirri home
    //    first, so resolution order is home → token → subscriber (the
    //    file layer redacts the token from everything it persists). The
    //    returned guard flushes the non-blocking file writer on drop —
    //    binding it to `_log_guard` keeps it alive for the whole serve
    //    lifetime.
    let mirri_home = mirri_core::config::mirri_home_dir()?;
    let (initial_token, auth_token_source_path) =
        server_auth_token(&mirri_home, auth_token.as_deref(), dangerous_bypass_auth)?;
    let _log_guard = logging::init(log_level, &mirri_home, initial_token.as_deref());
    if dangerous_bypass_auth {
        tracing::warn!(
            "authentication disabled via --dangerous-bypass-auth: every route except \
             /api/v1/shutdown is unauthenticated; only use on a trusted network"
        );
    }

    // 2. Session persistence lives under <mirri-home>/v3_sessions/ (the
    //    rust engine's own storage root — ADR-006-R2, physically separate
    //    from the TS engine's `sessions/`).

    // 2b. Resolve the default workspace root (W9, design §4/§2.1 — D3 + D5):
    //     an explicit root wins (created when missing, the historical
    //     behavior); without one, the most-recently-opened registry row with
    //     a root that still exists becomes the default (every other row the
    //     funnel restores stays registered-but-lazy); an empty registry
    //     falls back to the current directory. The root is normalized
    //     LEXICALLY (`lexically_resolve`) — never canonicalized: a symlinked
    //     root would otherwise register a second row for the same directory
    //     (the id derivation hashes the lexical form, the TS parity).
    let workspace_root = match workspace_root {
        Some(root) => {
            std::fs::create_dir_all(&root)
                .with_context(|| format!("create workspace directory '{}'", root.display()))?;
            mirri_core::lexically_resolve(&root)
        }
        None => serve_default_workspace_root(&mirri_home),
    };

    let persistence_dir = mirri_home.join("v3_sessions");
    std::fs::create_dir_all(&persistence_dir).with_context(|| {
        format!(
            "create persistence directory '{}'",
            persistence_dir.display()
        )
    })?;

    // 3b. Assemble the engine through the shared composition root
    //     (`mirri_embedded::bootstrap`): config.toml load with the upgraded
    //     order (migrate → `${VAR}` expand → env overrides), the trusted
    //     workspace scope derived from the config (permission mode + hooks),
    //     the session manager, agent profiles, the MCP connection manager
    //     (lazy connect), plugins, the environment provider factory and the
    //     local FetchURL provider. `mirri serve` runs the user's own
    //     workspace, so the scope is Trusted — destructive operations
    //     proceed with fewer prompts. The MCP global layer is
    //     `<mcp_home_base>/.mirri-code/mcp.json`: the MCP crate's contract
    //     names `$HOME`, so serve passes it explicitly (the funnel's
    //     Mirri-home fallback would double-nest the path).
    let mcp_home_base: Option<PathBuf> = std::env::var_os("HOME").map(PathBuf::from);
    let parts = assemble(
        workspace_id_for(&workspace_root),
        workspace_root.clone(),
        Some(mirri_home.clone()),
        mcp_home_base.clone(),
        WorkspacePolicy::TrustedFromConfig,
        Vec::new(),
    );

    // 3c. Shared config: the engine serves the live snapshot and persists
    //     config changes; the state stores the same `Arc` the G-12 hot-reload
    //     watcher swaps, so file changes propagate to every reader. The
    //     funnel always resolves the well-known `<home>/config.toml` path
    //     when the home resolved — whether or not the file existed at boot
    //     (a fresh environment's first config.set creates it).
    let config_path = parts
        .config_path
        .clone()
        .unwrap_or_else(|| mirri_home.join("config.toml"));
    let config = Arc::clone(&parts.config);

    // 4. The engine: the composition root's fully-wired in-process server.
    //     `mirri serve` no longer builds providers or runs turns itself —
    //     every engine-domain RPC method is an `EngineAdapter` call, and the
    //     server state owns only transport + auth + projection state.
    //     The parts the watchers need are cloned before `from_parts`
    //     consumes the rest.
    let session_manager = Arc::clone(&parts.session_manager);
    let profile_manager = Arc::clone(&parts.profile_manager);
    let mcp_manager = parts.mcp_manager.clone();
    // The workspace registry the engine routes sessions through (W3): cloned
    // before `from_parts` consumes the rest, and injected into the transport
    // layer's session-routing seam below.
    let workspace_registry = Arc::clone(&parts.workspace_registry);
    let engine: Arc<dyn mirri_transport::adapter::EngineAdapter> =
        Arc::new(EmbeddedServer::from_parts(parts));

    // W7 multi-root watcher inputs: every REGISTERED workspace root (the
    // registry the funnel restored) — bound here, before the routing seam
    // below takes the registry Arc. The engine's trust gate stays
    // authoritative: an untrusted root's file event runs the same
    // `mcp.sync`, and the kernel keeps that root's pool empty.
    let watcher_workspace_roots: Vec<PathBuf> = workspace_registry
        .registrations()
        .into_iter()
        .map(|row| row.root)
        .collect();

    // 4a. Authentication: a shared Bearer token protects every HTTP route
    //     except the public health/meta probes. The token was resolved once
    //     up front (step 1, before the file-logging layer initialized with
    //     it as its redaction secret); `initial_token` is `None` only under
    //     `--dangerous-bypass-auth`, whose warning already fired above.
    let auth_token = initial_token;
    let state = match auth_token.as_ref() {
        Some(token) => {
            // The startup banner below prints the token value as a one-shot
            // CLI convenience (loopback, and the token already lives in
            // plaintext at `<mirri-home>/server.token`, so no new exposure
            // surface); routine per-request logging still never includes it.
            ServerState::with_engine(
                Arc::clone(&engine),
                Arc::clone(&session_manager),
                env!("CARGO_PKG_VERSION").to_string(),
                Some(token.clone()),
            )
        }
        None => ServerState::with_engine(
            Arc::clone(&engine),
            Arc::clone(&session_manager),
            env!("CARGO_PKG_VERSION").to_string(),
            None,
        ),
    }
    .with_config(Arc::clone(&config), Some(config_path.clone()))
    // The files REST surface (P4) shares the engine's files root — the same
    // `<mirri-home>/files` directory the `message.send` entry resolution
    // reads, so an uploaded file is immediately attachable to a prompt.
    .with_files_dir(mirri_home.join("files"))
    // The transport layer's session-routing seam (W3): every
    // session-addressing transport-owned handler and WS subscription routes
    // through the engine's workspace registry instead of the single bound
    // default-workspace manager — a session of a non-default workspace
    // resolves on its own runtime, and restore-on-demand resumes through the
    // engine's own (routed) `session.restore`.
    .with_session_routing(Arc::new(RegistrySessionRouting {
        registry: Arc::clone(&workspace_registry),
        engine: Arc::clone(&engine),
    }));

    // 4b. MCP servers: attach the funnel's connection manager so `/meta` can
    //     report the `mcp` capability accurately. The eager startup
    //     `connect_all` is replaced by the funnel's lazy connect — the actual
    //     connections spawn on the first engine call, and a status listener
    //     registers tools into live agents as servers come up.
    let state = if let Some(manager) = &mcp_manager {
        state.with_mcp_manager(Arc::clone(manager))
    } else {
        state
    };

    // 4d. Agent profiles: loaded by the funnel from the user roots chain —
    //     the brand `<mirri-home>/agents` and generic `~/.agents/agents`
    //     dirs (when they exist) plus the legacy `<mirri-home>/profiles/`
    //     (the write-channel target, created by the funnel). Enablement is
    //     the file-level `enabled` flag alone; config.toml's
    //     `disabled_agents` is retired (R3-3 — the funnel warns once when
    //     the key is still non-empty). The watcher watches the same chain
    //     (R2-3).
    let profile_roots = {
        let mut roots: Vec<PathBuf> = mirri_core::profile::user_profile_root_candidates(
            &mirri_home,
            std::env::var_os("HOME").map(PathBuf::from).as_deref(),
        )
        .into_iter()
        .filter(|root| root.is_dir())
        .collect();
        roots.push(mirri_home.join("profiles"));
        roots
    };

    // 4e. Zero-config hot-reload: watch the MCP config (3 layers), the
    //     profiles directory, and `config.toml` for the lifetime of the
    //     server. The config.toml watcher swaps the shared server config
    //     snapshot (providers/models/thinking/permission/loop control/yolo/
    //     plan mode) and re-registers hooks; experimental flags require a
    //     restart and are preserved. Each watcher logs its own failures; none
    //     of them can take the server down.
    //
    //     The MCP watcher owns no diff of its own (F-2): a config-file
    //     event dispatches `mcp.sync` on the fully-built server state —
    //     the engine's single reconcile kernel, whose summary is only
    //     logged here (no pool handle ever reaches the watcher).
    //
    let state_for_mcp_sync = Arc::new(state.clone());
    mirri_cli::watcher::spawn_config_watchers(HotReloadInputs {
        // The MCP watcher only runs when MCP configs were discovered — the
        // funnel's manager owns the discovered file set, and the watcher
        // reconciles against the same `home_dir` convention the manager was
        // built with (the global layer resolves under the Mirri home).
        mcp: mcp_manager.is_some().then(|| McpHotReloadInputs {
            home_dir: mcp_home_base.clone().unwrap_or_else(|| mirri_home.clone()),
            workspace_roots: watcher_workspace_roots,
            sync: {
                let state_for_mcp_sync = Arc::clone(&state_for_mcp_sync);
                Arc::new(move || {
                    let state = Arc::clone(&state_for_mcp_sync);
                    async move {
                        match state.dispatch("mcp.sync", serde_json::json!([])).await {
                            Ok(summary) => tracing::info!(
                                summary = %summary,
                                "MCP config watcher reconciled the pools"
                            ),
                            Err(error) => tracing::warn!(
                                %error,
                                "MCP config watcher reconcile (mcp.sync) failed"
                            ),
                        }
                    }
                    .boxed()
                })
            },
        }),
        profiles: Some(ProfilesHotReloadInputs {
            roots: profile_roots,
            manager: profile_manager,
        }),
        config: Some(ConfigHotReloadInputs {
            config_path,
            engine: session_manager.workspace().hook_engine(),
            config,
            // B9 R3: a hot-reloaded config.toml pushes the (possibly new)
            // user-level `[[permission.rules]]` into every live agent of
            // every built workspace runtime — the callback is sync, so
            // the async fan-out spawns fire-and-forget on the runtime
            // handle (best-effort semantics, aligned with the watcher's
            // other paths; the push itself is idempotent).
            on_reload: {
                let registry = Arc::clone(&workspace_registry);
                Some(Arc::new(move |config: &mirri_core::ServerConfig| {
                    let rules = config
                        .permission
                        .as_ref()
                        .and_then(|permission| permission.rules.clone())
                        .unwrap_or_default();
                    let registry = Arc::clone(&registry);
                    tokio::runtime::Handle::current().spawn(async move {
                        for runtime in registry.built_runtimes() {
                            runtime
                                .manager
                                .apply_user_permission_rules(rules.clone())
                                .await;
                        }
                    });
                }))
            },
        }),
    });

    // 5. Resolve the HTTP bind address (accepts IP literals and hostnames)
    //    and bind the listener, so everything printed or spawned afterwards
    //    (banner, browser opener) targets an already-reachable URL.
    let http_addr = resolve_socket_addr(host, port).await?;
    let http_listener = tokio::net::TcpListener::bind(http_addr)
        .await
        .with_context(|| format!("bind the HTTP listener on {http_addr}"))?;

    // 6. Request a graceful shutdown when the user presses Ctrl+C.
    let shutdown_state = state.clone();
    tokio::spawn(async move {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(%error, "failed to install the Ctrl+C handler");
            return;
        }
        tracing::info!("received Ctrl+C; requesting graceful shutdown");
        shutdown_state.request_shutdown();
    });

    // 7. Serve until a graceful shutdown is requested — Ctrl+C above, or
    //    `POST /api/v1/shutdown`. The startup banner points the operator at
    //    what was just bound: the clickable web UI URL when a SPA source
    //    exists (an explicit `--web-dir`, a directory probed next to the
    //    executable, or the copy embedded in the binary), and the bearer
    //    token when the authentication gate is on. Both are one-shot CLI
    //    conveniences for the loopback operator; the token already lives in
    //    plaintext at `<mirri-home>/server.token`, so printing it here adds
    //    no new exposure surface.
    tracing::info!(
        http_addr = %http_addr,
        ipc_path = ?ipc_path,
        workspace = %workspace_root.display(),
        "starting Mirri Code server"
    );
    let web_ui_url = format!("http://{}/", display_http_addr(&http_addr));
    let has_web_ui = web_dir.is_some() || mirri_server::web_embed::embedded_web_available();
    if has_web_ui {
        tracing::info!("web UI: {web_ui_url}");
    }
    if let Some(token) = &auth_token {
        match &auth_token_source_path {
            Some(path) => tracing::info!("auth token: {token}  ({})", path.display()),
            None => tracing::info!("auth token: {token}"),
        }
    }
    if !has_web_ui && warn_missing_web_ui {
        tracing::warn!(
            "this build does not include the web SPA: pass --web-dir <path> or install the \
             full distribution; continuing with the API only"
        );
    }
    if launch_browser {
        spawn_browser(&web_ui_url);
    }
    mirri_server::serve(http_listener, ipc_path, state, web_dir)
        .await
        .context("server exited with an error")?;
    tracing::info!("server shut down cleanly");
    Ok(())
}

/// The composition root's [`SessionRouting`] implementation (W3, design 08
/// §3): every session-addressing transport-owned handler and WS
/// subscription resolves the addressed session through the engine's
/// [`WorkspaceRegistry`] (active table → shared index → bucket probe →
/// not-found) instead of the single bound default-workspace manager, so a
/// session of a non-default workspace resolves on its own runtime.
///
/// Restore-on-demand rides the engine itself (`session.restore` resumes
/// through the embedded seam, which routes to the owning workspace), and a
/// resolution that turns out to hold a live session backfills the registry's
/// active table (see the workspace-registry module docs).
struct RegistrySessionRouting {
    registry: Arc<WorkspaceRegistry>,
    engine: Arc<dyn mirri_transport::adapter::EngineAdapter>,
}

impl mirri_server::rpc::SessionRouting for RegistrySessionRouting {
    fn live_session(
        &self,
        session_id: &str,
    ) -> futures::future::BoxFuture<'static, Option<Arc<mirri_core::SessionScope>>> {
        match self.registry.manager_for(session_id) {
            mirri_embedded::workspace_registry::ManagerResolution::Active { manager, .. }
            | mirri_embedded::workspace_registry::ManagerResolution::Restored { manager, .. } => {
                let id = mirri_core::SessionId::new(session_id);
                Box::pin(async move { manager.get_session(&id).await })
            }
            mirri_embedded::workspace_registry::ManagerResolution::NotFound => {
                Box::pin(std::future::ready(None))
            }
        }
    }

    fn session_artifacts(&self, session_id: &str) -> Option<mirri_core::SessionArtifacts> {
        match self.registry.manager_for(session_id) {
            mirri_embedded::workspace_registry::ManagerResolution::Active { manager, .. }
            | mirri_embedded::workspace_registry::ManagerResolution::Restored { manager, .. } => {
                manager.session_artifacts(&mirri_core::SessionId::new(session_id))
            }
            mirri_embedded::workspace_registry::ManagerResolution::NotFound => None,
        }
    }

    fn owning_manager(&self, session_id: &str) -> Option<Arc<SessionManager>> {
        match self.registry.manager_for(session_id) {
            mirri_embedded::workspace_registry::ManagerResolution::Active { manager, .. }
            | mirri_embedded::workspace_registry::ManagerResolution::Restored { manager, .. } => {
                Some(manager)
            }
            mirri_embedded::workspace_registry::ManagerResolution::NotFound => None,
        }
    }

    fn is_closed(&self, session_id: &str) -> bool {
        match self.registry.manager_for(session_id) {
            mirri_embedded::workspace_registry::ManagerResolution::Active { manager, .. }
            | mirri_embedded::workspace_registry::ManagerResolution::Restored { manager, .. } => {
                manager.is_closed(&mirri_core::SessionId::new(session_id))
            }
            // No owning manager remembers the id as closed.
            mirri_embedded::workspace_registry::ManagerResolution::NotFound => false,
        }
    }

    fn restore_session(&self, session_id: &str) -> futures::future::BoxFuture<'static, ()> {
        let engine = Arc::clone(&self.engine);
        let sid = session_id.to_string();
        Box::pin(async move {
            // Best-effort — the failure surfaces through the caller's own
            // not-found error — but never silent: a swallowed restore error
            // (e.g. a corrupt wire log) is otherwise indistinguishable from
            // an unknown session id on every read face that reports it.
            if let Err(error) = engine
                .call_method("session.restore", serde_json::json!({ "sessionId": sid }))
                .await
            {
                tracing::warn!(
                    session_id = %sid,
                    %error,
                    "restore-on-demand failed; read faces will report the session as not found"
                );
            }
        })
    }
}

/// Resolve the serve-time bearer token once, before logging initializes.
///
/// `--dangerous-bypass-auth` disables the gate (and the secret), an
/// explicit `--auth-token` wins, and the default loads (or creates, on
/// first boot) the persistent token from `<mirri-home>/server.token` —
/// the same file the TypeScript kap-server reads. The early resolution
/// lets the file-logging layer treat the token as its redaction secret
/// from the very first event. The returned source path is the token's
/// provenance for the startup banner (only a file-backed token has one).
fn server_auth_token(
    mirri_home: &Path,
    explicit: Option<&str>,
    dangerous_bypass_auth: bool,
) -> Result<(Option<String>, Option<PathBuf>)> {
    if dangerous_bypass_auth {
        return Ok((None, None));
    }
    match explicit {
        Some(token) => Ok((Some(token.to_string()), None)),
        None => {
            let token = mirri_server::server_token::load_or_create_server_token(mirri_home)
                .with_context(|| {
                    format!(
                        "load or create the persistent bearer token at '{}'",
                        mirri_server::server_token::server_token_path(mirri_home).display()
                    )
                })?;
            Ok((
                Some(token),
                Some(mirri_server::server_token::server_token_path(mirri_home)),
            ))
        }
    }
}

/// Derive a stable workspace ID from the workspace root's directory name,
/// falling back to `"default"` when the path has no usable file name.
fn workspace_id_for(workspace_root: &Path) -> WorkspaceId {
    let name = workspace_root
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("default");
    WorkspaceId::new(name)
}

/// Resolve the default workspace root for a `mirri serve` that was started
/// without an explicit `--workspace` (W9, design §4 — D5's default choice).
///
/// The home's `workspaces.json` registry decides: the **most-recently-opened
/// row whose root still exists** becomes the default workspace, so a plain
/// `mirri serve` serves the same multi-workspace catalog the web sees — every
/// other row stays registered-but-lazy (the funnel's D5 restore). A row whose
/// root has vanished is never chosen (its directory is never silently
/// re-created, and it is never eagerly built); an empty (or all-vanished)
/// registry falls back to the current directory — the pre-W9 single-workspace
/// behavior. The decision is factored into [`serve_default_workspace_root_at`]
/// so it can be tested without touching the real home.
fn serve_default_workspace_root(mirri_home: &Path) -> PathBuf {
    serve_default_workspace_root_at(
        &mirri_home.join(mirri_session_index::WORKSPACE_REGISTRY_FILENAME),
    )
}

/// [`serve_default_workspace_root`] against an explicit registry path.
fn serve_default_workspace_root_at(registry_path: &Path) -> PathBuf {
    let default = mirri_session_index::read_workspace_registry(registry_path)
        .into_iter()
        .filter(|row| {
            row.root
                .as_deref()
                .map(std::path::Path::new)
                .is_some_and(std::path::Path::is_dir)
        })
        .max_by_key(|row| row.last_opened_at)
        .and_then(|row| row.root)
        .map(|root| mirri_core::lexically_resolve(std::path::Path::new(&root)));
    default.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// Resolve the default workspace root for the `web` subcommand.
///
/// A human running `mirri web` in a project gets that project as the
/// workspace, but a packaged front end (the Desktop app) spawns the binary
/// with the filesystem root as its cwd — a workspace of `/` is useless, so
/// the fallback is the user's home directory. The decision is factored into
/// [`web_workspace_for`] so it can be tested without touching the process
/// cwd.
fn web_workspace_default() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_default();
    web_workspace_for(&cwd)
}

/// The cwd-vs-home decision of [`web_workspace_default`], on an explicit
/// path: a path with no parent is the filesystem root (`/`, or a drive root
/// on Windows), so the workspace falls back to `$HOME`.
fn web_workspace_for(cwd: &Path) -> PathBuf {
    if cwd.parent().is_none() {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| cwd.to_path_buf())
    } else {
        cwd.to_path_buf()
    }
}

/// Probe the executable's location for a staged web frontend (SPA).
///
/// The Desktop package stages the Rust binary at
/// `<resources>/bin/<platform>-<arch>/mirri` and the SPA at either
/// `<resources>/web` (electron-builder `extraResources`) or
/// `<resources>/bin/<platform>-<arch>/web` (staged alongside the binary
/// before packaging). Both layouts are probed from the executable's own
/// path, so the packaged app needs no `--web-dir` flag. A directory only
/// counts when it contains an `index.html`. The decision is factored into
/// [`probe_web_dir_at`] so it can be tested with a temporary tree.
fn probe_web_dir_from_exe() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe_dir = exe.parent()?;
    probe_web_dir_at(exe_dir)
}

/// The probe of [`probe_web_dir_from_exe`] against an explicit binary
/// directory: `<bin>/web` first (assets staged next to the binary), then
/// `<bin>/../../web` (the Electron resources layout).
fn probe_web_dir_at(exe_dir: &Path) -> Option<PathBuf> {
    let candidates = [
        exe_dir.join("web"),
        exe_dir.join("..").join("..").join("web"),
    ];
    candidates
        .into_iter()
        .find(|dir| dir.join("index.html").is_file())
        // Canonicalize so the `..` segments of the Electron-layout candidate
        // do not leak into the resolved path.
        .and_then(|dir| dir.canonicalize().ok())
}

/// Resolve the `web` subcommand's SPA source: an explicit `--web-dir`
/// first, then the SPA embedded in the binary, then a directory staged
/// next to the executable. `None` means either "serve the embedded SPA"
/// (the server's own fallback handles it — no directory to pass) or, on a
/// build with no embedded SPA and nothing staged, "no web UI at all".
fn resolve_web_dir(
    explicit: Option<PathBuf>,
    embedded_available: bool,
    probed: Option<PathBuf>,
) -> Option<PathBuf> {
    match explicit {
        Some(dir) => Some(dir),
        None if embedded_available => None,
        None => probed,
    }
}

/// The bind address as users (and terminals) click it: an unspecified bind
/// (`0.0.0.0` / `::`) renders as loopback so the printed URL stays
/// reachable on the machine that ran the command; IPv6 literals keep their
/// brackets.
fn display_http_addr(addr: &SocketAddr) -> String {
    match addr.ip() {
        std::net::IpAddr::V4(ip) if ip.is_unspecified() => format!("127.0.0.1:{}", addr.port()),
        std::net::IpAddr::V6(ip) if ip.is_unspecified() => format!("[::1]:{}", addr.port()),
        std::net::IpAddr::V4(ip) => format!("{ip}:{}", addr.port()),
        std::net::IpAddr::V6(ip) => format!("[{ip}]:{}", addr.port()),
    }
}

/// The platform's URL opener program, or `None` where automatic opening is
/// unsupported (Windows support is a follow-up).
fn browser_opener_program() -> Option<&'static str> {
    if cfg!(target_os = "macos") {
        Some("open")
    } else if cfg!(target_os = "linux") {
        Some("xdg-open")
    } else {
        None
    }
}

/// Open the user's browser at `url` — fire-and-forget: a failure (or an
/// unsupported platform) only logs a warning and never affects the server.
/// Called after the listener is bound, so the URL is already reachable.
fn spawn_browser(url: &str) {
    let Some(program) = browser_opener_program() else {
        tracing::warn!("cannot open the browser on this platform; open {url} manually");
        return;
    };
    match tokio::process::Command::new(program).arg(url).spawn() {
        Ok(mut child) => {
            // Reap the opener so it does not linger as a zombie.
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
        }
        Err(error) => {
            tracing::warn!(%error, program, "failed to open the browser; open {url} manually");
        }
    }
}

/// Resolve `host:port` to a concrete socket address, accepting both IP
/// literals and hostnames.
async fn resolve_socket_addr(host: &str, port: u16) -> Result<SocketAddr> {
    let mut resolved = tokio::net::lookup_host((host, port))
        .await
        .with_context(|| format!("failed to resolve bind host '{host}'"))?;
    resolved
        .next()
        .with_context(|| format!("bind host '{host}' resolved to no addresses"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_pick_the_most_recently_opened_registry_row_when_serving_without_a_root() {
        // Given a registry with two rows whose roots exist, the older one
        // touched last
        let home = tempfile::tempdir().expect("home tempdir");
        let older_root = tempfile::tempdir().expect("older root tempdir");
        let newer_root = tempfile::tempdir().expect("newer root tempdir");
        let older_key = mirri_core::encode_work_dir_key(older_root.path());
        let newer_key = mirri_core::encode_work_dir_key(newer_root.path());
        std::fs::write(
            home.path()
                .join(mirri_session_index::WORKSPACE_REGISTRY_FILENAME),
            serde_json::json!({
                "version": 1,
                "workspaces": {
                    older_key.as_str(): {
                        "root": older_root.path().to_string_lossy(),
                        "name": "older",
                        "created_at": "2024-01-01T00:00:00.000Z",
                        "last_opened_at": "2024-01-01T00:00:00.000Z",
                    },
                    newer_key.as_str(): {
                        "root": newer_root.path().to_string_lossy(),
                        "name": "newer",
                        "created_at": "2024-02-01T00:00:00.000Z",
                        "last_opened_at": "2024-06-01T00:00:00.000Z",
                    },
                },
                "deleted_workspace_ids": [],
            })
            .to_string(),
        )
        .expect("the registry is written");

        // When the default serve root is derived from that registry
        let resolved = serve_default_workspace_root_at(
            &home
                .path()
                .join(mirri_session_index::WORKSPACE_REGISTRY_FILENAME),
        );

        // Then the most-recently-opened row's root wins (D5)
        assert_eq!(resolved, mirri_core::lexically_resolve(newer_root.path()));
    }

    #[test]
    fn should_skip_registry_rows_whose_roots_have_vanished_when_serving_without_a_root() {
        // Given a registry whose most-recently-opened row points at a
        // directory that no longer exists, and an older live row
        let home = tempfile::tempdir().expect("home tempdir");
        let live_root = tempfile::tempdir().expect("live root tempdir");
        let gone_root = home.path().join("gone");
        let live_key = mirri_core::encode_work_dir_key(live_root.path());
        let gone_key = mirri_core::encode_work_dir_key(&gone_root);
        std::fs::write(
            home.path()
                .join(mirri_session_index::WORKSPACE_REGISTRY_FILENAME),
            serde_json::json!({
                "version": 1,
                "workspaces": {
                    live_key.as_str(): {
                        "root": live_root.path().to_string_lossy(),
                        "name": "live",
                        "created_at": "2024-01-01T00:00:00.000Z",
                        "last_opened_at": "2024-01-01T00:00:00.000Z",
                    },
                    gone_key.as_str(): {
                        "root": gone_root.to_string_lossy(),
                        "name": "gone",
                        "created_at": "2024-02-01T00:00:00.000Z",
                        "last_opened_at": "2024-06-01T00:00:00.000Z",
                    },
                },
                "deleted_workspace_ids": [],
            })
            .to_string(),
        )
        .expect("the registry is written");

        // When the default serve root is derived from that registry
        let resolved = serve_default_workspace_root_at(
            &home
                .path()
                .join(mirri_session_index::WORKSPACE_REGISTRY_FILENAME),
        );

        // Then the vanished root is never chosen (never re-created, never
        // eagerly built) — the live row serves instead
        assert_eq!(resolved, mirri_core::lexically_resolve(live_root.path()));
    }

    #[test]
    fn should_fall_back_to_the_cwd_when_the_registry_is_empty() {
        // Given a home with no registry file at all
        let home = tempfile::tempdir().expect("home tempdir");

        // When the default serve root is derived from that (empty) registry
        let resolved = serve_default_workspace_root_at(
            &home
                .path()
                .join(mirri_session_index::WORKSPACE_REGISTRY_FILENAME),
        );

        // Then the fallback is the current directory (the pre-W9 behavior)
        let cwd = std::env::current_dir().expect("the cwd is readable");
        assert_eq!(resolved, cwd);
    }

    #[test]
    fn should_fall_back_to_home_when_the_cwd_is_the_filesystem_root() {
        // Given a cwd at the filesystem root (a packaged app inherits `/`)
        // and a HOME pointing into a temporary directory
        let home = std::env::temp_dir().join("mirri-web-workspace-test-home");
        std::fs::create_dir_all(&home).expect("create the test home dir");
        let previous = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", &home) };

        // When the default web workspace is derived from that cwd
        let resolved = web_workspace_for(Path::new("/"));

        // Then the user home is used instead of the root
        assert_eq!(resolved, home);
        match previous {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    #[test]
    fn should_keep_the_cwd_when_it_is_not_the_filesystem_root() {
        // Given a cwd inside a real directory (a project a human runs `web` in)
        let cwd = Path::new("/tmp/some-project");

        // When the default web workspace is derived from that cwd
        let resolved = web_workspace_for(cwd);

        // Then the cwd is kept as the workspace
        assert_eq!(resolved, cwd);
    }

    #[test]
    fn should_find_a_web_dir_staged_next_to_the_binary() {
        // Given a temporary tree with `index.html` inside `<bin>/web`
        let root = std::env::temp_dir().join("mirri-web-dir-probe-test");
        let bin = root.join("bin").join("darwin-arm64");
        let web = bin.join("web");
        std::fs::create_dir_all(&web).expect("create the staged web dir");
        std::fs::write(web.join("index.html"), "<html></html>")
            .expect("write the staged index.html");

        // When the web dir is probed from the binary's directory
        let found = probe_web_dir_at(&bin);

        // Then the staged directory next to the binary is found (canonicalized
        // — macOS resolves the temp dir's `/var` symlink to `/private/var`)
        assert_eq!(found.as_deref(), web.canonicalize().ok().as_deref());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn should_find_a_web_dir_in_the_electron_resources_layout() {
        // Given a temporary tree matching the Electron layout:
        // `<resources>/bin/<target>/mirri` next to `<resources>/web`
        let root = std::env::temp_dir().join("mirri-web-dir-probe-electron-test");
        let bin = root.join("bin").join("darwin-arm64");
        let web = root.join("web");
        std::fs::create_dir_all(&bin).expect("create the bin dir");
        std::fs::create_dir_all(&web).expect("create the resources web dir");
        std::fs::write(web.join("index.html"), "<html></html>")
            .expect("write the resources index.html");

        // When the web dir is probed from the binary's directory
        let found = probe_web_dir_at(&bin);

        // Then the resources-level web directory is found two levels up
        // (canonicalized — macOS resolves the temp dir's `/var` symlink)
        assert_eq!(found.as_deref(), web.canonicalize().ok().as_deref());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn should_not_find_a_web_dir_when_no_staged_spa_exists() {
        // Given a temporary tree containing no `web` directory at all
        let root = std::env::temp_dir().join("mirri-web-dir-probe-empty-test");
        std::fs::create_dir_all(&root).expect("create the empty bin dir");

        // When the web dir is probed from the binary's directory
        let found = probe_web_dir_at(&root);

        // Then no directory is found and the server runs API-only
        assert!(found.is_none(), "got: {found:?}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn should_prefer_an_explicit_web_dir_over_embedded_and_probed_sources() {
        // Given an explicit --web-dir plus a build with an embedded SPA and
        // a staged directory next to the binary
        // When the web dir is resolved
        let resolved = resolve_web_dir(
            Some(PathBuf::from("/explicit/web")),
            true,
            Some(PathBuf::from("/staged/web")),
        );

        // Then the explicit directory wins
        assert_eq!(resolved.as_deref(), Some(Path::new("/explicit/web")));
    }

    #[test]
    fn should_serve_the_embedded_spa_when_no_directory_is_given() {
        // Given a build with an embedded SPA and a directory staged next to
        // the binary, but no --web-dir
        // When the web dir is resolved
        let resolved = resolve_web_dir(None, true, Some(PathBuf::from("/staged/web")));

        // Then no directory is passed — the server's fallback serves the
        // embedded SPA
        assert_eq!(resolved, None);
    }

    #[test]
    fn should_fall_back_to_the_probed_web_dir_when_the_binary_has_no_embedded_spa() {
        // Given a lite build (no embedded SPA) with a directory staged next
        // to the binary
        // When the web dir is resolved
        let resolved = resolve_web_dir(None, false, Some(PathBuf::from("/staged/web")));

        // Then the staged directory is served
        assert_eq!(resolved.as_deref(), Some(Path::new("/staged/web")));
    }

    #[test]
    fn should_resolve_no_web_dir_when_a_lite_build_has_nothing_staged() {
        // Given a lite build (no embedded SPA) with no staged directory
        // When the web dir is resolved
        let resolved = resolve_web_dir(None, false, None);

        // Then no SPA source exists and the server runs API-only
        assert_eq!(resolved, None);
    }

    #[test]
    fn should_render_an_unspecified_bind_as_loopback_in_the_banner_url() {
        // Given a server bound to all interfaces (v4 and v6)
        let v4 = "0.0.0.0:58627".parse::<SocketAddr>().expect("parse v4");
        let v6 = "[::]:58627".parse::<SocketAddr>().expect("parse v6");

        // When the display address is derived
        let v4_display = display_http_addr(&v4);
        let v6_display = display_http_addr(&v6);

        // Then both render as a clickable loopback URL
        assert_eq!(v4_display, "127.0.0.1:58627");
        assert_eq!(v6_display, "[::1]:58627");
    }

    #[test]
    fn should_render_concrete_bind_addresses_verbatim() {
        // Given a server bound to concrete loopback addresses
        let v4 = "127.0.0.1:58627".parse::<SocketAddr>().expect("parse v4");
        let v6 = "[::1]:58627".parse::<SocketAddr>().expect("parse v6");

        // When the display address is derived
        let v4_display = display_http_addr(&v4);
        let v6_display = display_http_addr(&v6);

        // Then the addresses appear verbatim, IPv6 keeping its brackets
        assert_eq!(v4_display, "127.0.0.1:58627");
        assert_eq!(v6_display, "[::1]:58627");
    }

    #[test]
    fn should_delegate_provider_construction_to_the_shared_core_factory() {
        // Given an agent configured for the Google provider with no API key
        // in the environment
        let previous = std::env::var("GOOGLE_API_KEY").ok();
        unsafe { std::env::remove_var("GOOGLE_API_KEY") };
        let config = AgentScopeConfig {
            provider_type: mirri_core::ProviderType::Google,
            ..AgentScopeConfig::default()
        };

        // When the shared core LlmFactory builds the provider (the same
        // factory `mirri serve` now passes straight into ServerState)
        let result = mirri_core::LlmFactory::create_llm(
            &mirri_embedded::ProviderLlmFactory::default(),
            &config,
        );

        // Then the ProviderLlmFactory answered — its Google-specific
        // missing-key error is observable through the authoritative trait,
        // proving the delegation (with a key set, the Google adapter is
        // constructed; the wired branch is covered in mirri-embedded)
        let Err(error) = result else {
            panic!("google provider must not be constructible without a key");
        };
        assert!(error.contains("GOOGLE_API_KEY"), "got: {error}");
        if let Some(previous) = previous {
            unsafe { std::env::set_var("GOOGLE_API_KEY", previous) };
        }
    }
}
