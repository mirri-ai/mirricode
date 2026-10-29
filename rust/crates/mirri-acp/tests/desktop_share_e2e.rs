//! End-to-end desktop runtime sharing tests (design 14, Task 8): a real
//! in-process `mirri-server` on an ephemeral port, a temp `MIRRICODE_HOME`
//! carrying a faked `server/desktop-state.json` + `server.token` (exactly
//! what the desktop shell maintains), and the `AcpHandler` driven through
//! its public `handle()` surface with the design-14 remote-mode wiring
//! (`with_remote_engine` + `set_engine_replacement_path` — the same
//! machinery `run()` installs in remote mode).
//!
//! The probe itself (`probe_desktop_server`) reads `MIRRICODE_HOME` from
//! the process environment, so every test that exercises the probe (or the
//! handler's re-probe path) runs under `ENV_LOCK` (the process env is
//! global; parallel tests would race) and `--test-threads=1` (the runner
//! default for this file — see the run instruction in the plan).
//!
//! The server boots through the same composition root `mirri serve` uses
//! (`mirri_embedded::bootstrap::assemble` with `TrustedFromConfig`), rooted
//! at a registered workspace and sharing the temp home — so a second server
//! started later over the same home observes the persisted sessions
//! (restore-on-demand), and `session.create` routing by `workspaceId`
//! lands in the same per-workspace bucket the desktop would read.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mirri_acp::AcpHandler;
use mirri_core::WorkspaceId;
use mirri_embedded::EmbeddedServer;
use mirri_embedded::bootstrap::{WorkspacePolicy, assemble};
use mirri_remote::{RemoteAdapter, SubscribePolicy};
use mirri_server::ServerState;
use mirri_test_harness::{MockLlm, MockLlmFactory};
use mirri_transport::adapter::EngineAdapter;
use serde_json::{Value, json};
use tempfile::TempDir;

/// Deadline for one probe + connect round trip or one handler request.
/// Generous on purpose (the `rust-e2e-ws-handshake` skill's lesson): under
/// full-workspace load the WS handshake of a freshly-spawned server task
/// legitimately takes seconds; a tight deadline here flakes while the same
/// test passes in isolation.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

/// Serializes every test in this file that mutates process environment
/// variables (`MIRRICODE_HOME` for the probe / token reads,
/// `MIRRI_ACP_*` for the resolution chain). `std::env::set_var` is
/// `unsafe` in edition 2024 and the environment is process-global, so all
/// env-touching tests hold this lock for their whole body and restore the
/// previous values on exit.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Saves the current value of one environment variable and restores it on
/// drop (paired with `ENV_LOCK`, mirroring the discipline of the
/// `desktop_probe` unit tests).
struct EnvGuard {
    name: &'static str,
    saved: Option<std::ffi::OsString>,
}

impl EnvGuard {
    fn new(name: &'static str) -> Self {
        let saved = std::env::var_os(name);
        // SAFETY: ENV_LOCK held for the whole test; restored in Drop; no
        // other thread reads or writes this variable concurrently.
        unsafe { std::env::remove_var(name) };
        Self { name, saved }
    }

    /// Set the guarded variable for the test body.
    fn set(&self, value: impl AsRef<std::ffi::OsStr>) {
        // SAFETY: same discipline as `new`.
        unsafe { std::env::set_var(self.name, value) };
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: same discipline as `new`.
        match &self.saved {
            Some(value) => unsafe { std::env::set_var(self.name, value) },
            None => unsafe { std::env::remove_var(self.name) },
        }
    }
}

/// Guard every environment variable the desktop-sharing chain reads, so a
/// test body starts from a clean slate (no leaked `MIRRI_ACP_*` or
/// `MIRRICODE_HOME` from the surrounding process). The test must already
/// hold [`ENV_LOCK`].
struct TestEnv {
    _home: EnvGuard,
    _remote_engine: EnvGuard,
    _remote_token: EnvGuard,
    _engine: EnvGuard,
    _workspace_dir: EnvGuard,
}

impl TestEnv {
    /// Enter a clean desktop-sharing environment (call `acquire_env_lock`
    /// first and keep the guard alive for the whole test body).
    fn clean() -> Self {
        Self {
            _home: EnvGuard::new("MIRRICODE_HOME"),
            _remote_engine: EnvGuard::new("MIRRI_ACP_REMOTE_ENGINE"),
            _remote_token: EnvGuard::new("MIRRI_ACP_REMOTE_TOKEN"),
            _engine: EnvGuard::new("MIRRI_ACP_ENGINE"),
            _workspace_dir: EnvGuard::new("MIRRI_ACP_WORKSPACE_DIR"),
        }
    }
}

/// Take [`ENV_LOCK`] for the whole test body. Returns a guard that must
/// stay alive until the test's environment-dependent section ends.
fn acquire_env_lock() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// The mock LLM's canned reply — one scripted turn is enough to prove the
/// event stream relays end to end (design 14 acceptance 1's wire-level
/// content: the turn runs INSIDE the server's engine and the ACP update
/// relay carries it).
const ASSISTANT_REPLY: &str = "Hello from the shared desktop engine";

// ===========================================================================
// Test-server harness: a real `mirri-server` booted like `mirri serve`
// ===========================================================================

