//! The shared ACP host entry point.
//!
//! [`run`] owns the whole host lifecycle — stderr logging init, `--version`
//! handling, engine selection, and the serve loop — so the `mirri acp`
//! subcommand and the standalone `mirri-acp` binary thin-re-export the exact
//! same behavior. Logging must init to stderr *here* (stdout is the protocol
//! channel of the host, so nothing may write to it outside JSON-RPC frames),
//! which is also why the caller must not initialize the server's own
//! dual-sink subscriber before calling [`run`].

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::{AcpHandler, StdioTransport};
use mirri_core::WorkspaceId;
use mirri_transport::adapter::EngineAdapter;
use mirri_transport::frame::{JsonRpcError, JsonRpcFrame, JsonRpcResponse};
use mirri_transport::transport::{Transport, TransportError};

/// How long the host waits for the client to answer a
/// `session/request_permission` request before falling back to a rejected
/// approval — earlier than the engine's turn stall timeout, so an
/// unresponsive client denies the tool instead of hanging the turn.
/// Test knobs can shorten it through `MIRRI_ACP_PERMISSION_TIMEOUT_MS`.
const PERMISSION_REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// The effective permission-request timeout ([`PERMISSION_REQUEST_TIMEOUT`]
/// unless `MIRRI_ACP_PERMISSION_TIMEOUT_MS` overrides it — a test knob for
/// exercising the fallback without waiting a minute).
fn permission_request_timeout() -> Duration {
    std::env::var("MIRRI_ACP_PERMISSION_TIMEOUT_MS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(PERMISSION_REQUEST_TIMEOUT)
}

/// Run the ACP host on stdin/stdout until stdin closes.
///
/// This is the single entry point behind both `mirri acp` (the primary
/// user-facing path) and the standalone `mirri-acp` binary (retained as a
/// thin compatibility re-export for the conformance tests). All logging
/// goes to stderr — stdout carries only JSON-RPC frames.
pub async fn run() -> anyhow::Result<()> {
    // Initialize logging to stderr (stdout is the protocol channel).
    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    if std::env::args().any(|arg| arg == "--version") {
        // Only serves the standalone compatibility binary: when run as the
        // `mirri acp` subcommand, clap answers `--version` before `run()` is
        // reached, so this branch never fires there.
        println!("mirri-acp {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    tracing::info!("mirri-acp starting (pid={})", std::process::id());

    // The engine backing the ACP host, resolved by the design-14 §D3
    // priority chain (explicit remote > explicit embedded >
    // workspace-dir semaphore > auto-probe): the embedded engine rooted
    // at the workspace directory; a remote `mirri-server` engine when
    // forced by `MIRRI_ACP_REMOTE_ENGINE` or found by the desktop probe.
    // The remote engine speaks the unified JSON-RPC protocol through
    // `mirri-remote`, with the approval/question reverse-RPC domains
    // negotiated at the handshake.
    let remote_engine = match resolve_engine_choice() {
        // Priority 1: the explicit remote URL (byte-compatible with the
        // pre-design-14 behavior — the AllSessions policy and the token
        // precedence `MIRRI_ACP_REMOTE_TOKEN` > server.token file are
        // preserved exactly).
        EngineChoice::ForcedRemote { url } => {
            let token = std::env::var("MIRRI_ACP_REMOTE_TOKEN")
                .ok()
                .or_else(read_server_token_file);
            tracing::info!(url = %url, "mirri-acp using REMOTE engine");
            let adapter = mirri_remote::RemoteAdapter::connect(
                &url,
                token.as_deref(),
                &["approval", "question"],
            )
            .await
            .map_err(|error| anyhow::anyhow!("remote engine connect failed ({url}): {error}"))?;
            Some(adapter as Arc<dyn EngineAdapter>)
        }
        // Priorities 2-3: the explicit embedded override and the
        // workspace-dir semaphore — both serve embedded, no probing.
        EngineChoice::Embedded => None,
        // Priority 4: auto-probe (design 14 §D1) — the state-file-anchored
        // desktop probe with the handshake version gate; any failure
        // falls back to the embedded engine (never an error).
        EngineChoice::ProbeDesktop => match probe_and_connect_desktop().await {
            Some(engine) => Some(engine),
            None => {
                tracing::info!("no live desktop server found; mirri-acp using embedded engine");
                None
            }
        },
    };

    if let Some(engine) = remote_engine {
        let transport = Arc::new(StdioTransport::new());
        // The ACP host's start root (design 14 D6): the directory this
        // process was started in (the IDE launches the host at its project
        // root) — the root the remote-mode `session/list` pin translates
        // to the workspace it filters the server's answer down to.
        // `MIRRI_ACP_WORKSPACE_DIR` wins when set (the same source the
        // embedded branch roots its engine at — the test semaphore is
        // respected by the probe-disable chain above, not by this read).
        let host_start_root = std::env::var("MIRRI_ACP_WORKSPACE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                std::env::current_dir().expect("current working directory must be readable")
            });
        let handler = Arc::new(AcpHandler::with_remote_engine(engine, host_start_root));
        // The design-14 D3 replacement machinery: a transport-class
        // engine error (the desktop server exited) triggers one
        // re-probe — success swaps in a fresh remote adapter, failure
        // degrades the slot to the embedded engine (one-way). The
        // probe closure is sync by contract, so the async connect runs
        // on a dedicated thread's runtime (never block_on inside the
        // caller's tokio worker — that could deadlock the serve loop).
        let embedded_factory = build_embedded_engine_factory();
        handler.set_engine_replacement_path(
            Arc::new(|| {
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .ok();
                    let engine = runtime.as_ref().and_then(|runtime| {
                        runtime.block_on(async {
                            probe_and_connect_desktop()
                                .await
                                .map(|engine| engine as Arc<dyn EngineAdapter>)
                        })
                    });
                    let _ = tx.send(engine);
                    // The connected adapter's background tasks (the socket
                    // reader/writer, the event pump) live on THIS thread's
                    // runtime: dropping the runtime would abort them and
                    // hand the slot a dead engine — the thread therefore
                    // never finishes, driving the runtime's tasks for the
                    // rest of the process. `block_on(pending)` (not
                    // `thread::park`, which would starve the runtime's
                    // spawned tasks) keeps the event loop turning. One
                    // thread per successful re-probe; the ACP process is
                    // short-lived enough that this is not a leak.
                    if let Some(runtime) = runtime {
                        runtime.block_on(std::future::pending::<()>());
                    }
                });
                rx.recv().ok().flatten()
            }),
            embedded_factory,
        );
        serve(&transport, &handler).await?;
        tracing::info!("mirri-acp shutting down (stdin closed)");
        return Ok(());
    }

    // The embedded engine: fully-wired (config.toml, MCP servers,
    // plugins, and agent profiles auto-discovered), with the
    // config-aware LLM factory attached so prompts run turns on the
    // configured `[models]` aliases, with provider credentials
    // injectable through the environment (ANTHROPIC_API_KEY /
    // OPENAI_API_KEY — the provider-resolution layer under each
    // configured alias). Rooted at the workspace directory:
    // `MIRRI_ACP_WORKSPACE_DIR` when set (tests point it at a
    // throwaway tempdir), the current working directory otherwise.
    let workspace_dir = std::env::var("MIRRI_ACP_WORKSPACE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::current_dir().expect("current working directory must be readable")
        });
    tracing::info!("mirri-acp using embedded engine");
    let default_engine: Arc<dyn EngineAdapter> =
        Arc::new(mirri_embedded::EmbeddedServer::for_workspace_root(
            WorkspaceId::new("acp"),
            workspace_dir.clone(),
            None,
            Vec::new(),
        ));
    // The per-cwd engine pool: every `session/new` / `session/load`
    // carrying a `cwd` routes to the engine cached under that
    // (canonicalized) directory — same-cwd sessions share it, so the
    // engine's workspace-rooted FS / permission scope stays per-cwd
    // without ever making the engine's sessions cwd-aware. A request
    // whose cwd is the default root reuses the default engine
    // instead of minting a duplicate over the same directory.
    let default_root = std::fs::canonicalize(&workspace_dir).unwrap_or(workspace_dir);
    let engine_for_pool = Arc::clone(&default_engine);
    let factory_root = default_root.clone();
    let factory: crate::EngineFactory = Arc::new(move |cwd: &std::path::Path| {
        if cwd == factory_root {
            return Arc::clone(&engine_for_pool);
        }
        Arc::new(mirri_embedded::EmbeddedServer::for_workspace_root(
            WorkspaceId::new("acp"),
            cwd.to_path_buf(),
            None,
            Vec::new(),
        )) as Arc<dyn EngineAdapter>
    });
    let transport = Arc::new(StdioTransport::new());
    let handler = Arc::new(AcpHandler::with_engine_pool(
        Arc::clone(&default_engine),
        factory,
    ));
    // The default engine's root is what its sessions list as their
    // cwd and where `session/delete` removes the persisted files
    // (the pool mint records every factory engine itself).
    handler.remember_engine_cwd(&default_engine, default_root);
    serve(&transport, &handler).await?;
    tracing::info!("mirri-acp shutting down (stdin closed)");
    Ok(())
}

