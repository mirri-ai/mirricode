//! Shared test harness: boots a full in-process server on an ephemeral port.
//!
//! Since the M5-B3a thinning the server is transport + auth + projection
//! only: the test server builds an in-process [`EmbeddedServer`] (the same
//! engine `mirri serve` uses) and hands it to
//! [`ServerState::with_engine`]. Every engine-domain method the suites drive
//! (`session.create`, `message.send`, …) runs on the embedded engine behind
//! the shared dispatch, so the wire tests also prove the engine seam.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use mirri_core::{
    AppScope, AppScopeConfig, LlmFactory, ServerConfig, SessionManager, WorkspaceId,
    WorkspaceScopeConfig, WorkspaceTrustLevel,
};
use mirri_embedded::EmbeddedServer;
use mirri_server::ServerState;
use mirri_transport::adapter::EngineAdapter;
use tempfile::TempDir;

/// The engine-side default model seeded into every test boot's config
/// snapshot: the embedded create chain requires a resolvable model (spec 11
/// R4 — no built-in fallback), so every wire test that creates a session
/// without an explicit `modelId` needs SOME default configured, exactly
/// like a real user's config.toml would. The seed deliberately carries no
/// `models` alias entry (the context window stays UNKNOWN), matching the
/// cross-backend contract suite's choice in 4cf03e13 — a known window would
/// flip compaction-aware assertions that rely on budget-1 semantics. The
/// transport-level `ServerState` config stays separate and unseeded, so
/// the auth-summary readiness tests still observe their own no-default
/// default.
const TEST_DEFAULT_MODEL: &str = "test-default-model";

/// The seeded engine config snapshot shared by the test boots.
fn test_engine_config() -> Arc<RwLock<ServerConfig>> {
    Arc::new(RwLock::new(ServerConfig {
        default_model: Some(TEST_DEFAULT_MODEL.to_string()),
        ..ServerConfig::default()
    }))
}

/// A running test server bound to an ephemeral localhost port.
///
/// Each integration-test binary compiles this module on its own, so members
/// that one binary does not use are allowed to be dead code there.
#[allow(dead_code)]
pub struct TestServer {
    /// Address the HTTP (and WebSocket) server is listening on.
    pub addr: SocketAddr,
    /// The server state, kept so tests can inspect or shut it down.
    pub state: ServerState,
    /// The engine behind the server — kept so tests can address it directly
    /// (e.g. read the mock through the factory they constructed).
    pub engine: Arc<dyn EngineAdapter>,
    /// Unix-socket path the IPC transport listens on, when IPC is enabled.
    pub ipc_path: Option<PathBuf>,
    /// Temp persistence root; dropped when the server is dropped.
    _tmp: TempDir,
}

#[allow(dead_code)]
impl TestServer {
    /// Boot a server against a temp-dir persistence root, without an LLM
    /// provider factory (the `message.send` engine loop stays disabled) and
    /// without authentication (open mode).
    pub async fn start() -> Self {
        Self::start_with_parts(None, None, None, false, false, None, None).await
    }

    /// Boot a server against a temp-dir persistence root, with an optional
    /// LLM provider factory. Tests that exercise the engine loop pass a mock
    /// factory here; tests that verify the no-provider error path use
    /// [`start`](Self::start).
    pub async fn start_with_llm_factory(llm_factory: Option<Arc<dyn LlmFactory>>) -> Self {
        Self::start_with_parts(None, llm_factory, None, false, false, None, None).await
    }

    /// Boot a server that requires `Authorization: Bearer <auth_token>` on
    /// every HTTP route except the public `/api/v1/healthz` and `/api/v1/meta`
    /// probes, without an LLM provider factory.
    pub async fn start_with_auth(auth_token: impl Into<String>) -> Self {
        Self::start_with_parts(Some(auth_token.into()), None, None, false, false, None, None).await
    }