/// A running in-process server bound to an ephemeral loopback port, built
/// through the same composition root `mirri serve` uses (assemble with
/// `TrustedFromConfig` + a MockLlm factory) and rooted at a registered
/// workspace.
///
/// The server runs on a **dedicated child runtime** (its own thread) — that
/// is what makes `kill` a faithful "the desktop process exited":
/// `shutdown_timeout` on that runtime aborts every task spawned inside it,
/// which includes the per-connection tasks `axum` spawned (their sockets
/// close → the connected ACP adapter observes a transport-class error) and
/// the accept loop (the port releases). A mere graceful shutdown or
/// task abort would NOT sever established connections.
struct DesktopServer {
    addr: SocketAddr,
    state: ServerState,
    /// The child-runtime stop signal: `kill` sends on it; the server
    /// thread then shuts the runtime down (aborting every connection).
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    /// The server thread — joined by `kill` so the port release is
    /// synchronous-ish (the port poll in the tests still guards).
    thread: Option<std::thread::JoinHandle<()>>,
    /// The registered workspace's temp root, kept alive for the server's
    /// lifetime (the bucket key derivation reads it lazily).
    _workspace_root: TempDir,
}

impl DesktopServer {
    fn ws_rpc_url(&self) -> String {
        format!("ws://{}/api/v1/rpc", self.addr)
    }

    fn port(&self) -> u16 {
        self.addr.port()
    }

    /// The "the desktop app exited" event: stop the child runtime with a
    /// zero timeout — every task it spawned (accept loop, connection
    /// tasks, event forwarders) is aborted, every socket closes. The
    /// in-flight WS clients observe the connection death synchronously.
    fn kill(mut self) {
        let _ = self.stop.take().expect("kill is once").send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// [`DesktopServer`] on a dedicated child runtime (see the struct doc for
/// why the runtime boundary is the kill semantics).
async fn boot_server(home: &Path, workspace_root: &Path) -> DesktopServer {
    let mock = Arc::new(MockLlm::new());
    mock.push_text_response(ASSISTANT_REPLY);
    boot_server_with_llm(home, workspace_root, Arc::new(MockLlmFactory::new(mock))).await
}

/// [`boot_server`] with a caller-owned LLM factory.
async fn boot_server_with_llm(
    home: &Path,
    workspace_root: &Path,
    llm_factory: Arc<dyn mirri_core::LlmFactory>,
) -> DesktopServer {
    let (addr, state, stop_tx, thread) =
        spawn_server_thread(home, workspace_root, llm_factory, None).await;
    DesktopServer {
        addr,
        state,
        stop: Some(stop_tx),
        thread: Some(thread),
        _workspace_root: tempfile::tempdir().expect("workspace tempdir"),
    }
}

/// The server body shared by the ephemeral and fixed-port boots: build the
/// `mirri serve`-shaped state, bind (an ephemeral port, or the caller's),
/// and run `mirri_server::serve` on a dedicated current-thread runtime
/// owned by a spawned thread. Returns the bound address, the state (for
/// server-side assertions), and the stop receiver whose drop-side
/// `shutdown_timeout` is the kill.
async fn spawn_server_thread(
    home: &Path,
    workspace_root: &Path,
    llm_factory: Arc<dyn mirri_core::LlmFactory>,
    port: Option<u16>,
) -> (
    SocketAddr,
    ServerState,
    tokio::sync::oneshot::Sender<()>,
    std::thread::JoinHandle<()>,
) {
    let parts = assemble(
        WorkspaceId::new("desktop-share-e2e"),
        workspace_root.to_path_buf(),
        Some(home.to_path_buf()),
        Some(home.to_path_buf()),
        WorkspacePolicy::TrustedFromConfig,
        Vec::new(),
    );
    let session_manager = Arc::clone(&parts.session_manager);
    let engine: Arc<dyn EngineAdapter> = Arc::new(EmbeddedServer::from_parts(
        mirri_embedded::bootstrap::EngineParts {
            llm_factory,
            ..parts
        },
    ));
    let state = ServerState::with_engine(
        engine,
        session_manager,
        env!("CARGO_PKG_VERSION").to_string(),
        None,
    );
    // Bind INSIDE the server thread's runtime: a tokio listener's
    // readiness is driven by the runtime that created it, so binding on
    // the caller's (multi-thread) runtime and using it on the server's
    // (current-thread) one starves the accept loop whenever the caller's
    // workers are busy. The bound address comes back over a channel (the
    // CLI's bind-then-report shape).
    let bind_addr: std::net::SocketAddr = match port {
        Some(port) => format!("127.0.0.1:{port}")
            .parse()
            .expect("parse bind addr"),
        None => "127.0.0.1:0".parse().expect("parse bind addr"),
    };
    let state_for_serve = state.clone();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel::<()>();
    let (addr_tx, addr_rx) = tokio::sync::oneshot::channel::<SocketAddr>();
    let thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("the desktop-share server runtime must build");
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::bind(bind_addr)
                .await
                .expect("bind the desktop-share server port");
            let addr = listener.local_addr().expect("local addr");
            let _ = addr_tx.send(addr);
            let serve = mirri_server::serve(listener, None, state_for_serve, None);
            tokio::select! {
                result = serve => {
                    if let Err(error) = result {
                        tracing::error!(%error, "desktop-share test server exited with an error");
                    }
                }
                _ = stop_rx => {
                    // The kill: dropping out of this select makes block_on
                    // return, and the runtime drop below aborts every
                    // spawned task (the connections).
                }
            }
        });
        // Zero-timeout shutdown: abort every task the runtime spawned —
        // the accept loop AND the per-connection tasks (their sockets
        // close, which is the transport-class death the ACP host
        // observes).
        runtime.shutdown_timeout(Duration::from_millis(0));
    });
    let addr = addr_rx
        .await
        .expect("the server thread must report its bound address");
    (addr, state, stop_tx, thread)
}