/// Read `<mirri-home>/server.token` — the fallback bearer credential of
/// the explicit-remote mode (the same file the desktop probe and the
/// web UI read; `None` when missing, letting the connect fail with its
/// own auth error).
fn read_server_token_file() -> Option<String> {
    let home = mirri_core::config::mirri_home_dir().ok()?;
    let token = std::fs::read_to_string(home.join("server.token")).ok()?;
    let token = token.trim().to_string();
    if token.is_empty() { None } else { Some(token) }
}

/// Probe for a live local desktop server (design 14 §D1) and connect to
/// it with the ACP subscription policy (design 14 §D5 — no connect-time
/// mass subscribe). The handshake's `server_hello.version` is gated
/// against [`crate::desktop_probe::MIN_SERVER_VERSION`]; a probe
/// failure, a connect failure, or a version below the floor all fall
/// back to `None` (embedded).
async fn probe_and_connect_desktop() -> Option<Arc<dyn EngineAdapter>> {
    let endpoint = crate::desktop_probe::probe_desktop_server().await?;
    let adapter = mirri_remote::RemoteAdapter::connect_with_policy(
        &endpoint.url,
        Some(&endpoint.token),
        &["approval", "question"],
        mirri_remote::SubscribePolicy::OwnSessionsOnly,
    )
    .await
    .map_err(|error| {
        tracing::warn!(url = %endpoint.url, %error, "desktop server probe: connect failed");
    })
    .ok()?;
    let version = adapter.caps().map(|caps| caps.version).unwrap_or_default();
    if !crate::desktop_probe::version_satisfies(&version, crate::desktop_probe::MIN_SERVER_VERSION)
    {
        tracing::warn!(
            server_version = %version,
            min_version = crate::desktop_probe::MIN_SERVER_VERSION,
            "desktop server probe: version below the ACP host's floor; using embedded engine"
        );
        let _ = adapter.dispose().await;
        return None;
    }
    tracing::info!(
        url = %endpoint.url,
        "mirri-acp sharing the desktop server engine"
    );
    Some(adapter)
}