    /// Boot an open-mode server whose `/api/v1/rpc` endpoint heartbeats at
    /// the given interval (instead of the 15s default), so heartbeat tests
    /// run in milliseconds.
    pub async fn start_with_heartbeat_interval(interval: Duration) -> Self {
        Self::start_with_parts(None, None, Some(interval), false, false, None, None).await
    }

    /// Boot an open-mode server that also serves line-delimited JSON-RPC 2.0
    /// over a Unix domain socket, for the IPC transport tests.
    pub async fn start_with_ipc() -> Self {
        Self::start_with_parts(None, None, None, true, false, None, None).await
    }

    /// Boot an open-mode server whose files store (and engine files root)
    /// live in a temp dir, for the files REST tests.
    pub async fn start_with_files_dir() -> Self {
        Self::start_with_parts(None, None, None, false, true, None, None).await
    }

    /// Boot an open-mode server whose engine roots its Mirri home in the
    /// given directory — the agent-profile CRUD writes (`<home>/agents/`)
    /// and the models.dev cache land there instead of the ambient home, and
    /// a second server over the same home observes the persisted state (the
    /// restart-persistence tests).
    pub async fn start_with_home_dir(home_dir: PathBuf) -> Self {
        Self::start_with_parts(None, None, None, false, false, Some(home_dir), None).await
    }

    /// Boot an open-mode server whose engine is the caller's: HTTP/WS on an
    /// ephemeral port against a throwaway workspace. Used by the
    /// serve-surface suites that drive a specific engine behavior in
    /// isolation (e.g. the app-level MCP pool broadcast behind
    /// `mcp.subscribe`) without depending on the embedded engine's pool
    /// internals to produce the events.
    pub async fn start_with_engine(engine: Arc<dyn EngineAdapter>) -> Self {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let app = AppScope::new(AppScopeConfig::default());
        let workspace = app
            .create_workspace_with_config(
                WorkspaceId::new("test-ws"),
                tmp.path().to_path_buf(),
                WorkspaceScopeConfig {
                    trust_level: WorkspaceTrustLevel::Trusted,
                    ..Default::default()
                },
            )
            .await
            .expect("create workspace");
        let manager = Arc::new(SessionManager::with_persistence_dir(
            workspace,
            tmp.path().join(".mirri").join("sessions"),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        ));
        let state =
            ServerState::with_engine(Arc::clone(&engine), manager, "0.1.0-test".to_string(), None);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let server_state = state.clone();
        tokio::spawn(async move {
            if let Err(error) = mirri_server::http::serve(listener, server_state, None).await {
                tracing::error!(%error, "test server exited with an error");
            }
        });
        Self {
            addr,
            state,
            engine,
            ipc_path: None,
            _tmp: tmp,
        }
    }

    /// Boot an open-mode server whose transport-level
    /// [`ServerState`](mirri_server::ServerState) carries the given shared
    /// config snapshot (wired into the `session.status` model-window read),
    /// for the status tests that assert config-sourced context ceilings.
    pub async fn start_with_state_config(state_config: Arc<RwLock<ServerConfig>>) -> Self {
        Self::start_with_parts(None, None, None, false, false, None, Some(state_config)).await
    }