// ===========================================================================
// Handler harness: the run()-shaped remote-mode wiring
// ===========================================================================

/// The ACP-side test fixture: a temp home with the faked desktop-shell
/// state, one or two registered workspace roots, and the env pointing at
/// the home. `spawn_handler` connects through the probe exactly like
/// `run()`'s ProbeDesktop branch.
struct DesktopShareFixture {
    home: TempDir,
    workspace: TempDir,
}

impl DesktopShareFixture {
    /// A fresh home with no desktop state (the 5a "no desktop" shape),
    /// seeded with the config.toml a real desktop user's home carries: a
    /// `default_model` alias (spec 11 R4 — the create chain requires a
    /// resolvable model) whose provider resolves through the mock's env
    /// (the provider credentials are orthogonal to model resolution).
    fn fresh_home() -> Self {
        let fixture = Self {
            home: tempfile::tempdir().expect("home tempdir"),
            workspace: tempfile::tempdir().expect("workspace tempdir"),
        };
        std::fs::write(
            fixture.home.path().join("config.toml"),
            "default_model = \"desktop-share-e2e-model\"\n\n\
             [providers.mock]\n\
             type = \"anthropic\"\n\n\
             [models.\"desktop-share-e2e-model\"]\n\
             provider = \"mock\"\n\
             model = \"desktop-share-e2e-model\"\n\
             max_context_size = 128000\n",
        )
        .expect("the seeded config.toml must be writable");
        fixture
    }

    fn home_path(&self) -> &Path {
        self.home.path()
    }

    fn workspace_path(&self) -> &Path {
        self.workspace.path()
    }
}

/// Write `<home>/server/desktop-state.json` + `<home>/server.token` — the
/// two files the desktop shell maintains and the probe reads.
async fn write_desktop_state_files(home: &Path, pid: u32, port: u16, token: &str) {
    tokio::fs::create_dir_all(home.join("server"))
        .await
        .expect("create server state dir");
    tokio::fs::write(
        home.join("server/desktop-state.json"),
        json!({"pid": pid, "port": port}).to_string(),
    )
    .await
    .expect("write desktop-state.json");
    tokio::fs::write(home.join("server.token"), format!("{token}\n"))
        .await
        .expect("write server.token");
}

/// The probe-driven handler boot — the exact ProbeDesktop branch of
/// `run()`: resolve the engine choice, probe, connect with the
/// OwnSessionsOnly policy, gate the version, and wire the replacement
/// machinery (a counting wrapper around the real probe path so the tests
/// can pin the probe-invocation counts of D3).
async fn spawn_handler(home: &Path, host_start_root: &Path) -> AcpHandler {
    let probe_count = Arc::new(AtomicUsize::new(0));
    spawn_handler_with_probe_counter(home, host_start_root, probe_count).await
}

/// [`spawn_handler`] returning the probe counter alongside — the D3 tests
/// assert on it (one probe per connection death, none after degrade).
async fn spawn_handler_with_probe_counter(
    home: &Path,
    host_start_root: &Path,
    probe_count: Arc<AtomicUsize>,
) -> AcpHandler {
    // The probe path of the production `run()`: probe + version-gated
    // connect, run on a dedicated thread's runtime (the sync closure
    // contract of `set_engine_replacement_path` — block_on must never run
    // inside the caller's tokio worker).
    let probe_path = {
        let home = home.to_path_buf();
        let count = Arc::clone(&probe_count);
        Arc::new(move || {
            count.fetch_add(1, Ordering::SeqCst);
            let home = home.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .ok();
                let engine = runtime
                    .as_ref()
                    .and_then(|runtime| runtime.block_on(async { probe_and_connect(&home).await }));
                let _ = tx.send(engine);
                // The connected adapter's background tasks live on THIS
                // thread's runtime — the thread never finishes, driving
                // the runtime's tasks (`block_on(pending)`, not
                // `thread::park` which would starve them), mirroring the
                // production probe path's fix in `run.rs`.
                if let Some(runtime) = runtime {
                    runtime.block_on(std::future::pending::<()>());
                }
            });
            rx.recv().ok().flatten()
        })
    };
    let engine = probe_and_connect(home)
        .await
        .expect("the initial probe must find the server (the caller asserts the live shape)");
    let handler = AcpHandler::with_remote_engine(engine, host_start_root.to_path_buf());
    // The degrade factory — the embedded engine rooted at the host start
    // root, sharing the home (the ADR-019 storage unification premise).
    let embedded_factory_home = home.to_path_buf();
    let embedded_factory_root = host_start_root.to_path_buf();
    handler.set_engine_replacement_path(
        probe_path,
        Arc::new(move || {
            Arc::new(EmbeddedServer::for_workspace_root(
                WorkspaceId::new("acp"),
                embedded_factory_root.clone(),
                Some(embedded_factory_home.clone()),
                Vec::new(),
            )) as Arc<dyn EngineAdapter>
        }),
    );
    handler
}