/// The embedded fallback factory of the degrade path (design 14 D3): the
/// engine a dead remote connection degrades to. Home injection (the
/// ADR-019 batch) is NOT applied here — it lands as an independently
/// deliverable change; today's form stays `for_workspace_root(..., None,
/// ...)` rooted at the startup directory.
fn build_embedded_engine_factory() -> crate::acp_handler::EmbeddedEngineFactory {
    let workspace_dir = std::env::var("MIRRI_ACP_WORKSPACE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::current_dir().expect("current working directory must be readable")
        });
    Arc::new(move || {
        Arc::new(mirri_embedded::EmbeddedServer::for_workspace_root(
            WorkspaceId::new("acp"),
            workspace_dir.clone(),
            None,
            Vec::new(),
        )) as Arc<dyn EngineAdapter>
    })
}

/// Read frames from the transport and dispatch them to the handler until
/// stdin closes, sending one response per request and relaying ACP
/// `session/update` notifications as they stream from the engine.
///
/// JSON-RPC notifications (frames without an `id`) are processed but never
/// answered — in particular `session/cancel` and `$/cancel_request`, which
/// ACP / JSON-RPC define as notifications carrying no response.
///
/// Each client request is handled on its own task: a `session/prompt` turn
/// runs to completion before its response, and the engine may need the
/// client to answer a `session/request_permission` request *while that turn
/// is still running* (the approval reverse-RPC bridge). Those outbound
/// requests are pumped onto the transport and resolved when the matching
/// response frame comes back (the transport's pending-request table).
async fn serve(transport: &Arc<StdioTransport>, handler: &Arc<AcpHandler>) -> anyhow::Result<()> {
    let mut updates = handler.subscribe_updates();
    let mut client_requests = handler.subscribe_client_requests();
    loop {
        tokio::select! {
            frame = transport.recv() => {
                match frame {
                    Ok(JsonRpcFrame::Request(request)) => {
                        tracing::debug!(method = %request.method, id = ?request.id, "request received");
                        let is_notification = request.id.is_none();
                        let request_id = request.id;
                        let response_id = request_id.clone();
                        let method = request.method.clone();
                        let transport = Arc::clone(transport);
                        let handler = Arc::clone(handler);
                        // Handle concurrently: the loop must stay free to pump
                        // the engine's reverse-RPC requests (and to process
                        // `session/cancel` / `$/cancel_request`) while a
                        // prompt turn runs. The client-facing request id
                        // rides along, so `$/cancel_request` can name the
                        // in-flight prompt it wants aborted.
                        tokio::spawn(async move {
                            let response = match handler
                                .handle_request(request_id, &method, request.params)
                                .await
                            {
                                Ok(result) => JsonRpcResponse {
                                    id: response_id,
                                    result: Some(result),
                                    error: None,
                                },
                                Err((code, message)) => JsonRpcResponse {
                                    id: response_id,
                                    result: None,
                                    error: Some(JsonRpcError {
                                        code,
                                        message,
                                        data: None,
                                    }),
                                },
                            };
                            if is_notification {
                                // A request without an id is a JSON-RPC
                                // notification: process it but never reply
                                // (e.g. `session/cancel`).
                                tracing::debug!(method, "request without id processed as notification; no response sent");
                            } else {
                                let _ = transport
                                    .send(JsonRpcFrame::Response(response))
                                    .await;
                            }
                        });
                    }
                    Ok(JsonRpcFrame::Notification(notification)) => {
                        // Notifications never receive a response, but
                        // `session/cancel` and `$/cancel_request` still
                        // carry work: the former aborts the addressed
                        // session's in-flight turns, the latter aborts the
                        // in-flight request its `requestId` names.
                        if notification.method == "session/cancel"
                            || notification.method == "$/cancel_request"
                        {
                            tracing::debug!(
                                method = %notification.method,
                                "cancellation notification processed (no response)"
                            );
                            let _ = handler
                                .handle(&notification.method, notification.params)
                                .await;
                        } else {
                            tracing::debug!(method = %notification.method, "notification received (no response)");
                        }
                    }
                    Ok(JsonRpcFrame::Response(frame)) => {
                        // Responses matching a parked outbound request are
                        // resolved inside the transport and never surface
                        // here; anything else is a protocol violation (the
                        // host only expects responses to the
                        // `session/request_permission` requests it issues).
                        tracing::warn!("received a response frame from the client; ignoring: {frame:?}");
                    }
                    Err(TransportError::Closed) => break,
                    Err(TransportError::Parse(error)) => {
                        // A request whose text cannot be parsed is answered with a
                        // -32700 error and a null id (JSON-RPC 2.0 §4.2).
                        tracing::debug!(%error, "request failed to parse");
                        transport.send(parse_error_response()).await?;
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            update = updates.recv() => {
                match update {
                    Ok(notification) => {
                        tracing::debug!(method = %notification.method, "relaying update notification");
                        transport.send(JsonRpcFrame::Notification(notification)).await?;
                    }
                    Err(error) => {
                        // A lagged receiver keeps streaming (P5e decision
                        // B): no bounded blocking, the dropped batch is
                        // logged; only channel closure ends the relay.
                        if !handle_update_relay_failure(error) {
                            break;
                        }
                    }
                }
            }
            request = client_requests.recv() => {
                match request {
                    Some(pending) => {
                        tracing::debug!(method = %pending.method, "relaying reverse-RPC request to the client");
                        let method = pending.method.clone();
                        let transport = Arc::clone(transport);
                        let params = pending.params.clone();
                        let timeout = permission_request_timeout();
                        tokio::spawn(async move {
                            let result = transport
                                .request(&method, params, timeout)
                                .await;
                            pending.respond(result);
                        });
                    }
                    None => {
                        // Every handler is gone (shutdown); nothing to pump.
                    }
                }
            }
        }
    }
    Ok(())
}

/// Build the JSON-RPC 2.0 parse-error response (`-32700`, null id).
fn parse_error_response() -> JsonRpcFrame {
    JsonRpcFrame::Response(JsonRpcResponse {
        id: None,
        result: None,
        error: Some(JsonRpcError {
            code: -32700,
            message: "parse error".to_string(),
            data: None,
        }),
    })
}

/// Handle one failed `session/update` receive on the relay (the serve
/// loop's lag branch).
///
/// A lag means the engine outran the update channel and `skipped`
/// notifications were dropped — with the 1024-slot capacity this only
/// happens to a genuinely slow consumer, and the loss is deliberately
/// *loud* (a `tracing::warn!`, never a silent continue): the durable
/// session history lives in the wire log, so a client that cares can
/// `session/load` to recover what the UI missed. There is **no bounded
/// blocking** (P5e decision B): backpressuring the relay would cascade
/// through the engine's event forwarder and stall the whole turn for
/// every slow client, which is strictly worse than losing UI chunks.
///
/// Returns whether the relay should keep streaming — `false` only when
/// the channel closed (every producer is gone; shutdown).
fn handle_update_relay_failure(error: tokio::sync::broadcast::error::RecvError) -> bool {
    match error {
        tokio::sync::broadcast::error::RecvError::Lagged(skipped) => {
            tracing::warn!(
                skipped,
                "session/update relay lagged: {skipped} update notification(s) dropped \
                 (the client fell behind the update channel); the stream resumes with the \
                 next notification and session/load replays any lost history"
            );
            true
        }
        tokio::sync::broadcast::error::RecvError::Closed => false,
    }
}

/// How the host chooses its engine form at startup (design 14 §D3): the
/// priority chain is fixed — explicit remote > explicit embedded >
/// workspace-dir semaphore (no probing) > auto-probe the desktop server.
///
/// [`EngineChoice::Embedded`] covers both the forced and the fallback
/// shapes: the resolution chain only decides **whether** to probe, the
/// probe outcome decides between remote and embedded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EngineChoice {
    /// `MIRRI_ACP_REMOTE_ENGINE` is set — force remote (highest
    /// priority; byte-compatible with the pre-design-14 behavior).
    ForcedRemote { url: String },
    /// `MIRRI_ACP_ENGINE=embedded`, `MIRRI_ACP_WORKSPACE_DIR` set, or no
    /// probe override applies — serve with the embedded engine.
    Embedded,
    /// No override applies — probe for a live local desktop server and
    /// fall back to embedded when the probe fails.
    ProbeDesktop,
}

/// Resolve the startup engine form from the environment (design 14 §D3
/// priority chain — a pure function over env vars so the chain is
/// unit-testable):
///
/// 1. `MIRRI_ACP_REMOTE_ENGINE` set → forced remote (priority 1 wins
///    over every later rule, including `MIRRI_ACP_ENGINE=embedded`).
/// 2. `MIRRI_ACP_ENGINE=embedded` → forced embedded (no probing).
/// 3. `MIRRI_ACP_WORKSPACE_DIR` set → embedded (the test semaphore:
///    tests point it at a throwaway tempdir, so probing would break
///    their hermeticity — design 14 §D3 / acceptance 4).
/// 4. Otherwise → probe the desktop server, falling back to embedded.
pub(crate) fn resolve_engine_choice() -> EngineChoice {
    if let Ok(url) = std::env::var("MIRRI_ACP_REMOTE_ENGINE") {
        return EngineChoice::ForcedRemote { url };
    }
    if std::env::var("MIRRI_ACP_ENGINE")
        .ok()
        .is_some_and(|value| value == "embedded")
    {
        return EngineChoice::Embedded;
    }
    if std::env::var_os("MIRRI_ACP_WORKSPACE_DIR").is_some() {
        return EngineChoice::Embedded;
    }
    EngineChoice::ProbeDesktop
}

#[cfg(test)]
mod tests {
    use super::*;
    use mirri_transport::frame::JsonRpcNotification;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    /// Serializes tests that mutate process environment variables.
    ///
    /// `std::env::set_var`/`remove_var` are `unsafe` in edition 2024, and
    /// environment reads/writes race across threads when tests run in
    /// parallel, so every env-mutating test holds this lock and restores
    /// the previous value on drop (the same discipline as
    /// `desktop_probe`'s tests).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Saves the current value of every variable the engine-choice
    /// resolution reads and clears them for the test, restoring the
    /// previous values on drop so tests never leak env state.
    struct EnvGuard {
        remote_engine: Option<String>,
        remote_token: Option<String>,
        engine: Option<String>,
        workspace_dir: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn new() -> Self {
            let guard = Self {
                remote_engine: std::env::var("MIRRI_ACP_REMOTE_ENGINE").ok(),
                remote_token: std::env::var("MIRRI_ACP_REMOTE_TOKEN").ok(),
                engine: std::env::var("MIRRI_ACP_ENGINE").ok(),
                workspace_dir: std::env::var_os("MIRRI_ACP_WORKSPACE_DIR"),
            };
            // SAFETY: guarded by ENV_LOCK and restored in Drop; no other
            // thread reads or writes these variables concurrently.
            unsafe {
                std::env::remove_var("MIRRI_ACP_REMOTE_ENGINE");
                std::env::remove_var("MIRRI_ACP_REMOTE_TOKEN");
                std::env::remove_var("MIRRI_ACP_ENGINE");
                std::env::remove_var("MIRRI_ACP_WORKSPACE_DIR");
            }
            guard
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: same discipline as the clears — ENV_LOCK held for
            // the whole test, no concurrent readers.
            unsafe {
                if let Some(url) = &self.remote_engine {
                    std::env::set_var("MIRRI_ACP_REMOTE_ENGINE", url);
                }
                if let Some(token) = &self.remote_token {
                    std::env::set_var("MIRRI_ACP_REMOTE_TOKEN", token);
                }
                if let Some(engine) = &self.engine {
                    std::env::set_var("MIRRI_ACP_ENGINE", engine);
                }
                if let Some(dir) = &self.workspace_dir {
                    std::env::set_var("MIRRI_ACP_WORKSPACE_DIR", dir);
                }
            }
        }
    }

    /// Given `MIRRI_ACP_REMOTE_ENGINE` is set (and even with
    /// `MIRRI_ACP_ENGINE=embedded` set alongside it), when resolving,
    /// then the choice is forced remote — priority 1 wins over priority
    /// 2 (design 14 §D3; the explicit override must stay byte-compatible
    /// with the pre-probe behavior).
    #[test]
    fn should_prefer_explicit_remote_engine_over_everything() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::new();
        // SAFETY: ENV_LOCK held; restored by EnvGuard's Drop.
        unsafe {
            std::env::set_var("MIRRI_ACP_REMOTE_ENGINE", "ws://127.0.0.1:58627/api/v1/rpc");
            std::env::set_var("MIRRI_ACP_ENGINE", "embedded");
        }
        assert_eq!(
            resolve_engine_choice(),
            EngineChoice::ForcedRemote {
                url: "ws://127.0.0.1:58627/api/v1/rpc".to_string()
            },
            "the explicit remote URL outranks the embedded override"
        );
    }

    /// Given `MIRRI_ACP_ENGINE=embedded` and no remote URL, when
    /// resolving, then the choice is embedded — the explicit opt-out
    /// skips probing entirely (design 14 §D3).
    #[test]
    fn should_force_embedded_when_engine_env_says_so() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::new();
        // SAFETY: ENV_LOCK held; restored by EnvGuard's Drop.
        unsafe { std::env::set_var("MIRRI_ACP_ENGINE", "embedded") };
        assert_eq!(resolve_engine_choice(), EngineChoice::Embedded);
    }

    /// Given `MIRRI_ACP_WORKSPACE_DIR` set (and no higher-priority
    /// override), when resolving, then the choice is embedded — the
    /// workspace-dir semaphore disables probing so tests stay hermetic
    /// (design 14 §D3 / acceptance 4).
    #[test]
    fn should_skip_probing_when_workspace_dir_is_set() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::new();
        // SAFETY: ENV_LOCK held; restored by EnvGuard's Drop.
        unsafe { std::env::set_var("MIRRI_ACP_WORKSPACE_DIR", "/tmp/some-workspace") };
        assert_eq!(resolve_engine_choice(), EngineChoice::Embedded);
    }

    /// Given no override variable applies, when resolving, then the
    /// choice is to probe — the new default path of design 14 §2.
    #[test]
    fn should_probe_only_when_no_override_applies() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::new();
        assert_eq!(resolve_engine_choice(), EngineChoice::ProbeDesktop);
    }

    /// A capture writer for [`tracing_subscriber`] — the same pattern
    /// main.rs uses to assert the relay's warnings: a warn-level log line
    /// lands in the shared buffer the assertion reads back.
    #[derive(Default, Clone)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl SharedBuf {
        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone())
                .expect("the captured log is valid UTF-8")
        }
    }

    impl std::io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuf {
        type Writer = SharedBuf;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Given an update channel one notification past its capacity, when
    /// the relay's receive fails with the lag, then the failure handler
    /// warns (never a silent drop), keeps the relay streaming, and the
    /// stream resumes with the oldest retained notification.
    #[test]
    fn should_warn_and_keep_streaming_when_the_update_relay_lags() {
        let buf = SharedBuf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("test runtime must build")
                .block_on(async {
                    let (tx, mut rx) = tokio::sync::broadcast::channel(2);
                    let notification = |seq: i64| JsonRpcNotification {
                        method: "session/update".to_string(),
                        params: json!({"seq": seq}),
                    };
                    for seq in 0..3 {
                        tx.send(notification(seq))
                            .expect("a subscribed receiver takes every send");
                    }
                    let error = rx
                        .recv()
                        .await
                        .expect_err("a capacity-2 channel lags after 3 sends");
                    assert!(
                        handle_update_relay_failure(error),
                        "a lag keeps the relay streaming (the next notification resumes)"
                    );
                    assert!(
                        rx.recv().await.is_ok(),
                        "the stream must resume with the oldest retained notification"
                    );
                });
        });
        let logged = buf.contents();
        assert!(
            logged.contains("session/update"),
            "the lag must warn loudly, not pass silently: {logged:?}"
        );
        assert!(
            logged.contains("dropped"),
            "the warn names the dropped batch: {logged:?}"
        );
    }

    /// Given a closed update channel, when the relay's receive fails with
    /// closure, then the failure handler ends the relay.
    #[tokio::test]
    async fn should_stop_the_relay_when_the_update_channel_closes() {
        let (tx, mut rx) = tokio::sync::broadcast::channel::<JsonRpcNotification>(2);
        drop(tx);
        let error = rx
            .recv()
            .await
            .expect_err("a dropped sender closes the channel");
        assert!(
            !handle_update_relay_failure(error),
            "channel closure ends the relay"
        );
    }
}
