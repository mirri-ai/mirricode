//! Shared support for the harness self-tests: boots a full real `mirri-server`
//! (same pattern as `mirri-server`'s own test suite) on an ephemeral port,
//! and provides an echo tool to register on test agents.

use std::net::SocketAddr;
use std::sync::Arc;

use mirri_core::{
    AgentId, AgentScopeConfig, EngineResult, ExecutionPlan, LlmFactory, PermissionMode, SessionId,
    SessionManager, Tool, ToolExecution, ToolInputDisplay, ToolResult, WorkspaceId, WorkspaceScope,
    WorkspaceScopeConfig,
};
use mirri_embedded::EmbeddedServer;
use mirri_server::ServerState;
use mirri_transport::adapter::EngineAdapter;
use std::sync::RwLock;
use tempfile::TempDir;

/// The engine-side default model seeded into the test boot's config
/// snapshot: the embedded create chain requires a resolvable model (spec 11
/// R4 — no built-in fallback), so every harness self-test that creates a
/// session without an explicit `modelId` needs SOME default configured,
/// exactly like a real user's config.toml would (the same suite-wide seed
/// the mirri-server wire tests and the cross-backend contract suite in
/// 4cf03e13 carry). The seed carries no `models` alias entry — the context
/// window stays UNKNOWN, so compaction-aware histories keep their budget-1
/// semantics.
const TEST_DEFAULT_MODEL: &str = "test-default-model";

/// The seeded engine config snapshot for the harness test boots.
fn test_engine_config() -> Arc<RwLock<mirri_core::ServerConfig>> {
    Arc::new(RwLock::new(mirri_core::ServerConfig {
        default_model: Some(TEST_DEFAULT_MODEL.to_string()),
        ..mirri_core::ServerConfig::default()
    }))
}

/// A running test server bound to an ephemeral localhost port.
///
/// Dropping it closes the temp persistence root; the listener task is aborted
/// when the test runtime shuts down.
pub struct TestServer {
    /// Address the HTTP (and WebSocket) server is listening on.
    pub addr: SocketAddr,
    /// The server state, so tests can reach into the session manager to
    /// create agents, register tools, and publish events (there is no RPC
    /// endpoint for agent creation yet).
    pub state: ServerState,
    /// Temp persistence root; dropped when the server is dropped.
    _tmp: TempDir,
}

impl TestServer {
    /// Boot a server without an LLM provider factory (the `message.send`
    /// engine loop stays disabled) and without authentication.
    pub async fn start() -> Self {
        Self::start_with_parts(None, None).await
    }

    /// Boot a server with an optional LLM provider factory. Tests that
    /// exercise the engine loop pass a `mirri_test_harness::MockLlmFactory`
    /// here.
    pub async fn start_with_llm_factory(llm_factory: Option<Arc<dyn LlmFactory>>) -> Self {
        Self::start_with_parts(None, llm_factory).await
    }

    /// Boot a server that requires `Authorization: Bearer <auth_token>` on
    /// every route except the public `healthz`/`meta` probes.
    pub async fn start_with_auth(auth_token: impl Into<String>) -> Self {
        Self::start_with_parts(Some(auth_token.into()), None).await
    }

    /// Shared bootstrap: build the engine scopes and session manager, then
    /// serve HTTP/WS on an ephemeral loopback port. Since the M5-B3a
    /// thinning the test server is transport + auth + projection with an
    /// in-process [`EmbeddedServer`] behind the shared seam.
    async fn start_with_parts(
        auth_token: Option<String>,
        llm_factory: Option<Arc<dyn LlmFactory>>,
    ) -> Self {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let workspace = Arc::new(WorkspaceScope::new(
            WorkspaceId::new("test-ws"),
            tmp.path().to_path_buf(),
            WorkspaceScopeConfig::default(),
        ));
        let manager = Arc::new(SessionManager::with_persistence_dir(
            workspace,
            tmp.path().join(".mirri").join("sessions"),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        ));
        let engine: Arc<dyn EngineAdapter> = Arc::new(EmbeddedServer::with_providers(
            Arc::clone(&manager),
            llm_factory,
            None,
            None,
            None,
            test_engine_config(),
            None,
        ));
        let state = ServerState::with_engine(engine, manager, "0.1.0-test".to_string(), auth_token);
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
            _tmp: tmp,
        }
    }

    /// Base URL of the HTTP API, e.g. `http://127.0.0.1:PORT`.
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Create a fresh agent in the given session (through the session
    /// manager, since the API has no agent-creation endpoint).
    ///
    /// The agent runs in Yolo permission mode so transport-level tests do not
    /// trip the permission policy chain during engine turns; the gate itself
    /// is covered by `mirri-core`'s unit tests.
    pub async fn create_agent(&self, session_id: &str, agent_name: &str) {
        let session = self
            .state
            .session_manager
            .get_session(&SessionId::new(session_id))
            .await
            .expect("session is live");
        session
            .create_agent(
                AgentId::new(agent_name),
                AgentScopeConfig {
                    permission_mode: PermissionMode::Yolo,
                    ..AgentScopeConfig::default()
                },
            )
            .await
            .expect("create agent");
    }

    /// Register the shared [`EchoTool`] as the only active tool of the given
    /// session agent.
    pub async fn register_echo_tool(&self, session_id: &str, agent_name: &str) {
        let session = self
            .state
            .session_manager
            .get_session(&SessionId::new(session_id))
            .await
            .expect("session is live");
        let agent = session
            .get_agent(&AgentId::new(agent_name))
            .await
            .expect("agent is live");
        let tool_manager = agent.tool_manager();
        let mut tools = tool_manager.write();
        tools.register_builtin(Arc::new(EchoTool));
        tools.set_active_tools(vec!["Echo".to_string()]);
    }
}

/// A simple tool that echoes back the `message` argument, JSON-schema
/// validated, so tests can exercise `tool.list` end to end.
pub struct EchoTool;

impl Tool for EchoTool {
    fn name(&self) -> &str {
        "Echo"
    }

    fn description(&self) -> &str {
        "Echoes back the input"
    }

    fn parameters(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "message": { "type": "string" }
                },
                "required": ["message"]
            })
        })
    }

    fn resolve_execution(&self, input: serde_json::Value) -> EngineResult<ToolExecution> {
        let msg = input
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("no message")
            .to_string();
        Ok(ToolExecution::Runnable {
            plan: ExecutionPlan {
                accesses: Vec::new(),
                display: ToolInputDisplay::Generic {
                    summary: format!("Echo: {msg}"),
                },
                approval_rule: "Echo".to_string(),
                stop_batch_after_this: false,
            },
            execute: Box::new(move |_ctx| Box::pin(async move { ToolResult::success(msg) })),
        })
    }
}