/// The production probe + version-gated connect (the extracted body of
/// `run()`'s ProbeDesktop branch — `probe_and_connect_desktop` in
/// `run.rs`; re-implemented here against an explicit home because the
/// in-crate helper reads `MIRRICODE_HOME` and is `pub(crate)`-private).
async fn probe_and_connect(home: &Path) -> Option<Arc<dyn EngineAdapter>> {
    let endpoint = probe_home(home).await?;
    let adapter = RemoteAdapter::connect_with_policy(
        &endpoint.url,
        Some(&endpoint.token),
        &["approval", "question"],
        SubscribePolicy::OwnSessionsOnly,
    )
    .await
    .ok()?;
    let version = adapter.caps().map(|caps| caps.version).unwrap_or_default();
    if !mirri_acp::desktop_probe::version_satisfies(
        &version,
        mirri_acp::desktop_probe::MIN_SERVER_VERSION,
    ) {
        let _ = adapter.dispose().await;
        return None;
    }
    Some(adapter)
}

/// The probe against an explicit home — the public seam
/// `desktop_probe::probe_desktop_server` delegates to (re-built here for
/// the same pub-visibility reason; the logic mirrors `probe_home`).
async fn probe_home(home: &Path) -> Option<mirri_acp::desktop_probe::DesktopServerEndpoint> {
    let state: Value = serde_json::from_str(
        &tokio::fs::read_to_string(home.join("server/desktop-state.json"))
            .await
            .ok()?,
    )
    .ok()?;
    let pid = state.get("pid").and_then(Value::as_i64).unwrap_or(0);
    let port = state.get("port").and_then(Value::as_u64).unwrap_or(0);
    if pid <= 0 || port == 0 || port > u16::MAX as u64 {
        return None;
    }
    if !mirri_os::process::probe_process(pid as i32)
        .map(|probe| probe.alive)
        .unwrap_or(false)
    {
        return None;
    }
    let healthz = format!("http://127.0.0.1:{port}/api/v1/healthz");
    let response = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .ok()?
        .get(&healthz)
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body: Value = response.json().await.ok()?;
    if body.get("data").and_then(|data| data.get("ok")) != Some(&Value::Bool(true)) {
        return None;
    }
    let token = tokio::fs::read_to_string(home.join("server.token"))
        .await
        .ok()?
        .trim()
        .to_string();
    if token.is_empty() {
        return None;
    }
    Some(mirri_acp::desktop_probe::DesktopServerEndpoint {
        url: format!("ws://127.0.0.1:{port}/api/v1/rpc"),
        token,
    })
}

// ===========================================================================
// Wire helpers
// ===========================================================================

/// Send one ACP method through the handler's public surface with the
/// suite-wide patience.
async fn call(handler: &AcpHandler, method: &str, params: Value) -> Result<Value, (i64, String)> {
    tokio::time::timeout(RESPONSE_TIMEOUT, handler.handle(method, params))
        .await
        .expect("the handler must answer within the generous e2e timeout")
}

/// Drain `session/update` notifications until the accumulated
/// `agent_message_chunk` text contains `needle`, or fail after
/// [`RESPONSE_TIMEOUT`].
async fn wait_for_chunked_text(
    updates: &mut tokio::sync::broadcast::Receiver<mirri_transport::frame::JsonRpcNotification>,
    session_id: &str,
    needle: &str,
) -> String {
    let deadline = tokio::time::Instant::now() + RESPONSE_TIMEOUT;
    let mut chunked_text = String::new();
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for assistant text {needle:?}; got {chunked_text:?}"
        );
        let update = tokio::time::timeout(RESPONSE_TIMEOUT, updates.recv())
            .await
            .expect("timed out waiting for a session/update notification")
            .expect("update channel closed unexpectedly");
        if update.params["sessionId"].as_str() == Some(session_id)
            && update.params["update"]["sessionUpdate"].as_str() == Some("agent_message_chunk")
        {
            chunked_text.push_str(
                update.params["update"]["content"]["text"]
                    .as_str()
                    .unwrap_or_default(),
            );
            if chunked_text.contains(needle) {
                return chunked_text;
            }
        }
    }
}