    /// Shared bootstrap for every [`TestServer`] variant: builds the engine
    /// scopes and session manager, then serves HTTP/WS on an ephemeral
    /// loopback port with the requested auth, LLM factory, and heartbeat
    /// interval configuration.
    ///
    /// The engine is an [`EmbeddedServer`] built exactly like the historical
    /// server-side wiring: no profile manager (no Agent/AgentSwarm tools), no
    /// web-search / fetch-url providers, and the factory (or `None`) the
    /// caller requested.
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::fn_params_excessive_bools)]
    async fn start_with_parts(
        auth_token: Option<String>,
        llm_factory: Option<Arc<dyn LlmFactory>>,
        heartbeat_interval: Option<Duration>,
        with_ipc: bool,
        with_files_dir: bool,
        home_dir: Option<PathBuf>,
        state_config: Option<Arc<RwLock<ServerConfig>>>,
    ) -> Self {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let app = AppScope::new(AppScopeConfig::default());
        let workspace = app
            .create_workspace_with_config(
                WorkspaceId::new("test-ws"),
                tmp.path().to_path_buf(),
                WorkspaceScopeConfig {
                    trust_level: WorkspaceTrustLevel::Trusted,
                    ..Default::default()
                },
            )
            .await
            .expect("create workspace");
        let manager = Arc::new(SessionManager::with_persistence_dir(
            workspace,
            tmp.path().join(".mirri").join("sessions"),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        ));
        // The files store root: shared by the REST upload/download routes
        // (through the state) and the engine's `message.send` entry
        // resolution, so an uploaded file is immediately attachable.
        let files_dir = with_files_dir.then(|| tmp.path().join("files"));
        let engine: Arc<dyn EngineAdapter> = {
            let mut embedded = EmbeddedServer::with_providers(
                Arc::clone(&manager),
                llm_factory,
                // A home-rooted engine loads its profile manager from the
                // user roots chain (`<home>/agents`, then the legacy
                // `<home>/profiles`) exactly like the `mirri serve` boot;
                // the ambient-home default keeps no profile manager.
                home_dir.as_ref().map(|home| {
                    let profiles = Arc::new(mirri_core::ProfileManager::new());
                    let _ = profiles.load_from_roots(
                        &[home.join("agents"), home.join("profiles")],
                        &mirri_os::LocalSyncFs,
                    );
                    profiles
                }),
                None,
                None,
                test_engine_config(),
                None,
            )
            // The models.dev upstream stays off the REST test wire (no
            // network in tests; refresh is exercised with wiremock in the
            // embedded crate).
            .with_models_dev_url(None);
            if let Some(home) = &home_dir {
                embedded = embedded.with_home_dir(Some(home.clone()));
            }
            if let Some(dir) = &files_dir {
                embedded = embedded.with_files_dir(dir.clone());
            }
            Arc::new(embedded)
        };
        let state = ServerState::with_engine(
            Arc::clone(&engine),
            manager,
            "0.1.0-test".to_string(),
            auth_token,
        );
        let state = match state_config {
            Some(config) => state.with_config(config, None),
            None => state,
        };
        let state = match heartbeat_interval {
            Some(interval) => state.with_heartbeat_interval(interval),
            None => state,
        };
        let state = match files_dir {
            Some(dir) => state.with_files_dir(dir),
            None => state,
        };
        let ipc_path = with_ipc.then(|| tmp.path().join("test.ipc.sock"));
        if let Some(path) = ipc_path.clone() {
            let ipc_state = state.clone();
            tokio::spawn(async move {
                if let Err(error) = mirri_server::serve_ipc(path, ipc_state).await {
                    tracing::error!(%error, "test IPC server exited with an error");
                }
            });
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let server_state = state.clone();
        tokio::spawn(async move {
            if let Err(error) = mirri_server::http::serve(listener, server_state, None).await {
                tracing::error!(%error, "test server exited with an error");
            }
        });
        Self {
            addr,
            state,
            engine,
            ipc_path,
            _tmp: tmp,
        }
    }

    /// Base URL of the HTTP API, e.g. `http://127.0.0.1:PORT`.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Base URL of the multiplexed WebSocket endpoint, e.g.
    /// `ws://127.0.0.1:PORT/api/v1/ws`.
    pub fn ws_url(&self) -> String {
        format!("ws://{}/api/v1/ws", self.addr)
    }

    /// Base URL of the unified JSON-RPC WebSocket endpoint, e.g.
    /// `ws://127.0.0.1:PORT/api/v1/rpc`.
    pub fn ws_rpc_url(&self) -> String {
        format!("ws://{}/api/v1/rpc", self.addr)
    }
}