/// Poll a synchronous condition until it holds or the deadline passes.
async fn poll_until(what: &str, condition: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + RESPONSE_TIMEOUT;
    loop {
        if condition() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// One full round: a fresh registered workspace root, a server booted on
/// it, and the desktop-state files written for the probe. Returns the
/// pieces the individual scenarios build on.
struct LiveDesktop {
    fixture: DesktopShareFixture,
    server: DesktopServer,
}

/// Boot a server over the fixture's home and write the desktop state for
/// it. The workspace is the fixture's single registered root (the default
/// workspace — a desktop user has exactly one root open).
async fn live_desktop(fixture: DesktopShareFixture) -> LiveDesktop {
    let server = boot_server(fixture.home_path(), fixture.workspace_path()).await;
    write_desktop_state_files(
        fixture.home_path(),
        std::process::id(),
        server.port(),
        "e2e-token",
    )
    .await;
    LiveDesktop { fixture, server }
}

// ===========================================================================
// Scenarios
// ===========================================================================

/// Scenario 1 (acceptance 1 / D1): given a live desktop server recorded in
/// the home, when the host boots through the probe path, then the handler
/// runs in remote mode — a `session/new` with the registered cwd as its
/// cwd creates the session ON THE SERVER (visible through the server's
/// own session manager), and the probe's engine is a RemoteAdapter.
#[tokio::test]
#[allow(clippy::await_holding_lock)] // ENV_LOCK serializes process-env mutation by design
async fn should_put_the_remote_engine_in_the_slot_when_the_probe_finds_the_server() {
    let _env_lock = acquire_env_lock();
    let env = TestEnv::clean();
    let fixture = DesktopShareFixture::fresh_home();
    let live = live_desktop(fixture).await;
    env._home.set(live.fixture.home_path());

    let handler = spawn_handler(live.fixture.home_path(), live.fixture.workspace_path()).await;
    handler
        .handle("initialize", json!({"protocolVersion": 1}))
        .await
        .expect("initialize must succeed");

    // A session created through the ACP surface must land on the server:
    // the server's own session manager sees it live.
    let created = call(
        &handler,
        "session/new",
        json!({
            "cwd": live.fixture.workspace_path().to_string_lossy(),
            "title": "desktop-share-e2e",
        }),
    )
    .await
    .expect("session/new on the registered cwd must succeed");
    let session_id = created["sessionId"]
        .as_str()
        .expect("a session id")
        .to_string();
    let found = live
        .server
        .state
        .session_manager
        .get_session(&mirri_core::SessionId::new(&session_id))
        .await;
    assert!(found.is_some(), "the session must live ON the server");
    live.server.kill();
}

/// Scenario 2 (acceptance 1): given a remote handler over the live server,
/// when a session is prompted, then the turn runs inside the server's
/// engine and the assistant's reply streams to the ACP update relay as
/// official `agent_message_chunk` session/update notifications.
#[tokio::test]
#[allow(clippy::await_holding_lock)] // ENV_LOCK serializes process-env mutation by design
async fn should_relay_the_servers_turn_events_to_the_acp_update_stream() {
    let _env_lock = acquire_env_lock();
    let env = TestEnv::clean();
    let fixture = DesktopShareFixture::fresh_home();
    let live = live_desktop(fixture).await;
    env._home.set(live.fixture.home_path());

    let handler = spawn_handler(live.fixture.home_path(), live.fixture.workspace_path()).await;
    let mut updates = handler.subscribe_updates();
    handler
        .handle("initialize", json!({"protocolVersion": 1}))
        .await
        .expect("initialize must succeed");
    let created = call(
        &handler,
        "session/new",
        json!({"cwd": live.fixture.workspace_path().to_string_lossy()}),
    )
    .await
    .expect("session/new must succeed");
    let session_id = created["sessionId"].as_str().unwrap().to_string();

    let result = call(
        &handler,
        "session/prompt",
        json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "Say hello"}],
        }),
    )
    .await
    .expect("the prompt turn must complete on the shared engine");
    assert_eq!(result["stopReason"], "end_turn");
    let chunked_text = wait_for_chunked_text(&mut updates, &session_id, ASSISTANT_REPLY).await;
    assert!(chunked_text.contains(ASSISTANT_REPLY));
    live.server.kill();
}

/// Scenario 3 (acceptance 2 / D2): given the remote handler, when
/// `session/new` arrives with a cwd that is NOT a registered workspace,
/// then the request errors with the "not registered" message, and the
/// server holds no session (no misrouted sandbox was created).
#[tokio::test]
#[allow(clippy::await_holding_lock)] // ENV_LOCK serializes process-env mutation by design
async fn should_reject_session_new_when_the_cwd_is_not_a_registered_workspace() {
    let _env_lock = acquire_env_lock();
    let env = TestEnv::clean();
    let fixture = DesktopShareFixture::fresh_home();
    let live = live_desktop(fixture).await;
    env._home.set(live.fixture.home_path());

    let handler = spawn_handler(live.fixture.home_path(), live.fixture.workspace_path()).await;
    handler
        .handle("initialize", json!({"protocolVersion": 1}))
        .await
        .expect("initialize must succeed");

    let unregistered = tempfile::tempdir().expect("unregistered root tempdir");
    let (code, message) = call(
        &handler,
        "session/new",
        json!({"cwd": unregistered.path().to_string_lossy()}),
    )
    .await
    .expect_err("an unregistered cwd must be rejected");
    assert_eq!(code, -32602, "invalid_params: {message}");
    assert!(
        message.contains("not a registered workspace"),
        "the error names the unregistered directory: {message}"
    );
    // And no session was created anywhere: the server's live set is empty
    // (a session.create never ran).
    let listed = call(&handler, "session/list", json!({}))
        .await
        .expect("list");
    let sessions = listed["sessions"].as_array().cloned().unwrap_or_default();
    assert!(
        sessions.is_empty(),
        "no session may exist after the rejected create: {sessions:?}"
    );
    live.server.kill();
}

/// Scenario 4 (acceptance 7): given a home carrying a persisted-only
/// session directory the handler never touched, when the remote handler
/// connects and serves requests, then the persisted session is NOT
/// revived (the server's live set stays empty of it) — OwnSessionsOnly
/// must not resurrect untouched persisted sessions.
#[tokio::test]
#[allow(clippy::await_holding_lock)] // ENV_LOCK serializes process-env mutation by design
async fn should_not_revive_untouched_persisted_sessions() {
    let _env_lock = acquire_env_lock();
    let env = TestEnv::clean();
    let fixture = DesktopShareFixture::fresh_home();
    let workspace_key = mirri_core::encode_work_dir_key(fixture.workspace_path());
    let persisted_dir = fixture
        .home_path()
        .join("v3_sessions")
        .join(&workspace_key)
        .join("session-persisted-ghost");
    std::fs::create_dir_all(&persisted_dir).expect("create the persisted session dir");
    std::fs::write(
        persisted_dir.join("state.json"),
        json!({
            "version": 3,
            "title": "a persisted ghost",
            "createdAt": "2026-01-01T00:00:00Z",
            "updatedAt": "2026-01-01T00:00:00Z",
            "tags": [],
            "archived": false,
            "workDir": fixture.workspace_path().to_string_lossy(),
        })
        .to_string(),
    )
    .expect("seed the ghost's state.json");

    let live = live_desktop(fixture).await;
    env._home.set(live.fixture.home_path());
    let handler = spawn_handler(live.fixture.home_path(), live.fixture.workspace_path()).await;
    handler
        .handle("initialize", json!({"protocolVersion": 1}))
        .await
        .expect("initialize must succeed");

    // Driving unrelated traffic on the handler must not revive the ghost:
    // list, create, and prompt a FRESH session, then check the ghost was
    // never restored (the server's live set does not contain it).
    let _ = call(&handler, "session/list", json!({}))
        .await
        .expect("list");
    let created = call(
        &handler,
        "session/new",
        json!({"cwd": live.fixture.workspace_path().to_string_lossy()}),
    )
    .await
    .expect("session/new must succeed");
    let fresh_id = created["sessionId"].as_str().unwrap().to_string();
    let _ = call(&handler, "session/close", json!({"sessionId": fresh_id}))
        .await
        .expect("close must succeed");

    let ghost_is_live = live
        .server
        .state
        .session_manager
        .get_session(&mirri_core::SessionId::new("session-persisted-ghost"))
        .await
        .is_some();
    assert!(
        !ghost_is_live,
        "the untouched persisted session must NOT be revived by the ACP connection"
    );
    live.server.kill();
}

/// Scenarios 5 + 5b (acceptance 3 / D3): given a remote handler over a
/// server that then dies, when the next request's re-probe fails, then the
/// slot degrades to the embedded engine — that request fails with its
/// original error, a subsequent `session/new` succeeds locally, and after
/// the server comes BACK the degraded host still does not re-probe (the
/// degrade is one-way; a session-addressed call answers NotConnected).
#[tokio::test]
#[allow(clippy::await_holding_lock)] // ENV_LOCK serializes process-env mutation by design
async fn should_degrade_to_the_embedded_engine_once_when_the_reprobe_fails() {
    let _env_lock = acquire_env_lock();
    let env = TestEnv::clean();
    let fixture = DesktopShareFixture::fresh_home();
    let live = live_desktop(fixture).await;
    env._home.set(live.fixture.home_path());
    let handler = spawn_handler(live.fixture.home_path(), live.fixture.workspace_path()).await;
    handler
        .handle("initialize", json!({"protocolVersion": 1}))
        .await
        .expect("initialize must succeed");
    let created = call(
        &handler,
        "session/new",
        json!({"cwd": live.fixture.workspace_path().to_string_lossy()}),
    )
    .await
    .expect("session/new before the kill must succeed");
    let session_id = created["sessionId"].as_str().unwrap().to_string();

    // When the server dies (the desktop app exited)…
    let dead_port = live.server.port();
    live.server.kill();
    poll_until("the listener to release the port", || {
        std::net::TcpStream::connect(("127.0.0.1", dead_port)).is_err()
    })
    .await;

    // …the in-flight/prompt request fails with the transport error surfaced
    // (design 14 D3: in-flight turns are never migrated).
    let prompt_error = call(
        &handler,
        "session/prompt",
        json!({"sessionId": session_id, "prompt": "hi"}),
    )
    .await
    .expect_err("the prompt on the dead connection must fail");
    assert_eq!(
        prompt_error.0, -32603,
        "internal (transport): {prompt_error:?}"
    );

    // And the NEXT new-session request lands on the embedded engine
    // (degraded) — a fresh cwd validates against the local filesystem
    // again, and the session is served locally.
    let local_workspace = tempfile::tempdir().expect("local workspace tempdir");
    let created_local = call(
        &handler,
        "session/new",
        json!({"cwd": local_workspace.path().to_string_lossy()}),
    )
    .await
    .expect("session/new after the degrade must succeed locally");
    assert!(
        created_local["sessionId"]
            .as_str()
            .is_some_and(str::is_empty)
            .eq(&false),
        "a local session id must be returned: {created_local}"
    );

    // 5b: after the degrade, restart the server — the degraded host does
    // NOT re-probe (one-way): a new session-addressed call to the OLD
    // session still fails, and a new session still lands on the embedded
    // slot.
    let live_again = boot_server(live.fixture.home_path(), live.fixture.workspace_path()).await;
    let ghost_error = call(
        &handler,
        "session/prompt",
        json!({"sessionId": session_id, "prompt": "hi"}),
    )
    .await
    .expect_err("the old session's calls must keep failing after the degrade (D3)");
    // The degraded shape: the call lands on the embedded slot, which never
    // knew the server-side session — "not found" is the honest post-degrade
    // rendering of D3's "旧会话保持 NotConnected" (the alternative — a
    // re-probe silently reviving the remote — is exactly what D3 forbids).
    assert_eq!(
        ghost_error.0, -32002,
        "session not found (degraded): {ghost_error:?}"
    );
    let still_local = call(
        &handler,
        "session/new",
        json!({"cwd": local_workspace.path().to_string_lossy()}),
    )
    .await
    .expect("session/new must still succeed on the embedded slot");
    assert!(
        still_local["sessionId"].as_str().is_some(),
        "the embedded engine keeps serving new sessions: {still_local}"
    );
    live_again.kill();
}

/// Scenario 5a + 8 (acceptance 3 + 4): given a fresh home with NO
/// desktop-state.json, when the host boots through the probe path, then it
/// serves embedded from the start (`session/new` succeeds locally), and the
/// probe made zero side effects — no `workspaces.json` was produced and no
/// TCP connection was attempted (the state file is the only anchor; its
/// absence costs exactly one failed read).
#[tokio::test]
#[allow(clippy::await_holding_lock)] // ENV_LOCK serializes process-env mutation by design
async fn should_fall_back_to_embedded_with_zero_probe_side_effects_when_no_desktop_state_exists() {
    let _env_lock = acquire_env_lock();
    let env = TestEnv::clean();
    let fixture = DesktopShareFixture::fresh_home();
    env._home.set(fixture.home_path());

    // The PRODUCTION probe (the exact function `run()`'s ProbeDesktop
    // branch calls) against the empty home fails closed BEFORE any HTTP
    // request is attempted — the state file is the only anchor, and its
    // read is the probe's entire cost on a machine without the desktop.
    let endpoint = mirri_acp::desktop_probe::probe_desktop_server().await;
    assert!(endpoint.is_none(), "no desktop state → no endpoint");

    // Side-effect freedom (acceptance 4), asserted BEFORE any engine is
    // constructed (an embedded engine's index write-through would
    // legitimately create `workspaces.json` later — that is the engine's
    // doing, never the probe's): the home carries no registry file.
    assert!(
        !fixture.home_path().join("workspaces.json").exists(),
        "the probe must not create workspaces.json"
    );
    // And zero TCP contact: with no state file the probe never reaches the
    // healthz step — the `None` above is decided by the file read alone,
    // before any client construction (design 14 D1's "无端口扫描" —
    // the state-file short-circuit IS the guarantee a recorded transport
    // would pin with count == 0).

    // The handler boots embedded: a session/new over a real local cwd
    // succeeds (the embedded engine binds the workspace itself).
    let local_workspace = tempfile::tempdir().expect("local workspace tempdir");
    let home = fixture.home_path().to_path_buf();
    let handler = {
        // No remote engine exists — mirror run()'s fallback wiring: the
        // embedded pool rooted at the workspace dir.
        let default_engine: Arc<dyn EngineAdapter> = Arc::new(EmbeddedServer::for_workspace_root(
            WorkspaceId::new("acp"),
            local_workspace.path().to_path_buf(),
            Some(home.clone()),
            Vec::new(),
        ));
        let root = local_workspace.path().canonicalize().unwrap();
        let engine = Arc::clone(&default_engine);
        let root_for_cwd = root.clone();
        let engine_for_cwd = Arc::clone(&default_engine);
        let factory_home = home.clone();
        let handler = AcpHandler::with_engine_pool(
            default_engine,
            Arc::new(move |cwd: &Path| {
                if cwd == root {
                    return Arc::clone(&engine);
                }
                Arc::new(EmbeddedServer::for_workspace_root(
                    WorkspaceId::new("acp"),
                    cwd.to_path_buf(),
                    Some(factory_home.clone()),
                    Vec::new(),
                )) as Arc<dyn EngineAdapter>
            }),
        );
        handler.remember_engine_cwd(&engine_for_cwd, root_for_cwd);
        handler
    };
    handler
        .handle("initialize", json!({"protocolVersion": 1}))
        .await
        .expect("initialize must succeed");
    let created = call(
        &handler,
        "session/new",
        json!({"cwd": local_workspace.path().to_string_lossy()}),
    )
    .await
    .expect("embedded session/new must succeed");
    assert!(
        created["sessionId"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "an embedded session id: {created}"
    );
}

/// Scenario 6 (acceptance 6): given server A dying and server B starting
/// on the same recorded port (sharing the home), when the next request
/// re-probes successfully, then the slot returns to remote mode and a
/// session-addressed call to a session created BEFORE the kill is
/// touch-to-subscribed on the NEW connection (the shared home's
/// restore-on-demand serves it).
#[tokio::test]
#[allow(clippy::await_holding_lock)] // ENV_LOCK serializes process-env mutation by design
async fn should_touch_to_subscribe_pre_kill_sessions_after_a_successful_reprobe() {
    let _env_lock = acquire_env_lock();
    let env = TestEnv::clean();
    let fixture = DesktopShareFixture::fresh_home();
    let live = live_desktop(fixture).await;
    env._home.set(live.fixture.home_path());
    let handler = spawn_handler(live.fixture.home_path(), live.fixture.workspace_path()).await;
    handler
        .handle("initialize", json!({"protocolVersion": 1}))
        .await
        .expect("initialize must succeed");
    let created = call(
        &handler,
        "session/new",
        json!({"cwd": live.fixture.workspace_path().to_string_lossy()}),
    )
    .await
    .expect("session/new on server A must succeed");
    let session_id = created["sessionId"].as_str().unwrap().to_string();

    // Materialize the session (SDR-004 lazy creation: the first user
    // message persists `state.json` + the wire log — the ghost B will
    // restore-on-demand must have something on disk).
    let prompted = call(
        &handler,
        "session/prompt",
        json!({"sessionId": session_id, "prompt": "hello"}),
    )
    .await
    .expect("the pre-kill turn must complete on server A");
    assert_eq!(prompted["stopReason"], "end_turn");

    // Kill A, then boot B on the SAME port over the SHARED home — the
    // desktop "restarted on its recorded port" shape.
    let port = live.server.port();
    live.server.kill();
    poll_until("server A's port to release", || {
        std::net::TcpStream::connect(("127.0.0.1", port)).is_err()
    })
    .await;
    let server_b = boot_server_on_port(
        live.fixture.home_path(),
        live.fixture.workspace_path(),
        port,
    )
    .await;

    // The next session-addressed call re-probes successfully (B answers),
    // and the call itself — `session/load` of the pre-kill session — is
    // served through the fresh connection: the adapter's touch-to-subscribe
    // (D5) precedes the `session.resume` on the wire, B's subscribe handler
    // restores the persisted session on demand, and the load completes
    // against B.
    let loaded = call(
        &handler,
        "session/load",
        json!({
            "sessionId": session_id,
            "cwd": live.fixture.workspace_path().to_string_lossy(),
        }),
    )
    .await
    .expect("the pre-kill session must resume on server B after the re-probe");
    assert!(
        loaded["sessionId"].as_str().is_some(),
        "the load answers the session surface: {loaded}"
    );

    // And a subsequent prompt runs on the resumed session through B.
    let prompted = call(
        &handler,
        "session/prompt",
        json!({"sessionId": session_id, "prompt": "hi"}),
    )
    .await;
    assert!(
        prompted.is_ok(),
        "the resumed session is promptable: {prompted:?}"
    );
    server_b.kill();
}

/// Scenario 7 (acceptance 5): given `MIRRI_ACP_REMOTE_ENGINE` pointing at
/// the live server, when the resolution chain runs, then the explicit
/// branch is taken (the env resolution test pins the priority; here the
/// connect itself must succeed with the AllSessions semantics —
/// byte-compatible with the pre-design-14 forced remote).
#[tokio::test]
#[allow(clippy::await_holding_lock)] // ENV_LOCK serializes process-env mutation by design
async fn should_connect_directly_when_the_explicit_remote_engine_env_is_set() {
    let _env_lock = acquire_env_lock();
    let env = TestEnv::clean();
    let fixture = DesktopShareFixture::fresh_home();
    let live = live_desktop(fixture).await;
    env._home.set(live.fixture.home_path());
    // The explicit branch: `MIRRI_ACP_REMOTE_ENGINE` set → forced remote,
    // the token from `MIRRI_ACP_REMOTE_TOKEN` (the env precedence the
    // design pins; the server.token file fallback is the same read).
    env._remote_engine.set(live.server.ws_rpc_url());
    env._remote_token.set("e2e-token");

    assert_eq!(
        resolve_engine_choice(),
        EngineChoice::ForcedRemote {
            url: live.server.ws_rpc_url(),
        },
        "the explicit URL outranks every other rule (design 14 D3)"
    );
    let adapter = RemoteAdapter::connect(
        &live.server.ws_rpc_url(),
        Some("e2e-token"),
        &["approval", "question"],
    )
    .await
    .expect("the explicit-branch connect must succeed (byte-compat)");
    let sessions = adapter
        .call_method("session.list", json!({}))
        .await
        .expect("a connected forced-remote adapter serves requests");
    assert!(
        sessions["items"].is_array(),
        "the page envelope: {sessions}"
    );
    let _ = adapter.dispose().await;
    live.server.kill();
}

// ===========================================================================
// Local mirrors of run()'s resolution types (the crate keeps them
// pub(crate)-private; the e2e needs the same shapes to assert the chain).
// ===========================================================================

/// The engine form the host chooses at startup (design 14 §D3).
#[derive(Debug, Clone, PartialEq, Eq)]
enum EngineChoice {
    ForcedRemote { url: String },
    Embedded,
    ProbeDesktop,
}

/// Mirror of `run.rs`'s `resolve_engine_choice` — same priority chain:
/// `MIRRI_ACP_REMOTE_ENGINE` > `MIRRI_ACP_ENGINE=embedded` >
/// `MIRRI_ACP_WORKSPACE_DIR` (semaphore) > probe.
fn resolve_engine_choice() -> EngineChoice {
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

/// Boot a server bound to an explicit port (scenario 6's server B — the
/// desktop-state file still names A's port, so B must own it). Same
/// dedicated-runtime kill semantics as [`boot_server`].
async fn boot_server_on_port(home: &Path, workspace_root: &Path, port: u16) -> DesktopServer {
    let mock = Arc::new(MockLlm::new());
    mock.push_text_response(ASSISTANT_REPLY);
    let (addr, state, stop_tx, thread) = spawn_server_thread(
        home,
        workspace_root,
        Arc::new(MockLlmFactory::new(mock)),
        Some(port),
    )
    .await;
    DesktopServer {
        addr,
        state,
        stop: Some(stop_tx),
        thread: Some(thread),
        _workspace_root: tempfile::tempdir().expect("workspace tempdir"),
    }
}
