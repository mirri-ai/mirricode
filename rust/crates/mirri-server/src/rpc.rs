//! RPC method table shared by every transport (HTTP, WebSocket, IPC).
//!
//! A method is an [`RpcHandler`]: a static method name plus an async
//! `handle(params) -> RpcResult<Value>`. The [`RpcRegistry`] maps method names
//! to handlers, so the HTTP server, the WebSocket dispatcher, and the IPC
//! server all speak one language — each transport only adds its own envelope:
//!
//! - HTTP wraps results in [`ApiResponse`] / [`ApiErrorResponse`]
//!   (`{code, msg, data, request_id}`), matching the TypeScript server.
//! - IPC uses plain JSON-RPC 2.0 (`{jsonrpc, result|error, id}`).
//!
//! Error codes ([`RpcError::json_rpc_code`]) follow JSON-RPC 2.0 conventions
//! (`-326xx` for protocol errors, `-32xxx` for application errors).
//!
//! ## Transport registry (M5-B3a)
//!
//! After the M5-B3a thinning, the registry contains exactly the 24
//! transport-owned methods: the server-only fact rows of
//! [`mirri_proto::rpc`](mirri_proto::rpc) (`agent.get`/`agent.list`,
//! `approval.decide`, `config.watch`, `file.*` ×6, `message.list`,
//! `question.answer`/`question.dismiss`,
//! `session.children`/`session.snapshot`/
//! `session.status`, `tool.catalog`) plus the explicit
//! overrides `btw.start`, `session.get`, and `session.restore` — whose
//! real server-side handlers stay here (same wire shapes as the embedded
//! routes) after the B1 flips to `both`. Every other method the serve wire
//! exposes is a `mirri_proto::rpc` availability fact (`serves_server`),
//! dispatched to the [`EngineAdapter`](crate::ServerState::engine) through
//! [`ServerState::dispatch`] (see `state.rs`): methods with a local handler
//! run locally, other serve-surface methods fall through to the engine, and
//! everything else answers `-32601` method-not-found.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use base64::Engine as _;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use mirri_core::{
    AgentId, AgentLifecycle, AgentScope, ContentPart, Role, ServerConfig, SessionArtifacts,
    SessionId, SessionInfo, SessionManager, SessionScope, max_context_tokens_for,
    validate_workspace_path,
};
#[cfg(test)]
use mirri_proto::WireOp;
use mirri_proto::rpc::json_rpc_code_for_domain_code;

use crate::ws_rpc::SessionEventChannelRegistry;

/// The RPC trait — each method is one RPC endpoint.
/// HTTP routes, WS dispatch, and IPC all share this definition.
#[async_trait]
pub trait RpcHandler: Send + Sync {
    /// Method name for routing (e.g., "session.create", "message.send").
    fn method(&self) -> &'static str;
    /// Handle the request.
    async fn handle(&self, params: serde_json::Value) -> RpcResult<serde_json::Value>;
}

/// Result type returned by [`RpcHandler::handle`].
pub type RpcResult<T> = Result<T, RpcError>;

/// Errors produced by RPC handlers, mapped to JSON-RPC 2.0-style codes.
#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("method not found: {0}")]
    MethodNotFound(String),
    #[error("invalid params: {0}")]
    InvalidParams(String),
    /// Internal error with a server-generated correlation id.
    ///
    /// The id is minted by [`RpcError::internal`], embedded in the message
    /// (`internal error [id: …]: …`) and echoed into the response `data`
    /// (`{"error_id": …}`) so clients and support can correlate a report to
    /// server logs.
    #[error("internal error: {message}")]
    Internal { message: String, error_id: String },
    #[error("not found: {0}")]
    NotFound(String),
    #[error("unauthorized")]
    Unauthorized,
    /// An engine error surfaced over RPC, carrying its domain
    /// [`ErrorCode`](mirri_core::ErrorCode) for the wire response.
    #[error(transparent)]
    Engine(#[from] mirri_core::EngineError),
    /// A session-layer error surfaced over RPC.
    #[error(transparent)]
    Session(#[from] mirri_core::SessionError),
}

impl RpcError {
    /// Create an internal error with a fresh correlation id.
    ///
    /// The id appears both in the message and (via
    /// [`ApiErrorResponse::error`]) in the response `data`.
    pub fn internal(message: impl Into<String>) -> Self {
        let error_id = Uuid::new_v4().simple().to_string();
        Self::Internal {
            message: format!("internal error [id: {error_id}]: {}", message.into()),
            error_id,
        }
    }

    /// JSON-RPC 2.0 error code for this error, matching the TS server's
    /// scheme (`-326xx` protocol errors, `-32xxx` application errors).
    ///
    /// Domain errors ([`RpcError::Engine`] / [`RpcError::Session`]) map
    /// through the shared D5 mapping (`mirri_proto::rpc::json_rpc_code_for_domain_code`)
    /// — the same single fact source the embedded engine uses — so
    /// `request.invalid` answers `-32602` (invalid params) and the entity
    /// not-found codes (`session.not_found`, `agent.not_found`) keep the
    /// `-32001` application code clients branch on.
    pub fn json_rpc_code(&self) -> i32 {
        match self {
            RpcError::MethodNotFound(_) => -32601,
            RpcError::InvalidParams(_) => -32602,
            RpcError::Engine(e) => json_rpc_code_for_domain_code(e.code().as_str()) as i32,
            RpcError::Session(e) => json_rpc_code_for_domain_code(e.code().as_str()) as i32,
            RpcError::Internal { .. } => -32603,
            RpcError::NotFound(_) => -32001,
            RpcError::Unauthorized => -32002,
        }
    }

    /// Domain error code string (e.g. `"provider.api_error"`,
    /// `"session.not_found"`) when this error wraps a domain error type;
    /// `None` otherwise.
    pub fn domain_code(&self) -> Option<&str> {
        match self {
            RpcError::Engine(e) => Some(e.code().as_str()),
            RpcError::Session(e) => Some(e.code().as_str()),
            _ => None,
        }
    }

    /// Server-generated error id for internal errors; `None` for other
    /// variants.
    pub fn error_id(&self) -> Option<&str> {
        match self {
            RpcError::Internal { error_id, .. } => Some(error_id.as_str()),
            _ => None,
        }
    }
}

/// JSON-RPC 2.0 response envelope (matching the TS server's format).
#[derive(Debug, Clone, serde::Serialize)]
pub struct ApiResponse<T: serde::Serialize> {
    pub code: i32,
    pub msg: String,
    pub data: T,
    pub request_id: String,
}

impl<T: serde::Serialize> ApiResponse<T> {
    pub fn ok(data: T, request_id: &str) -> Self {
        Self {
            code: 0,
            msg: "success".to_string(),
            data,
            request_id: request_id.to_string(),
        }
    }
}

/// Error response envelope.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ApiErrorResponse {
    pub code: i32,
    pub msg: String,
    /// Domain error code (e.g. `"provider.api_error"`, `"session.not_found"`)
    /// when this error wraps a domain error type; absent otherwise.
    /// Serialized only when present, under the D5 camelCase `domainCode` key
    /// — the same key the WS and IPC transports put inside `error.data`.
    #[serde(skip_serializing_if = "Option::is_none", rename = "domainCode")]
    pub domain_code: Option<String>,
    pub data: serde_json::Value,
    pub request_id: String,
}

impl ApiErrorResponse {
    pub fn error(error: &RpcError, request_id: &str) -> Self {
        let (code, msg, domain_code, data) = match error {
            RpcError::Internal { message, error_id } => (
                error.json_rpc_code(),
                message.clone(),
                None,
                json!({ "error_id": error_id }),
            ),
            RpcError::Engine(engine_error) => {
                // The quota-exhausted error's Retry-After hint rides the
                // response `data` (P11a F-P8-13) — the TS
                // `APIProviderQuotaExhaustedError` parity.
                let data = match engine_error {
                    mirri_core::EngineError::ProviderQuotaExhausted {
                        retry_after_ms: Some(retry_after_ms),
                        ..
                    } => json!({ "retryAfterMs": retry_after_ms }),
                    _ => Value::Null,
                };
                (
                    error.json_rpc_code(),
                    engine_error.to_string(),
                    Some(engine_error.code().as_str().to_string()),
                    data,
                )
            }
            RpcError::Session(session_error) => (
                error.json_rpc_code(),
                session_error.to_string(),
                Some(session_error.code().as_str().to_string()),
                Value::Null,
            ),
            other => (other.json_rpc_code(), other.to_string(), None, Value::Null),
        };
        Self {
            code,
            msg,
            domain_code,
            data,
            request_id: request_id.to_string(),
        }
    }
}

/// Registry of RPC handlers, indexed by method name.
pub struct RpcRegistry {
    handlers: HashMap<&'static str, Arc<dyn RpcHandler>>,
}

impl RpcRegistry {
    pub fn new() -> Self {
        Self {
            handlers: Default::default(),
        }
    }

    pub fn register(&mut self, handler: Arc<dyn RpcHandler>) {
        self.handlers.insert(handler.method(), handler);
    }

    pub fn get(&self, method: &str) -> Option<&Arc<dyn RpcHandler>> {
        self.handlers.get(method)
    }

    /// Every registered method name, sorted. Used for introspection and to
    /// assert the protocol surface in tests.
    pub fn list_methods(&self) -> Vec<&'static str> {
        let mut methods: Vec<&'static str> = self.handlers.keys().copied().collect();
        methods.sort_unstable();
        methods
    }
}

impl Default for RpcRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared registry of in-flight engine turns, keyed by
/// `"{session_id}:{agent_id}:{turn_id}"`.
///
/// One instance is created per [`ServerState`](crate::state::ServerState) and
/// shared with the transport registry (`session.status` / `session.snapshot`
/// derive the session's `running`/`busy` flag from the entries) and the
/// engine fall-through in [`ServerState::dispatch`](crate::state::ServerState):
/// the dispatch wrapper inserts fire-mode `message.send` engine calls here and
/// a watcher removes the entry when the session's event channel reports the
/// turn's terminal event.
#[allow(clippy::type_complexity)]
pub(crate) type InFlightTurns =
    Arc<tokio::sync::Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>;

// ===========================================================================
// Wire views
// ===========================================================================

/// Session summary in the wire format (camelCase, matching the TS server).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionView {
    pub id: String,
    pub title: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub agent_count: usize,
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Whether a turn of this session is in flight (any agent).
    pub busy: bool,
    /// The first agent's model id (empty string before any agent exists —
    /// a plain String keeps every list row shape-identical).
    pub model: String,
    /// The session's aggregated token usage (four buckets) across every
    /// agent — main and subagents alike (SDR-002 full aggregation).
    pub usage: SessionUsageView,
    /// The first agent's context message count.
    pub message_count: usize,
    /// The session's last durable event seq (0 before any durable event).
    pub last_seq: u64,
    /// The session's context root — the engine binds one workspace, so
    /// every session's cwd is that workspace's root (P11a F-P9-8; the
    /// desktop bridge resolves workspace-relative paths against it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// The four-bucket token usage as the wire `SessionView.usage` carries it —
/// deliberately Rust-spelled camelCase, distinct from `agent.getUsage`'s TS
/// `TokenUsage` spelling (ADR-012: no TS consumer reads the session view's
/// bucket keys; the embedded engine's view uses the identical struct).
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionUsageView {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    /// The session's first LLM call — the cold-start baseline (SDR-004).
    /// Absent before any call or for history predating the field.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_call: Option<SessionFirstCallView>,
}

impl From<&mirri_core::TokenUsage> for SessionUsageView {
    fn from(usage: &mirri_core::TokenUsage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            cache_creation_tokens: usage.cache_creation_tokens,
            first_call: None,
        }
    }
}

impl From<mirri_core::TokenUsage> for SessionUsageView {
    fn from(usage: mirri_core::TokenUsage) -> Self {
        Self::from(&usage)
    }
}

/// The first LLM call's four-bucket usage as the wire `SessionView.usage`
/// carries it — the cold-start baseline (SDR-004), same Rust-spelled
/// camelCase as [`SessionUsageView`].
#[derive(Debug, Clone, Copy, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionFirstCallView {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
}

impl From<&mirri_core::TokenUsage> for SessionFirstCallView {
    fn from(usage: &mirri_core::TokenUsage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            cache_creation_tokens: usage.cache_creation_tokens,
        }
    }
}

impl From<&SessionInfo> for SessionView {
    fn from(info: &SessionInfo) -> Self {
        Self {
            id: info.id.clone(),
            title: info.title.clone(),
            created_at: info.created_at,
            updated_at: info.updated_at,
            agent_count: info.agent_count,
            tags: info.tags.clone(),
            status: Some(match info.status {
                mirri_core::SessionStatus::Running => "running".to_string(),
                mirri_core::SessionStatus::Closed => "closed".to_string(),
            }),
            // `SessionInfo` is a static summary (no live scope to read the
            // agents from), so the runtime fields carry neutral values.
            busy: false,
            model: String::new(),
            usage: SessionUsageView::default(),
            message_count: 0,
            last_seq: 0,
            cwd: None,
        }
    }
}

/// `session.get` returns the summary fields plus the agent list.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionDetails {
    #[serde(flatten)]
    pub summary: SessionView,
    pub agents: Vec<AgentView>,
}

/// Per-session agent summary inside [`SessionDetails`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentView {
    pub id: String,
}

/// Build the wire view of a live session scope: the static metadata plus
/// the runtime fields — busy from the in-flight turn registry, model and
/// messageCount from the session's first agent, usage summed across every
/// agent (SDR-002), and lastSeq from the session's durable event channel.
async fn session_view(
    manager: &SessionManager,
    session: &SessionScope,
    in_flight_turns: &InFlightTurns,
    event_channels: &Arc<SessionEventChannelRegistry>,
    epoch: &Arc<str>,
    engine: &Arc<dyn mirri_transport::adapter::EngineAdapter>,
) -> SessionView {
    let metadata = session.metadata().await;
    let session_id = session.id();
    let extras = session.runtime_extras().await;
    let session_prefix = format!("{}:", session_id.as_str());
    let busy = {
        let turns = in_flight_turns.lock().await;
        turns.keys().any(|key| key.starts_with(&session_prefix))
    } || engine
        .in_flight_sessions()
        .await
        .iter()
        .any(|sid| sid == session_id.as_str());
    // The same per-session durable log channel the WS layer subscribes to
    // (the `session.snapshot` derivation): its tail is the session's last
    // durable seq. Get-or-create keeps the read side-effect-free for
    // sessions that never had a durable event yet; a session without live
    // artifacts has no channel and an honest 0.
    let last_seq = manager
        .session_artifacts(&SessionId::new(session_id.as_str()))
        .map(|artifacts| {
            event_channels
                .get_or_create(
                    session_id.as_str(),
                    Arc::clone(&artifacts.event_bus),
                    Arc::clone(epoch),
                )
                .tail_seq()
        })
        .unwrap_or(0);
    SessionView {
        id: session_id.as_str().to_string(),
        title: metadata.title,
        created_at: metadata.created_at,
        updated_at: metadata.updated_at,
        agent_count: session.list_agents().await.len(),
        tags: metadata.tags.into_iter().collect(),
        status: Some("running".to_string()),
        busy,
        model: extras.model,
        usage: SessionUsageView {
            first_call: extras.first_call.as_ref().map(SessionFirstCallView::from),
            ..SessionUsageView::from(extras.usage)
        },
        message_count: extras.message_count,
        last_seq,
        cwd: Some(manager.workspace().cwd().to_string_lossy().into_owned()),
    }
}

/// Treat a missing/`null` params value as an empty object so callers may
/// omit the parameters entirely.
fn normalize_params(params: Value) -> Value {
    if params.is_null() { json!({}) } else { params }
}

/// Serialize a typed wire result (a `#[derive(Serialize)]` response struct)
/// into the RPC [`Value`]. Serialization failures indicate a programming
/// error in the struct definition, never a bad request — they surface as an
/// internal error with a correlation id.
fn to_json<T: Serialize>(result: T) -> RpcResult<Value> {
    serde_json::to_value(result).map_err(|e| RpcError::internal(e.to_string()))
}

/// Shared `noResult`-style success envelope used by the transport-owned
/// void-ish methods (`approval.decide` etc. — the v2 contract models those
/// methods as void, answered `{ok: true}` on the current wire shape).
#[derive(Debug, Serialize)]
struct SessionOkResult {
    ok: bool,
}

/// Common params for handlers that address one session by id.
///
/// The wire historically carries both key spellings — clients send
/// `sessionId` (camelCase protocol shape) or `session_id` (the Web client's
/// RPC layer) — so the field accepts both; the canonical key remains
/// `sessionId`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionIdParams {
    #[serde(rename = "sessionId", alias = "session_id")]
    session_id: String,
}

/// A hook the transport-owned session handlers use to resume a
/// persisted-only session on demand through the engine adapter — the full
/// embedded assembly (reverse-RPC callback, MCP overlay registration, event
/// relay, cron driver) rather than this layer's bare wire replay.
pub type EngineRestoreHook = Arc<
    dyn Fn(
            String,
        )
            -> futures::future::BoxFuture<'static, Result<(), mirri_transport::adapter::EngineError>>
        + Send
        + Sync,
>;

/// The transport layer's session-routing seam (W3, design 08 §3): every
/// session-addressing transport-owned handler resolves the addressed
/// session through this trait instead of holding the single bound
/// [`SessionManager`]. The default implementation
/// ([`SingleManagerRouting`]) preserves today's single-workspace semantics;
/// the composition root (`mirri serve`) injects a registry-backed
/// implementation so a session of a non-default workspace resolves on its
/// own runtime (active table → index fallback → bucket probe → not-found —
/// never a silent default).
pub trait SessionRouting: Send + Sync {
    /// The live session owning `session_id`, routed through the engine's
    /// workspace registry. `None` when no workspace owns it or it is not
    /// live.
    fn live_session(
        &self,
        session_id: &str,
    ) -> futures::future::BoxFuture<'static, Option<Arc<SessionScope>>>;

    /// The live session's artifacts (gates / event bus), same routing.
    /// `None` when the session is not live (artifacts are live-only).
    fn session_artifacts(&self, session_id: &str) -> Option<SessionArtifacts>;

    /// The owning manager for a (live or persisted) session id — the handler
    /// performing this layer's own replay resumes on it. `None` when no
    /// workspace owns the id.
    fn owning_manager(&self, session_id: &str) -> Option<Arc<SessionManager>>;

    /// Whether the owning manager remembers the session as closed in this
    /// process — restore-on-demand must not resurrect a session the user
    /// closed.
    fn is_closed(&self, session_id: &str) -> bool;

    /// Restore-on-demand: resume a persisted-only session through the engine
    /// (the [`EngineRestoreHook`] mechanism). Best-effort — errors are
    /// swallowed by the resolution helper.
    fn restore_session(&self, session_id: &str) -> futures::future::BoxFuture<'static, ()>;
}

/// The default [`SessionRouting`] implementation: today's semantics — every
/// resolution goes to the one bound manager, and restore-on-demand (when a
/// hook is attached) resumes through the engine adapter. Installed by every
/// handler constructor and by [`crate::state::ServerState::with_engine`];
/// the registry-backed implementation replaces it at the composition root.
pub struct SingleManagerRouting {
    manager: Arc<SessionManager>,
    restore_hook: Option<EngineRestoreHook>,
}

impl SingleManagerRouting {
    /// A live-only routing over one manager (no restore-on-demand) — the
    /// handlers' constructor default.
    pub fn live(manager: Arc<SessionManager>) -> Self {
        Self {
            manager,
            restore_hook: None,
        }
    }

    /// A routing over one manager with restore-on-demand through the given
    /// engine hook — [`crate::state::ServerState::with_engine`]'s wiring
    /// (the fields stay private to this module).
    pub fn with_restore_hook(
        manager: Arc<SessionManager>,
        restore_hook: Option<EngineRestoreHook>,
    ) -> Self {
        Self {
            manager,
            restore_hook,
        }
    }
}

impl SessionRouting for SingleManagerRouting {
    fn live_session(
        &self,
        session_id: &str,
    ) -> futures::future::BoxFuture<'static, Option<Arc<SessionScope>>> {
        let manager = Arc::clone(&self.manager);
        let id = SessionId::new(session_id);
        Box::pin(async move { manager.get_session(&id).await })
    }

    fn session_artifacts(&self, session_id: &str) -> Option<SessionArtifacts> {
        self.manager.session_artifacts(&SessionId::new(session_id))
    }

    fn owning_manager(&self, _session_id: &str) -> Option<Arc<SessionManager>> {
        Some(Arc::clone(&self.manager))
    }

    fn is_closed(&self, session_id: &str) -> bool {
        self.manager.is_closed(&SessionId::new(session_id))
    }

    fn restore_session(&self, session_id: &str) -> futures::future::BoxFuture<'static, ()> {
        match self.restore_hook.clone() {
            Some(hook) => {
                let sid = session_id.to_string();
                Box::pin(async move {
                    // Best-effort — the failure surfaces through the caller's
                    // own not-found error — but never silent: a swallowed
                    // restore error (e.g. a corrupt wire log) is otherwise
                    // indistinguishable from an unknown session id on every
                    // read face that reports it.
                    if let Err(error) = hook(sid.clone()).await {
                        tracing::warn!(
                            session_id = %sid,
                            %error,
                            "restore-on-demand failed; read faces will report the session as not found"
                        );
                    }
                })
            }
            None => Box::pin(std::future::ready(())),
        }
    }
}

/// Resolve `session_id` to a live session through the routing seam,
/// resuming it through the routing's restore hook when it exists only on
/// disk (a server restart left it persisted). A session still missing
/// after the resume attempt (unknown id, or the resume failed) yields
/// `None` — the caller maps that to the method's NotFound error. The
/// restore path itself routes through the seam, so a persisted session of
/// a non-default workspace resumes on its own runtime.
pub(crate) async fn live_or_restored_session(
    routing: &dyn SessionRouting,
    session_id: &str,
) -> Option<Arc<SessionScope>> {
    if let Some(session) = routing.live_session(session_id).await {
        return Some(session);
    }
    // A session this process closed stays closed — restore-on-demand is
    // for sessions a restart left persisted, not for resurrecting closed
    // ones behind the user's back.
    if routing.is_closed(session_id) {
        return None;
    }
    // A racing restore may already have resumed the session (or this call's
    // resume lost the race) — either way the retry below sees the live
    // session; a genuine failure falls through to the same retry-then-None.
    routing.restore_session(session_id).await;
    routing.live_session(session_id).await
}

pub struct GetSessionHandler {
    manager: Arc<SessionManager>,
    routing: Arc<dyn SessionRouting>,
    in_flight_turns: InFlightTurns,
    event_channels: Arc<SessionEventChannelRegistry>,
    epoch: Arc<str>,
    /// The engine-side in-flight turn registry (driver-injected turns the
    /// transport never tracked) — unioned into the view's busy flag.
    engine: Arc<dyn mirri_transport::adapter::EngineAdapter>,
}

impl GetSessionHandler {
    pub fn new(
        manager: Arc<SessionManager>,
        in_flight_turns: InFlightTurns,
        event_channels: Arc<SessionEventChannelRegistry>,
        epoch: Arc<str>,
        engine: Arc<dyn mirri_transport::adapter::EngineAdapter>,
    ) -> Self {
        Self {
            routing: Arc::new(SingleManagerRouting::live(Arc::clone(&manager))),
            manager,
            in_flight_turns,
            event_channels,
            epoch,
            engine,
        }
    }

    /// Route session resolutions through an explicit [`SessionRouting`]
    /// implementation (the composition root's registry-backed routing).
    pub fn with_routing(mut self, routing: Arc<dyn SessionRouting>) -> Self {
        self.routing = routing;
        self
    }

    /// Attach the engine restore hook used to resume persisted-only
    /// sessions on demand (see [`EngineRestoreHook`]) — the single-manager
    /// routing's restore leg.
    pub fn with_restore_hook(mut self, restore_hook: Option<EngineRestoreHook>) -> Self {
        if let Some(hook) = restore_hook {
            self.routing = Arc::new(SingleManagerRouting {
                manager: Arc::clone(&self.manager),
                restore_hook: Some(hook),
            });
        }
        self
    }
}

#[async_trait]
impl RpcHandler for GetSessionHandler {
    fn method(&self) -> &'static str {
        "session.get"
    }

    async fn handle(&self, params: Value) -> RpcResult<Value> {
        let params: SessionIdParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| RpcError::InvalidParams(e.to_string()))?;

        let session = live_or_restored_session(self.routing.as_ref(), &params.session_id)
            .await
            .ok_or_else(|| RpcError::NotFound(params.session_id.clone()))?;

        let agents: Vec<AgentView> = session
            .list_agents()
            .await
            .into_iter()
            .map(|agent_id| AgentView {
                id: agent_id.as_str().to_string(),
            })
            .collect();
        // The view's manager leg (lastSeq / cwd) follows the owning
        // workspace's manager (the routing seam, W3) — a non-default
        // session reports its own workspace's paths.
        let owning = self
            .routing
            .owning_manager(&params.session_id)
            .unwrap_or_else(|| Arc::clone(&self.manager));
        let details = SessionDetails {
            summary: session_view(
                &owning,
                &session,
                &self.in_flight_turns,
                &self.event_channels,
                &self.epoch,
                &self.engine,
            )
            .await,
            agents,
        };
        to_json(details)
    }
}

/// The serve wire's directory-grant sink (plan D3): the `approval.decide`
/// override applies a granted dir (the pending ask's `grant_dir`) into the
/// session's shared sandbox and — for a persist grant — into
/// `<projectRoot>/.mirri-code/local.toml`'s `workspace.additional_dir`, the
/// same semantics as the embedded engine's sink (the server is a
/// composition root: file IO is its job, the core never touches the
/// filesystem).
struct ServeAdditionalDirGrantSink {
    paths: Arc<mirri_core::tools::WorkspacePaths>,
    project_root: PathBuf,
}

impl ServeAdditionalDirGrantSink {
    fn new(paths: Arc<mirri_core::tools::WorkspacePaths>, project_root: PathBuf) -> Self {
        Self { paths, project_root }
    }

    /// Write `dir` into the workspace-local `local.toml`, preserving every
    /// other key the file carried; a dir already listed is a no-op.
    fn persist_dir(&self, dir: &Path) -> Result<(), String> {
        let config_path = self.project_root.join(".mirri-code").join("local.toml");
        let file = if config_path.exists() {
            let text = std::fs::read_to_string(&config_path)
                .map_err(|e| format!("failed to read {}: {e}", config_path.display()))?;
            if text.trim().is_empty() {
                toml::Value::Table(Default::default())
            } else {
                toml::from_str(&text)
                    .map_err(|e| format!("invalid TOML in {}: {e}", config_path.display()))?
            }
        } else {
            toml::Value::Table(Default::default())
        };
        let mut root = file;
        let workspace = root
            .as_table_mut()
            .ok_or_else(|| "workspace must be a table".to_string())?
            .entry("workspace")
            .or_insert_with(|| toml::Value::Table(Default::default()));
        let table = workspace
            .as_table_mut()
            .ok_or_else(|| "workspace must be a table".to_string())?;
        let desired = dir.to_string_lossy().into_owned();
        let existing = table
            .get("additional_dir")
            .and_then(toml::Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(toml::Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if existing.iter().any(|entry| entry == &desired) {
            return Ok(());
        }
        let mut dirs = existing;
        dirs.push(desired);
        table.insert(
            "additional_dir".to_string(),
            toml::Value::Array(dirs.into_iter().map(toml::Value::String).collect()),
        );
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create {}: {e}", parent.display()))?;
        }
        let text = toml::to_string(&root).map_err(|e| format!("failed to serialize local.toml: {e}"))?;
        std::fs::write(&config_path, format!("{text}\n"))
            .map_err(|e| format!("failed to write {}: {e}", config_path.display()))
    }
}

impl mirri_core::AdditionalDirGrantSink for ServeAdditionalDirGrantSink {
    fn apply_grant(&self, dir: &Path, persist: bool) {
        // The sandbox widening is idempotent and session-wide (the shared
        // `WorkspacePaths` handle, exactly like the embedded engine).
        self.paths.add_additional(dir.to_path_buf());
        if persist
            && let Err(error) = self.persist_dir(dir)
        {
            tracing::warn!(
                %error,
                "could not persist the granted directory; the grant stays session-only"
            );
        }
    }
}

/// Build the serve-wire grant sink for a live session: its shared sandbox
/// plus the project root that owns it (the nearest `.git` ancestor — the
/// same resolution the embedded engine applies). `None` for legacy sessions
/// without a workspace sandbox: a session/persist grant decision degrades
/// to a one-off approval.
fn serve_session_grant_sink(
    session: &Arc<SessionScope>,
) -> Option<Arc<dyn mirri_core::AdditionalDirGrantSink>> {
    let paths = session.workspace_paths()?;
    let cwd = session.workspace_root()?;
    // The embedded `find_project_root` walk: the nearest ancestor (inclusive)
    // carrying a `.git` marker, falling back to the cwd itself.
    let mut current = cwd.to_path_buf();
    loop {
        if current.join(".git").exists() {
            return Some(Arc::new(ServeAdditionalDirGrantSink::new(paths, current)));
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => {
                return Some(Arc::new(ServeAdditionalDirGrantSink::new(
                    paths,
                    cwd.to_path_buf(),
                )))
            }
        }
    }
}

/// `approval.decide` — resolve a pending tool-approval request through the
/// addressed session's [`ApprovalGate`](mirri_core::ApprovalGate) (the
/// serve-wire counterpart of the embedded handler; same wire shapes). An
/// optional `scope: "session"` mirrors the klient `ApprovalResponse.scope`
/// field: for a plain ask the decision is recorded as session-scoped and the
/// approval-rule pattern is memorized for the rest of the session, so later
/// same-pattern calls no longer ask (anything else or absent is a one-off
/// decision). For a grant-carrying ask (plan D3) `scope: "session"` /
/// `"persist"` instead applies the directory grant through the session's
/// sink and resolves plain `Approved` — a directory grant is its own
/// memory, the tool-name pattern is never memorized.
pub struct ResolveApprovalHandler {
    routing: Arc<dyn SessionRouting>,
}

impl ResolveApprovalHandler {
    pub fn new(manager: Arc<SessionManager>) -> Self {
        Self {
            routing: Arc::new(SingleManagerRouting::live(manager)),
        }
    }

    /// Route session resolutions through an explicit [`SessionRouting`]
    /// implementation (the composition root's registry-backed routing).
    pub fn with_routing(mut self, routing: Arc<dyn SessionRouting>) -> Self {
        self.routing = routing;
        self
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResolveApprovalParams {
    #[serde(rename = "sessionId", alias = "session_id")]
    session_id: String,
    #[serde(rename = "requestId", alias = "request_id")]
    request_id: String,
    approved: bool,
    #[serde(default)]
    reason: Option<String>,
    /// `Some("session")` memorizes the approval as a session-scoped rule
    /// (the klient `ApprovalResponse.scope` field; anything else or absent
    /// is a one-off decision). For a grant-carrying ask, `"session"` /
    /// `"persist"` apply the directory grant instead (plan D3) — persist
    /// additionally writes the workspace local.toml.
    #[serde(default)]
    scope: Option<String>,
}

#[async_trait]
impl RpcHandler for ResolveApprovalHandler {
    fn method(&self) -> &'static str {
        "approval.decide"
    }

    async fn handle(&self, params: Value) -> RpcResult<Value> {
        let params: ResolveApprovalParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| RpcError::InvalidParams(e.to_string()))?;

        // The session must still be live for its approvals to be resolvable —
        // and it is the sink's owner (its shared sandbox + project root).
        let session = live_or_restored_session(self.routing.as_ref(), &params.session_id)
            .await
            .ok_or_else(|| RpcError::NotFound(params.session_id.clone()))?;

        let request_id = Uuid::parse_str(&params.request_id)
            .map_err(|e| RpcError::InvalidParams(format!("invalid request_id: {e}")))?;
        // Plan D3: a grant-carrying ask (the pending snapshot's `grant_dir`)
        // approved with `session`/`persist` scope applies the directory
        // grant through the session's sink and resolves plain `Approved` —
        // never `ApprovedForSession` (a directory grant is its own memory;
        // memorizing the tool-name pattern would over-approve every later
        // call of the same tool). Non-grant asks keep the pre-D3 mapping.
        let artifacts = self
            .routing
            .session_artifacts(&params.session_id)
            .ok_or_else(|| RpcError::NotFound(params.session_id.clone()))?;
        let decision = if params.approved {
            let pending = artifacts.approval_gate.snapshot(request_id);
            if let Some(dir) = pending.as_ref().and_then(|s| s.grant_dir.clone())
                && let Some(scope) = params.scope.as_deref()
                && (scope == "session" || scope == "persist")
            {
                if let Some(sink) = serve_session_grant_sink(&session) {
                    sink.apply_grant(&dir, scope == "persist");
                }
                mirri_core::ApprovalDecision::Approved
            } else if params.scope.as_deref() == Some("session") {
                mirri_core::ApprovalDecision::ApprovedForSession
            } else {
                mirri_core::ApprovalDecision::Approved
            }
        } else {
            mirri_core::ApprovalDecision::Denied(
                params
                    .reason
                    .unwrap_or_else(|| "denied by user".to_string()),
            )
        };
        // Resolve through the session's own gate — approvals of other
        // sessions are never reachable (or affected) here.
        let resolved = artifacts.approval_gate.resolve(request_id, decision);
        if !resolved {
            return Err(RpcError::NotFound(format!(
                "approval request not found: {}",
                params.request_id
            )));
        }

        to_json(SessionOkResult { ok: true })
    }
}

/// `question.answer` — answer a pending user question through the addressed
/// session's [`QuestionGate`](mirri_core::QuestionGate).
pub struct AnswerQuestionHandler {
    routing: Arc<dyn SessionRouting>,
}

impl AnswerQuestionHandler {
    pub fn new(manager: Arc<SessionManager>) -> Self {
        Self {
            routing: Arc::new(SingleManagerRouting::live(manager)),
        }
    }

    /// Route session resolutions through an explicit [`SessionRouting`]
    /// implementation (the composition root's registry-backed routing).
    pub fn with_routing(mut self, routing: Arc<dyn SessionRouting>) -> Self {
        self.routing = routing;
        self
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AnswerQuestionParams {
    #[serde(rename = "sessionId", alias = "session_id")]
    session_id: String,
    #[serde(rename = "requestId", alias = "request_id")]
    request_id: String,
    answer: String,
}

#[async_trait]
impl RpcHandler for AnswerQuestionHandler {
    fn method(&self) -> &'static str {
        "question.answer"
    }

    async fn handle(&self, params: Value) -> RpcResult<Value> {
        let params: AnswerQuestionParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| RpcError::InvalidParams(e.to_string()))?;

        live_or_restored_session(self.routing.as_ref(), &params.session_id)
            .await
            .ok_or_else(|| RpcError::NotFound(params.session_id.clone()))?;

        let request_id = Uuid::parse_str(&params.request_id)
            .map_err(|e| RpcError::InvalidParams(format!("invalid request_id: {e}")))?;
        // Answer through the session's own gate — questions of other
        // sessions are never reachable (or affected) here.
        let artifacts = self
            .routing
            .session_artifacts(&params.session_id)
            .ok_or_else(|| RpcError::NotFound(params.session_id.clone()))?;
        let answered = artifacts.question_gate.answer(request_id, params.answer);
        if !answered {
            return Err(RpcError::NotFound(format!(
                "question not found: {}",
                params.request_id
            )));
        }

        to_json(SessionOkResult { ok: true })
    }
}

// ===========================================================================
// question.dismiss
// ===========================================================================

/// `question.dismiss` — dismiss a pending user question without answering
/// it, through the addressed session's [`QuestionGate`](mirri_core::QuestionGate).
///
/// The awaiting `ask` future resolves to `None`; a `QuestionDismissed`
/// event is published so clients can track the cancellation.
pub struct DismissQuestionHandler {
    routing: Arc<dyn SessionRouting>,
}

impl DismissQuestionHandler {
    pub fn new(manager: Arc<SessionManager>) -> Self {
        Self {
            routing: Arc::new(SingleManagerRouting::live(manager)),
        }
    }

    /// Route session resolutions through an explicit [`SessionRouting`]
    /// implementation (the composition root's registry-backed routing).
    pub fn with_routing(mut self, routing: Arc<dyn SessionRouting>) -> Self {
        self.routing = routing;
        self
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DismissQuestionParams {
    #[serde(rename = "sessionId", alias = "session_id")]
    session_id: String,
    #[serde(rename = "requestId", alias = "request_id")]
    request_id: String,
}

/// `question.dismiss` result: the `{"ok": true, "dismissed": true}` ack
/// (current wire shape).
#[derive(Debug, Serialize)]
struct DismissQuestionResult {
    ok: bool,
    dismissed: bool,
}

#[async_trait]
impl RpcHandler for DismissQuestionHandler {
    fn method(&self) -> &'static str {
        "question.dismiss"
    }

    async fn handle(&self, params: Value) -> RpcResult<Value> {
        let params: DismissQuestionParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| RpcError::InvalidParams(e.to_string()))?;

        live_or_restored_session(self.routing.as_ref(), &params.session_id)
            .await
            .ok_or_else(|| RpcError::NotFound(params.session_id.clone()))?;

        let request_id = Uuid::parse_str(&params.request_id)
            .map_err(|e| RpcError::InvalidParams(format!("invalid request_id: {e}")))?;
        // Dismiss through the session's own gate — questions of other
        // sessions are never reachable (or affected) here.
        let artifacts = self
            .routing
            .session_artifacts(&params.session_id)
            .ok_or_else(|| RpcError::NotFound(params.session_id.clone()))?;
        let dismissed = artifacts.question_gate.dismiss(request_id);
        if !dismissed {
            return Err(RpcError::NotFound(format!(
                "question not found: {}",
                params.request_id
            )));
        }

        to_json(DismissQuestionResult {
            ok: true,
            dismissed: true,
        })
    }
}

// ===========================================================================
// config.get / config.set / config.watch
// ===========================================================================

/// `config.get` — the current configuration as the MirriConfig wire shape
/// (camelCase, every field present — null when unset), with provider api
/// keys redacted. Server identity (version/server id/started time) lives in
/// the REST `/meta` endpoint, not here (`core.info` is a known M1 gap on
/// this backend). The optional `reload` param is ignored, mirroring the
/// embedded engine.
pub struct WatchConfigHandler;

impl Default for WatchConfigHandler {
    fn default() -> Self {
        Self
    }
}

impl WatchConfigHandler {
    pub fn new() -> Self {
        Self
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WatchConfigParams {
    // Accepted for protocol-shape validation; there is no config-change
    // stream, so the filter is never read after parsing.
    #[serde(rename = "domain", default)]
    _domain: Option<String>,
}

#[async_trait]
impl RpcHandler for WatchConfigHandler {
    fn method(&self) -> &'static str {
        "config.watch"
    }

    async fn handle(&self, params: Value) -> RpcResult<Value> {
        let _params: WatchConfigParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| RpcError::InvalidParams(e.to_string()))?;

        to_json(WatchConfigResult {
            ok: true,
            note: "Config watch is not yet supported in mirri-server",
        })
    }
}

/// `config.watch` result — the honest acknowledgement: the server has no
/// config-change stream, so the note explains why nothing is subscribed.
#[derive(Debug, Serialize)]
struct WatchConfigResult {
    ok: bool,
    note: &'static str,
}

/// `mcp.subscribe` — the app-level MCP pool event subscription (M7, F-9,
/// ADR-010). On the WebSocket endpoint the request is handled
/// connection-locally (the `session.subscribe` pattern, `ws_rpc.rs`): the
/// connection registers on the engine's pool broadcast and receives
/// `event.mcp.server.status` notifications until it disconnects. This
/// shared-dispatch handler serves the transports with no notification
/// channel (HTTP, IPC): the honest acknowledgement explains that the
/// subscription surface lives on the WebSocket endpoint.
pub struct McpSubscribeHandler;

impl Default for McpSubscribeHandler {
    fn default() -> Self {
        Self
    }
}

impl McpSubscribeHandler {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl RpcHandler for McpSubscribeHandler {
    fn method(&self) -> &'static str {
        "mcp.subscribe"
    }

    async fn handle(&self, _params: Value) -> RpcResult<Value> {
        to_json(McpSubscribeResult {
            ok: true,
            note: "mcp.subscribe notifications are delivered on the WebSocket endpoint's connection that requested them",
        })
    }
}

/// `mcp.subscribe` result on the non-push transports — the honest
/// acknowledgement that the event stream rides the WebSocket connection.
#[derive(Debug, Serialize)]
struct McpSubscribeResult {
    ok: bool,
    note: &'static str,
}

// ===========================================================================
// session.resume
// ===========================================================================

/// `session.resume` — resume a persisted session by replaying its wire log.
pub struct ListAgentsHandler {
    routing: Arc<dyn SessionRouting>,
}

impl ListAgentsHandler {
    pub fn new(manager: Arc<SessionManager>) -> Self {
        Self {
            routing: Arc::new(SingleManagerRouting::live(manager)),
        }
    }

    /// Route session resolutions through an explicit [`SessionRouting`]
    /// implementation (the composition root's registry-backed routing).
    pub fn with_routing(mut self, routing: Arc<dyn SessionRouting>) -> Self {
        self.routing = routing;
        self
    }
}

#[async_trait]
impl RpcHandler for ListAgentsHandler {
    fn method(&self) -> &'static str {
        "agent.list"
    }

    async fn handle(&self, params: Value) -> RpcResult<Value> {
        let params: SessionIdParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| RpcError::InvalidParams(e.to_string()))?;

        let session = live_or_restored_session(self.routing.as_ref(), &params.session_id)
            .await
            .ok_or_else(|| RpcError::NotFound(params.session_id.clone()))?;

        let lifecycle = AgentLifecycle::new(Arc::clone(&session));
        let agents = lifecycle.list_agents().await;
        to_json(agents)
    }
}

// ===========================================================================
// agent.get
// ===========================================================================

/// `agent.get` — get details of a specific agent in a session.
pub struct GetAgentHandler {
    routing: Arc<dyn SessionRouting>,
}

impl GetAgentHandler {
    pub fn new(manager: Arc<SessionManager>) -> Self {
        Self {
            routing: Arc::new(SingleManagerRouting::live(manager)),
        }
    }

    /// Route session resolutions through an explicit [`SessionRouting`]
    /// implementation (the composition root's registry-backed routing).
    pub fn with_routing(mut self, routing: Arc<dyn SessionRouting>) -> Self {
        self.routing = routing;
        self
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetAgentParams {
    #[serde(rename = "sessionId", alias = "session_id")]
    session_id: String,
    #[serde(rename = "agentId", alias = "agent_id")]
    agent_id: String,
}

#[async_trait]
impl RpcHandler for GetAgentHandler {
    fn method(&self) -> &'static str {
        "agent.get"
    }

    async fn handle(&self, params: Value) -> RpcResult<Value> {
        let params: GetAgentParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| RpcError::InvalidParams(e.to_string()))?;

        let session = live_or_restored_session(self.routing.as_ref(), &params.session_id)
            .await
            .ok_or_else(|| RpcError::NotFound(params.session_id.clone()))?;

        let lifecycle = AgentLifecycle::new(Arc::clone(&session));
        let agents = lifecycle.list_agents().await;
        let agent = agents
            .into_iter()
            .find(|a| a.id == params.agent_id)
            .ok_or_else(|| RpcError::NotFound(params.agent_id.clone()))?;

        to_json(agent)
    }
}

// ===========================================================================
// Shared helpers for the Phase 2 protocol surface
// ===========================================================================

/// Resolve the agent addressed by an optional `agentId`, defaulting to the
/// session's first agent — the same addressing rule `message.send` uses.
/// Returns `None` when the named agent does not exist or the session has no
/// agents at all.
async fn resolve_session_agent(
    session: &SessionScope,
    agent_id: Option<&str>,
) -> Option<Arc<AgentScope>> {
    match agent_id {
        Some(id) => session.get_agent(&AgentId::new(id)).await,
        None => match session.list_agents().await.into_iter().next() {
            Some(first) => session.get_agent(&first).await,
            None => None,
        },
    }
}

/// Resolve the agent addressed by an optional `agentId` (defaulting to the
/// session's first agent), or fail with a precise not-found error: the named
/// agent when `agentId` was given, else "session has no agents yet".
async fn require_session_agent(
    session: &SessionScope,
    session_id: &str,
    agent_id: Option<&str>,
) -> RpcResult<Arc<AgentScope>> {
    match agent_id {
        Some(agent_id) => Ok(resolve_session_agent(session, Some(agent_id))
            .await
            .ok_or_else(|| RpcError::NotFound(agent_id.to_string()))?),
        None => Ok(resolve_session_agent(session, None).await.ok_or_else(|| {
            RpcError::NotFound(format!("session '{session_id}' has no agents yet"))
        })?),
    }
}

/// Project a [`ContextMessage`]'s content onto the TS protocol content-part
/// shapes (the `protocolMessage.ts` union: `text` / `tool_use` /
/// `tool_result` / `thinking` / `image` / `video` / `file`) — the shared
/// read-face projection behind `message.list` and `session.snapshot` rows.
///
/// The persisted wire spellings (`think`, `image_url`, `audio_url`, …) are a
/// storage contract and never change on disk; the projection happens only at
/// this serialization boundary. Plain-text rows keep the historical string
/// collapse; anything structured becomes an array of protocol parts. Shapes
/// with no protocol home (audio, inline non-image files) degrade to text
/// markers instead of leaving the union; a tool result without a correlation
/// id degrades to its raw content (the protocol `tool_result` requires a
/// non-empty `tool_call_id`).
fn message_content_to_protocol_json(ctx: &mirri_core::context::ContextMessage) -> Value {
    let message = &ctx.message;
    // Tool results project to a single `tool_result` part bound to the tool
    // call id, carrying the context message's error flag when set (the
    // persisted `isError` — replay round-trips it). `output` keeps the
    // historical content read face: a string for plain text / empty content,
    // else a part array.
    if message.role == Role::Tool {
        if message
            .tool_call_id
            .as_deref()
            .map(str::is_empty)
            .unwrap_or(true)
        {
            // A tool row with no correlation id (legacy wire records) cannot
            // form a protocol `tool_result`: degrade to the raw content so
            // the row stays schema-valid without inventing a fake id.
            if !message.content.is_empty()
                && message
                    .content
                    .iter()
                    .all(|part| matches!(part, ContentPart::Text { .. }))
            {
                return Value::String(message.text_content());
            }
            if message.content.is_empty() {
                return Value::String(String::new());
            }
            return Value::Array(project_content_parts(&message.content));
        }
        let output = if !message.content.is_empty()
            && message
                .content
                .iter()
                .all(|part| matches!(part, ContentPart::Text { .. }))
        {
            Value::String(message.text_content())
        } else if message.content.is_empty() {
            Value::String(String::new())
        } else {
            Value::Array(project_content_parts(&message.content))
        };
        let mut tool_result = json!({
            "type": "tool_result",
            "tool_call_id": message.tool_call_id.clone().expect("checked non-empty above"),
            "output": output,
        });
        if ctx.is_error {
            tool_result["is_error"] = json!(true);
        }
        return Value::Array(vec![tool_result]);
    }

    // Pure-text rows keep the string form — except when the message carries
    // tool calls, which must not be dropped (the R1 fix).
    if !message.content.is_empty()
        && message
            .content
            .iter()
            .all(|part| matches!(part, ContentPart::Text { .. }))
        && message.tool_calls.is_empty()
    {
        return Value::String(message.text_content());
    }
    if message.content.is_empty() && message.tool_calls.is_empty() {
        return Value::String(String::new());
    }

    let mut parts = project_content_parts(&message.content);
    for call in &message.tool_calls {
        // Parse the raw JSON argument string into a structured `input`; keep
        // the raw string when the provider did not emit valid JSON.
        let input = serde_json::from_str::<Value>(&call.function.arguments)
            .unwrap_or_else(|_| Value::String(call.function.arguments.clone()));
        parts.push(json!({
            "type": "tool_use",
            "tool_call_id": call.id,
            "tool_name": call.function.name,
            "input": input,
        }));
    }
    Value::Array(parts)
}

/// Map every persisted content part onto its TS protocol part shape.
fn project_content_parts(parts: &[ContentPart]) -> Vec<Value> {
    parts.iter().map(project_content_part).collect()
}

/// Map one persisted content part onto its TS protocol part shape. Legacy
/// spellings without a protocol home degrade instead of throwing:
/// `audio_url` flattens to the `[audio:…]` text marker the TS projector
/// uses, inline non-image file data flattens to a `[file:…]` marker, and
/// inline image file data rides the protocol `image` part's base64 source
/// (the `file` part requires a store id the inline form does not carry).
fn project_content_part(part: &ContentPart) -> Value {
    match part {
        ContentPart::Text { text } => json!({ "type": "text", "text": text }),
        ContentPart::Think { text } => json!({ "type": "thinking", "thinking": text }),
        ContentPart::ImageUrl { image_url, .. } => json!({
            "type": "image",
            "source": protocol_image_source(&image_url.url),
        }),
        ContentPart::AudioUrl { audio_url } => json!({
            "type": "text",
            "text": format!("[audio:{audio_url}]"),
        }),
        ContentPart::VideoUrl { video_url } => json!({
            "type": "video",
            "source": { "kind": "url", "url": video_url },
        }),
        ContentPart::File {
            file_id,
            data,
            name,
            media_type,
            size,
        } => {
            let effective_media_type = media_type.clone().or_else(|| {
                data.as_deref()
                    .and_then(parse_data_uri)
                    .map(|(media_type, _)| media_type)
            });
            match file_id {
                // Inbound wire form (a files-store reference): a protocol
                // `file` part; an unknown media type falls back to the
                // generic binary type (the schema requires a non-empty
                // string).
                Some(file_id) => json!({
                    "type": "file",
                    "file_id": file_id,
                    "name": name.clone().unwrap_or_default(),
                    "media_type": effective_media_type
                        .unwrap_or_else(|| "application/octet-stream".to_string()),
                    "size": size.unwrap_or(0),
                }),
                // Context-memory inline form (resolved file bytes, a data:
                // URI): image bytes ride the protocol `image` part's base64
                // source; everything else has no protocol home and degrades
                // to the `[file:…]` marker rather than leaving the union.
                None => {
                    let parsed = data.as_deref().and_then(parse_data_uri);
                    match parsed {
                        Some((media_type, payload)) if media_type.starts_with("image/") => {
                            json!({
                                "type": "image",
                                "source": { "kind": "base64", "media_type": media_type, "data": payload },
                            })
                        }
                        _ => json!({
                            "type": "text",
                            "text": format!("[file:{}]", name.clone().unwrap_or_default()),
                        }),
                    }
                }
            }
        }
    }
}

/// A remote url keeps the protocol `url` source kind (with no provider id —
/// the rust wire part does not carry one); a `data:` URI degrades to the
/// `base64` kind with the media type split out of the URI, matching the
/// protocol's `{ kind: "base64", media_type, data }` source shape.
fn protocol_image_source(url: &str) -> Value {
    match parse_data_uri(url) {
        Some((media_type, data)) => json!({
            "kind": "base64",
            "media_type": media_type,
            "data": data,
        }),
        None => json!({ "kind": "url", "url": url }),
    }
}

/// Split `data:<media-type>;base64,<payload>` (or `data:<media-type>,<raw>`)
/// into its media type and payload. `None` when the string is not a
/// well-formed data URI.
fn parse_data_uri(uri: &str) -> Option<(String, String)> {
    let rest = uri.strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    let media_type = meta.split(';').next().unwrap_or_default();
    if media_type.is_empty() || payload.is_empty() {
        return None;
    }
    Some((media_type.to_string(), payload.to_string()))
}

/// Resolve the session's file root: the session's own workspace root when it
/// has one, else the workspace manager's cwd.
fn session_file_root(session: &SessionScope, manager: &SessionManager) -> PathBuf {
    session
        .workspace_root()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| manager.workspace().cwd().to_path_buf())
}

/// Parse a `message.list` cursor id (`"msg-<entry_seq>"`) back into its
/// entry number. The ids anchor to the session-cumulative entry numbering
/// (see [`message_list_row`]), resolved against the current history by
/// [`resolve_message_position`].
fn parse_message_seq(message_id: &str) -> Result<usize, RpcError> {
    message_id
        .strip_prefix("msg-")
        .and_then(|suffix| suffix.parse::<usize>().ok())
        .ok_or_else(|| {
            RpcError::InvalidParams(format!(
                "invalid message id '{message_id}' (expected 'msg-<entry_seq>')"
            ))
        })
}

/// Render gate-snapshot epoch milliseconds as an RFC 3339 timestamp (the
/// pending-approval/question rows' `created_at` spelling).
fn ms_to_iso(ms: u64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms as i64)
        .unwrap_or_default()
        .to_rfc3339()
}

// ===========================================================================
// session.rename / session.updateMeta
// ===========================================================================

/// `session.rename` — set a session's display title.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionIdCamelParams {
    #[serde(rename = "sessionId", alias = "session_id")]
    session_id: String,
}

/// `session.status` — a session's run-state snapshot: whether an engine turn
/// is in flight, how many agents it has, and the first agent's model /
/// permission mode plus the session's plan-mode flag.
///
/// `running` mirrors the fire-mode turns `message.send` is currently tracking
/// in the shared in-flight registry; wait-mode turns are answered inline and
/// never appear as in-flight; unioned with the engine-side in-flight registry
/// so driver-injected turns (parked-notification reactivations, cron) count
/// as running too.
pub struct StatusSessionHandler {
    routing: Arc<dyn SessionRouting>,
    in_flight_turns: InFlightTurns,
    /// The engine-side in-flight turn registry (driver-injected turns the
    /// transport never tracked) — unioned into the running flag.
    engine: Arc<dyn mirri_transport::adapter::EngineAdapter>,
    /// The shared server config snapshot the model context-window read
    /// resolves through (hot-reload visible through the shared Arc).
    config: Arc<RwLock<ServerConfig>>,
}

impl StatusSessionHandler {
    pub fn new(
        manager: Arc<SessionManager>,
        in_flight_turns: InFlightTurns,
        engine: Arc<dyn mirri_transport::adapter::EngineAdapter>,
    ) -> Self {
        Self {
            routing: Arc::new(SingleManagerRouting::live(manager)),
            in_flight_turns,
            engine,
            // The pre-`with_config` placeholder: no models declared, so the
            // context window reads as unknown (0) until the composition
            // root attaches the real shared config.
            config: Arc::new(RwLock::new(ServerConfig::default())),
        }
    }

    /// Route session resolutions through an explicit [`SessionRouting`]
    /// implementation (the composition root's registry-backed routing).
    pub fn with_routing(mut self, routing: Arc<dyn SessionRouting>) -> Self {
        self.routing = routing;
        self
    }

    /// Read the model context window from the given shared config snapshot
    /// (the same source of truth `config.get` / `model.list` serve).
    pub fn with_config(mut self, config: Arc<RwLock<ServerConfig>>) -> Self {
        self.config = config;
        self
    }
}

/// `session.status` result: the run-state snapshot. `model`/`permission`
/// are `null` when the session has no agent yet; `thinkingEffort` is the
/// first agent's configured effort (the rust spellings `none`/`low`/
/// `medium`/`high` plus any model-declared effort string carried verbatim
/// (e.g. `max`, `xhigh`) — `"none"` when unset); `swarmMode` is the session-level
/// swarm flag; `collaborationMode` is the session-level collaboration-mode
/// flag (the `collaboration.enter` / `collaboration.exit` mode gate);
/// `contextTokens` is the first agent's estimated context size
/// (provider-reported base + pending-message estimate); `maxContextTokens`
/// is the model's context window resolved from the server config's model
/// catalog (0 = unknown model / no window configured).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionStatusResult {
    session_id: String,
    running: bool,
    agent_count: usize,
    model: Option<String>,
    permission: Option<Value>,
    plan_mode: bool,
    thinking_effort: String,
    swarm_mode: bool,
    collaboration_mode: bool,
    context_tokens: u64,
    max_context_tokens: u64,
}

#[async_trait]
impl RpcHandler for StatusSessionHandler {
    fn method(&self) -> &'static str {
        "session.status"
    }

    async fn handle(&self, params: Value) -> RpcResult<Value> {
        let params: SessionIdCamelParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| RpcError::InvalidParams(e.to_string()))?;

        let session = live_or_restored_session(self.routing.as_ref(), &params.session_id)
            .await
            .ok_or_else(|| RpcError::NotFound(params.session_id.clone()))?;

        let session_prefix = format!("{}:", params.session_id);
        let running = {
            let turns = self.in_flight_turns.lock().await;
            turns.keys().any(|key| key.starts_with(&session_prefix))
        } || self
            .engine
            .in_flight_sessions()
            .await
            .contains(&params.session_id);

        let agent_ids = session.list_agents().await;
        let agent_count = agent_ids.len();
        let first_agent = match agent_ids.first() {
            Some(agent_id) => session.get_agent(agent_id).await,
            None => None,
        };
        let model = first_agent.as_ref().map(|agent| {
            agent
                .config()
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .model_id
                .clone()
        });
        let permission = first_agent
            .as_ref()
            .and_then(|agent| serde_json::to_value(agent.permission_manager().mode()).ok());
        // The thinking effort in the `agent.getEffectiveThinkingLevel` wire
        // spelling (`None` → `"none"`).
        let thinking_effort = first_agent
            .as_ref()
            .map(|agent| {
                let config = agent.config();
                let config = config.read().unwrap_or_else(|e| e.into_inner());
                match config.thinking_effort.clone() {
                    Some(effort) => serde_json::to_value(effort)
                        .unwrap_or_else(|_| json!("none"))
                        .as_str()
                        .unwrap_or("none")
                        .to_string(),
                    None => "none".to_string(),
                }
            })
            .unwrap_or_else(|| "none".to_string());
        let context_tokens = first_agent
            .as_ref()
            .map(|agent| {
                let memory = agent.context_memory();
                let memory = memory.lock().unwrap_or_else(|e| e.into_inner());
                memory.token_count_with_pending()
            })
            .unwrap_or(0);
        // The model's context window, resolved from the shared config's
        // model catalog (alias id or the `${provider}/${model}` composite
        // the pickers send). 0 = unknown model — the wire spelling clients
        // already render as "no ceiling wired".
        let max_context_tokens = model
            .as_deref()
            .map(|model_id| {
                let config = self.config.read().unwrap_or_else(|e| e.into_inner());
                max_context_tokens_for(&config, model_id)
            })
            .unwrap_or(0);

        to_json(SessionStatusResult {
            session_id: params.session_id,
            running,
            agent_count,
            model,
            permission,
            plan_mode: session.plan().is_active(),
            thinking_effort,
            swarm_mode: session.swarm_mode().is_active(),
            collaboration_mode: session.collaboration_mode().is_active(),
            context_tokens,
            max_context_tokens,
        })
    }
}
pub struct ChildrenSessionHandler {
    routing: Arc<dyn SessionRouting>,
}

impl ChildrenSessionHandler {
    pub fn new(manager: Arc<SessionManager>) -> Self {
        Self {
            routing: Arc::new(SingleManagerRouting::live(manager)),
        }
    }

    /// Route session resolutions through an explicit [`SessionRouting`]
    /// implementation (the composition root's registry-backed routing).
    pub fn with_routing(mut self, routing: Arc<dyn SessionRouting>) -> Self {
        self.routing = routing;
        self
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChildrenSessionParams {
    #[serde(rename = "sessionId", alias = "session_id")]
    session_id: String,
    // Accepted for protocol-shape validation; the engine has no child
    // sessions, so the pagination/filter fields are never read after parsing.
    #[serde(rename = "beforeId", default)]
    _before_id: Option<String>,
    #[serde(rename = "afterId", default)]
    _after_id: Option<String>,
    #[serde(rename = "pageSize", default)]
    _page_size: Option<usize>,
    #[serde(default)]
    _busy: Option<bool>,
    #[serde(rename = "includeArchive", default)]
    _include_archive: Option<bool>,
}

/// `session.children` result: one empty page of the child-session list
/// (current wire shape — `items` + `hasMore`; the engine has no
/// child-session model, so the list is always empty).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ChildrenSessionsResult {
    items: Vec<SessionView>,
    has_more: bool,
}

#[async_trait]
impl RpcHandler for ChildrenSessionHandler {
    fn method(&self) -> &'static str {
        "session.children"
    }

    async fn handle(&self, params: Value) -> RpcResult<Value> {
        let params: ChildrenSessionParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| RpcError::InvalidParams(e.to_string()))?;

        live_or_restored_session(self.routing.as_ref(), &params.session_id)
            .await
            .ok_or_else(|| RpcError::NotFound(params.session_id.clone()))?;

        to_json(ChildrenSessionsResult {
            items: Vec::new(),
            has_more: false,
        })
    }
}
pub struct SnapshotSessionHandler {
    manager: Arc<SessionManager>,
    routing: Arc<dyn SessionRouting>,
    event_channels: Arc<SessionEventChannelRegistry>,
    in_flight_turns: InFlightTurns,
    /// The engine-side in-flight turn registry (driver-injected turns the
    /// transport never tracked) — unioned into the view's busy flag.
    engine: Arc<dyn mirri_transport::adapter::EngineAdapter>,
    /// Epoch stamped on records of channels this handler creates — the same
    /// server id the WS subscribe path uses, so cursor replay stays
    /// consistent whether the channel was first created by a subscribe or by
    /// a snapshot.
    epoch: Arc<str>,
}

impl SnapshotSessionHandler {
    pub fn new(
        manager: Arc<SessionManager>,
        event_channels: Arc<SessionEventChannelRegistry>,
        in_flight_turns: InFlightTurns,
        epoch: Arc<str>,
        engine: Arc<dyn mirri_transport::adapter::EngineAdapter>,
    ) -> Self {
        Self {
            routing: Arc::new(SingleManagerRouting::live(Arc::clone(&manager))),
            manager,
            event_channels,
            in_flight_turns,
            engine,
            epoch,
        }
    }

    /// Route session resolutions through an explicit [`SessionRouting`]
    /// implementation (the composition root's registry-backed routing).
    pub fn with_routing(mut self, routing: Arc<dyn SessionRouting>) -> Self {
        self.routing = routing;
        self
    }

    /// Attach the engine restore hook used to resume persisted-only
    /// sessions on demand (see [`EngineRestoreHook`]) — the single-manager
    /// routing's restore leg.
    pub fn with_restore_hook(mut self, restore_hook: Option<EngineRestoreHook>) -> Self {
        if let Some(hook) = restore_hook {
            self.routing = Arc::new(SingleManagerRouting {
                manager: Arc::clone(&self.manager),
                restore_hook: Some(hook),
            });
        }
        self
    }
}

/// `session.snapshot` result — the run-state snapshot bound to a
/// durable-event watermark (current wire shape: snake_case envelope keys,
/// camelCase keys inside the nested `session` view).
#[derive(Debug, Serialize)]
struct SessionSnapshotResult {
    as_of_seq: u64,
    epoch: String,
    session: SnapshotSessionView,
    messages: SnapshotMessagesPage,
    /// The main agent's in-flight turn (volatile stream state) — `null`
    /// when no turn is running. Read from the session artifacts' shadow
    /// accumulator, so a mid-turn reconnect rebuilds the half-finished
    /// reply and its running tools.
    in_flight_turn: Option<Value>,
    /// No subagent snapshot roster yet — always `[]` (the conversation-
    /// rendering domain tracks this as gap G3).
    subagents: Vec<Value>,
    /// Pending approval requests, from the session's approval gate (the
    /// gate is the pending source of truth).
    pending_approvals: Vec<Value>,
    /// Pending questions, from the session's question gate.
    pending_questions: Vec<Value>,
    /// Per-turn usage of completed turns (rerender parity for the usage
    /// pill) — `[]` when the wire log is unavailable.
    turn_usages: Vec<Value>,
}

/// The session sub-view inside a snapshot (camelCase keys).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotSessionView {
    id: String,
    /// Snapshots do not carry the title yet — always `null`.
    title: Option<String>,
    busy: bool,
    agent_count: usize,
    model: Option<String>,
    permission: Option<Value>,
    message_count: usize,
    /// The session's context root — the same derivation the `session.list`
    /// rows use (W4/W5): the session's own `work_dir` (backfilled at create
    /// from the owning workspace manager's cwd), else the owning manager's
    /// workspace cwd. Clients merge the snapshot view onto richer list-row
    /// records, so a view that omits `cwd` would wipe it and the session
    /// would fall out of its sidebar workspace group.
    #[serde(skip_serializing_if = "Option::is_none")]
    cwd: Option<String>,
    /// The work-facts tuple (US4.4/SDR-013) — the same shape the
    /// `session.list` rows carry: the main conversation's turn is in
    /// flight (the sidebar "running" indicator's input), the pending
    /// human-interaction kind, and the latest turn outcome. A snapshot is
    /// the page-refresh resync seed, so without these the refreshed
    /// sidebar waits for the first live work_changed frame instead of
    /// restoring the indicator immediately.
    main_turn_active: bool,
    pending_interaction: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_turn_reason: Option<String>,
    /// The session's aggregated token usage (four buckets + the first-call
    /// cold-start baseline, SDR-002/SDR-004) — the same shape the
    /// `session.list` rows carry. A snapshot is the page-refresh /
    /// resync-rebuild seed for the usage bar, so without it the bar waits
    /// for the first live status frame and flashes in late.
    usage: SessionUsageView,
}

/// Map the activity fold's pending-interaction kind onto the wire string
/// ("none"/"approval"/"question") — the same vocabulary `session.list`
/// rows and `event.session.work_changed` payloads carry (US4.4/SDR-013).
fn pending_kind(kind: &mirri_core::SessionPendingInteraction) -> String {
    match kind {
        mirri_core::SessionPendingInteraction::None => "none".to_string(),
        mirri_core::SessionPendingInteraction::Approval => "approval".to_string(),
        mirri_core::SessionPendingInteraction::Question => "question".to_string(),
    }
}

/// Map the activity fold's turn outcome onto the wire string
/// ("completed"/"cancelled"/"failed"/"stalled" — ADR-004) — the same
/// vocabulary `session.list` rows and `event.session.work_changed`
/// payloads carry.
fn outcome_str(outcome: mirri_core::SessionTurnOutcome) -> String {
    match outcome {
        mirri_core::SessionTurnOutcome::Completed => "completed".to_string(),
        mirri_core::SessionTurnOutcome::Cancelled => "cancelled".to_string(),
        mirri_core::SessionTurnOutcome::Failed => "failed".to_string(),
        mirri_core::SessionTurnOutcome::Stalled => "stalled".to_string(),
    }
}

/// The un-paginated message page inside a snapshot (snake_case keys).
#[derive(Debug, Serialize)]
struct SnapshotMessagesPage {
    items: Vec<Value>,
    has_more: bool,
}

/// Map an engine [`TaskStatus`](mirri_core::task::TaskStatus) onto the
/// protocol's task status vocabulary.
fn task_status_to_wire(status: &mirri_core::task::TaskStatus) -> &'static str {
    match status {
        mirri_core::task::TaskStatus::Running => "running",
        mirri_core::task::TaskStatus::Completed { .. } => "completed",
        mirri_core::task::TaskStatus::Failed(_) => "failed",
        mirri_core::task::TaskStatus::Stopped => "cancelled",
    }
}

/// Extract one attribute value from a `<subagent …>` opening tag.
fn swarm_attr_value(tag: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let start = tag.find(&needle)? + needle.len();
    let rest = &tag[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Per-member outcomes parsed out of `<agent_swarm_result>` payloads in the
/// main history (tool results) — the honest terminal source for swarm members
/// that have no task-store record (swarm members never register a task; the
/// batch runs through the scheduler, not the Agent tool's registration paths).
fn swarm_outcomes_from_history(
    history: &[mirri_core::context::ContextMessage],
) -> HashMap<String, String> {
    let mut outcomes: HashMap<String, String> = HashMap::new();
    for message in history {
        let joined = message
            .message
            .content
            .iter()
            .filter_map(|part| match part {
                mirri_core::ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        if !joined.contains("<agent_swarm_result>") {
            continue;
        }
        let mut rest = joined.as_str();
        while let Some(start) = rest.find("<subagent ") {
            let after = &rest[start..];
            let Some(tag_end) = after.find('>') else {
                break;
            };
            let tag = &after[..tag_end];
            if let (Some(agent_id), Some(outcome)) = (
                swarm_attr_value(tag, "agent_id"),
                swarm_attr_value(tag, "outcome"),
            ) {
                outcomes.insert(agent_id, outcome);
            }
            rest = &after[tag_end..];
        }
    }
    outcomes
}

#[async_trait]
impl RpcHandler for SnapshotSessionHandler {
    fn method(&self) -> &'static str {
        "session.snapshot"
    }

    async fn handle(&self, params: Value) -> RpcResult<Value> {
        let params: SessionIdCamelParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| RpcError::InvalidParams(e.to_string()))?;

        let session = live_or_restored_session(self.routing.as_ref(), &params.session_id)
            .await
            .ok_or_else(|| RpcError::NotFound(params.session_id.clone()))?;

        // The channel is the same per-session durable log the WS layer
        // subscribes to, so `as_of_seq` advances with the client's event
        // stream. Get-or-create with this server's epoch (matching the WS
        // subscribe path); an existing channel keeps its own epoch either
        // way.
        let artifacts = self
            .routing
            .session_artifacts(&params.session_id)
            .ok_or_else(|| RpcError::NotFound(params.session_id.clone()))?;
        let channel = self.event_channels.get_or_create(
            &params.session_id,
            Arc::clone(&artifacts.event_bus),
            Arc::clone(&self.epoch),
        );
        let as_of_seq = channel.tail_seq();
        let epoch = channel.epoch().to_string();

        // Busy mirrors the fire-mode turns `message.send` tracks (same
        // derivation as `session.status`), unioned with the engine-side
        // registry so driver-injected turns count too.
        let session_prefix = format!("{}:", params.session_id);
        let busy = {
            let turns = self.in_flight_turns.lock().await;
            turns.keys().any(|key| key.starts_with(&session_prefix))
        } || self
            .engine
            .in_flight_sessions()
            .await
            .contains(&params.session_id);

        // The work-facts tuple (US4.4/SDR-013): the activity shadow's
        // fold on the same artifacts set — the same authority the
        // `session.list` rows read, so the snapshot and the list never
        // drift on what the sidebar shows. `busy` unions the shadow's
        // aggregate with the in-flight registries above (the shadow folds
        // bus events; the registries track dispatch-level turns the
        // fold may not have consumed yet — either source lighting means
        // work is in flight).
        let activity = artifacts.activity_shadow.snapshot_value();
        let busy = activity.busy || busy;
        let main_turn_active = activity.main_turn_active;
        let pending_interaction = pending_kind(&activity.pending_interaction);
        let last_turn_reason = activity.last_turn_reason.map(outcome_str);

        // The volatile in-flight turn and the pending interaction requests
        // come from the same artifacts set the channel was built on — one
        // consistent per-session state. Pending rows follow the TS
        // `rest-snapshot` approval/question request shapes (snake_case),
        // with the two B9 fields in the klient camelCase spelling the
        // `approval.listPending` / interaction-kernel rows use:
        // `approvalRule` rides the row only when the snapshot carries the
        // facet (absent ≠ empty), and `expiresAt` is honest — the engine
        // gate derives no expiry, so the field is an explicit `null` (never
        // a fabricated timestamp; a real value rides the snapshot once the
        // engine gains one).
        let in_flight_turn = artifacts.in_flight_shadow.snapshot_value();
        // Supervision B1 (US5.3): resolve the pending approvals' requesting
        // member against the live roster first — a row carries `agent_id`
        // only when the gate snapshot's agent uuid names a registered member
        // agent (the `agent-<uuid>` namespace the host mints at spawn). The
        // main agent and unresolvable requesters carry no identity on the
        // row, so single-agent sessions stay byte-for-byte unchanged.
        let agent_ids = session.list_agents().await;
        let member_ids_by_uuid: std::collections::HashMap<uuid::Uuid, String> = agent_ids
            .iter()
            .filter_map(|id| {
                id.as_str()
                    .strip_prefix("agent-")
                    .and_then(|tail| tail.parse::<uuid::Uuid>().ok())
                    .map(|uuid| (uuid, id.as_str().to_string()))
            })
            .collect();
        let pending_approvals = artifacts
            .approval_gate
            .list_pending()
            .into_iter()
            .map(|(request_id, snapshot)| {
                let mut row = json!({
                    "approval_id": request_id.to_string(),
                    "session_id": params.session_id,
                    "tool_name": snapshot.tool_name,
                    "action": snapshot.action,
                    "tool_input_display": snapshot.display,
                    "created_at": ms_to_iso(snapshot.created_at),
                    "expiresAt": Value::Null,
                });
                if let Some(turn_id) = snapshot.turn_id {
                    row["turn_id"] = json!(turn_id);
                }
                if let Some(tool_call_id) = snapshot.tool_call_id {
                    row["tool_call_id"] = json!(tool_call_id);
                }
                // The engine approval-rule facet the call was gated on
                // (B9) — the pattern an "always allow" upgrade or a
                // session-scoped approval memorizes. Emitted only when the
                // tool reported one (the klient field is optional).
                if let Some(approval_rule) = &snapshot.approval_rule {
                    row["approvalRule"] = json!(approval_rule);
                }
                // The requesting member's identity (B1) — the full agent id,
                // the same key the subagents roster and the WS subagent rows
                // use, so the client correlates the badge without a second
                // mapping. Emitted only for member-originated approvals.
                if let Some(member_id) = member_ids_by_uuid.get(&snapshot.agent_id) {
                    row["agent_id"] = json!(member_id);
                }
                row
            })
            .collect();
        let pending_questions = artifacts
            .question_gate
            .list_pending()
            .into_iter()
            .map(|(request_id, snapshot)| {
                let mut row = json!({
                    "question_id": request_id.to_string(),
                    "session_id": params.session_id,
                    // The v2 REST contract (`questionRequestSchema`) requires
                    // item and option ids; the gate transparently forwards the
                    // AskUserQuestion tool input, which has none — mint them
                    // here with the same scheme the TS v2 REST edge uses
                    // (`q_<idx>` / `opt_<item>_<option>`), so a snapshot-
                    // restored card answers unambiguously.
                    "questions": mint_question_ids(&snapshot.questions),
                    "created_at": ms_to_iso(snapshot.created_at),
                });
                if let Some(turn_id) = snapshot.turn_id {
                    row["turn_id"] = json!(turn_id);
                }
                if let Some(tool_call_id) = snapshot.tool_call_id {
                    row["tool_call_id"] = json!(tool_call_id);
                }
                row
            })
            .collect();

        // (`agent_ids` was fetched above the pending-approval rows so the
        // member-identity resolution shares one consistent roster read.)
        let agent_count = agent_ids.len();
        let first_agent = match agent_ids.first() {
            Some(agent_id) => session.get_agent(agent_id).await,
            None => None,
        };
        // The usage view seed (SDR-002/SDR-004) — same derivation the
        // `session.list` rows use (`build_session_view`), so the snapshot
        // and the list never drift on what the usage bar shows.
        let extras = session.runtime_extras().await;
        // The session's own workspace root (W4 parity with the `session.list`
        // rows): `work_dir` is backfilled at create from the owning (routed)
        // manager's cwd, so a session of a non-default workspace reports its
        // own root — never the process default root. The fallbacks keep a
        // value for sessions whose metadata predates the field.
        let cwd = session
            .metadata()
            .await
            .work_dir
            .or_else(|| {
                session
                    .workspace_root()
                    .map(|root| root.to_string_lossy().into_owned())
            })
            .unwrap_or_else(|| {
                self.routing
                    .owning_manager(&params.session_id)
                    .unwrap_or_else(|| Arc::clone(&self.manager))
                    .workspace()
                    .cwd()
                    .to_string_lossy()
                    .into_owned()
            });
        let title = session.metadata().await.title;
        let model = first_agent.as_ref().map(|agent| {
            agent
                .config()
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .model_id
                .clone()
        });
        let permission = first_agent
            .as_ref()
            .and_then(|agent| serde_json::to_value(agent.permission_manager().mode()).ok());

        // The full message history of the first agent (snapshots are
        // un-paginated), serialized like `message.list` entries (the shared
        // row builder — the two read faces must not drift apart). The fold
        // shares one computation between the envelope's `turn_usages` and
        // the rows' `promptId`/`durationMs` stamps; a session without an
        // agent has no history, so its fold is empty too.
        let (messages, turn_usages, open_turn_key): (Vec<Value>, Vec<Value>, Option<String>) =
            match &first_agent {
                Some(agent) => {
                    let agent_id = agent.id().as_str().to_string();
                    let history = agent
                        .context_memory()
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .history()
                        .to_vec();
                    let base_created_at = session.metadata().await.created_at;
                    let (turn_usages, prompt_ids, open_turn_key, duration_rows) =
                        turn_usage_read_face(
                            self.routing.owning_manager(&params.session_id).as_deref(),
                            &params.session_id,
                            &history,
                        )
                        .await;
                    let messages = history
                        .iter()
                        .enumerate()
                        .map(|(position, message)| {
                            let row =
                                message_list_row(message, &agent_id, base_created_at, position);
                            let row = stamp_row_prompt_id(row, message, &prompt_ids);
                            stamp_row_duration_ms(row, message, &duration_rows)
                        })
                        .collect();
                    (messages, turn_usages, open_turn_key)
                }
                None => (Vec::new(), Vec::new(), None),
            };
        let message_count = messages.len();

        // `current_prompt_id` (TS parity): the fold's still-open tail bracket
        // IS the in-flight turn the shadow reports — stamp its key onto the
        // volatile snapshot so the client's in-flight seed and the transcript
        // rows (attributed through the same tail opener) share one turn
        // identity. Without it the client mints a fresh id, the two copies of
        // the running step split into separate groups, and the running Task
        // card renders twice after a mid-turn reload.
        //
        // `current_turn_started_at` (epoch-ms) rides the same open bracket:
        // the turn's REAL start time from the wire log, so a refreshed
        // client's live "Working for X" header ticks from the server's anchor
        // instead of restarting at zero or guessing from its own clock. Best
        // effort — a log without the open turn (or without a `time` stamp)
        // simply leaves the field off, and the client falls back to its bare
        // "Working…" label.
        let open_turn_started_at = match self.routing.owning_manager(&params.session_id) {
            Some(manager) => {
                manager
                    .read_only_open_turn_started_at(&SessionId::new(&params.session_id))
                    .await
            }
            None => None,
        };
        let in_flight_turn = match (in_flight_turn, open_turn_key) {
            (Some(mut value), Some(key)) => {
                if let Some(obj) = value.as_object_mut() {
                    obj.insert("current_prompt_id".to_string(), json!(key));
                    if let Some(started_at) = open_turn_started_at {
                        obj.insert("current_turn_started_at".to_string(), json!(started_at));
                    }
                }
                Some(value)
            }
            (value, _) => value,
        };

        // Member roster (supervision §4.2, US4.1): the session's registered
        // subagent identities joined with honest statuses. Identity comes
        // from the live subagent registry (`subagent.registered` ops,
        // rebuilt on restore — BTW side-chat forks never record one, so they
        // never appear); status comes from the task store when an Agent-kind
        // task exists (single dispatches register one), else from the
        // `<agent_swarm_result>` payload in the main history (swarm members),
        // else the honest "stopped" — registered, settled, outcome not
        // derivable from this session's state. Never a fabricated "running".
        let mut subagents: Vec<Value> = Vec::new();
        let registrations = session.list_subagent_registrations().await;
        if !registrations.is_empty() {
            let mut task_by_agent: HashMap<String, mirri_core::task::AgentTask> = HashMap::new();
            for task in session.tasks().list() {
                if let mirri_core::task::TaskKind::Agent {
                    agent_id: Some(id), ..
                } = &task.kind
                {
                    task_by_agent.insert(id.clone(), task);
                }
            }
            let swarm_outcomes = match &first_agent {
                Some(agent) => {
                    let memory = agent.context_memory();
                    let memory = memory.lock().unwrap_or_else(|e| e.into_inner());
                    swarm_outcomes_from_history(memory.history())
                }
                None => HashMap::new(),
            };
            for registration in &registrations {
                let task = task_by_agent.get(&registration.agent_id);
                let status = task
                    .map(|t| task_status_to_wire(&t.status))
                    .or_else(|| {
                        swarm_outcomes.get(&registration.agent_id).map(|outcome| {
                            if outcome == "completed" {
                                "completed"
                            } else {
                                "failed"
                            }
                        })
                    })
                    .unwrap_or("cancelled");
                let phase = match status {
                    "running" => "working",
                    "completed" => "completed",
                    _ => "failed",
                };
                let mut row = json!({
                    "id": registration.agent_id,
                    "session_id": params.session_id,
                    "kind": "subagent",
                    "description": task
                        .map(|t| t.description.clone())
                        .or_else(|| registration.swarm_item.clone())
                        .unwrap_or_else(|| registration.agent_id.clone()),
                    "status": status,
                    "created_at": task
                        .map(|t| t.created_at.to_rfc3339())
                        .unwrap_or_default(),
                    "subagent_phase": phase,
                    "subagent_type": registration.profile_name,
                    "agent_id": registration.agent_id,
                });
                if let Some(parent) = &registration.parent_tool_call_id {
                    row["parent_tool_call_id"] = json!(parent);
                }
                if let Some(index) = registration.swarm_index {
                    row["swarm_index"] = json!(index);
                }
                if let Some(model) = &registration.model {
                    row["model"] = json!(model);
                }
                if let Some(task) = task {
                    if !task.output.is_empty() {
                        row["output_preview"] = json!(task.output);
                    }
                    row["run_in_background"] = json!(task.detached);
                }
                subagents.push(row);
            }
        }

        to_json(SessionSnapshotResult {
            as_of_seq,
            epoch,
            session: SnapshotSessionView {
                id: params.session_id,
                title: Some(title),
                busy,
                agent_count,
                model,
                permission,
                message_count,
                cwd: Some(cwd),
                main_turn_active,
                pending_interaction,
                last_turn_reason,
                usage: SessionUsageView {
                    first_call: extras.first_call.as_ref().map(SessionFirstCallView::from),
                    ..SessionUsageView::from(extras.usage)
                },
            },
            messages: SnapshotMessagesPage {
                items: messages,
                has_more: false,
            },
            in_flight_turn,
            subagents,
            pending_approvals,
            pending_questions,
            turn_usages,
        })
    }
}
fn default_message_page_size() -> usize {
    50
}

/// Upper bound on `message.list` page sizes, per the protocol spec.
const MAX_MESSAGE_PAGE_SIZE: usize = 200;

/// `message.list` — page through an agent's context-memory history.
///
/// Messages are identified by `msg-<entry_seq>` (the session-cumulative
/// entry number, stable across restarts and within a data state — see
/// [`message_list_row`]). `beforeId` pages toward older history, `afterId`
/// toward newer; the two are mutually exclusive. The first agent of the
/// session is addressed (the protocol's default agent); a session without
/// agents yields an empty page.
pub struct ListMessagesHandler {
    routing: Arc<dyn SessionRouting>,
}

impl ListMessagesHandler {
    pub fn new(manager: Arc<SessionManager>) -> Self {
        Self {
            routing: Arc::new(SingleManagerRouting::live(manager)),
        }
    }

    /// Route session resolutions through an explicit [`SessionRouting`]
    /// implementation (the composition root's registry-backed routing).
    pub fn with_routing(mut self, routing: Arc<dyn SessionRouting>) -> Self {
        self.routing = routing;
        self
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListMessagesParams {
    #[serde(rename = "sessionId", alias = "session_id")]
    session_id: String,
    /// Optional member agent to address (e.g. a subagent). `None` keeps the
    /// protocol default — the session's first agent (main). The member-scoped
    /// read face for supervision (architecture §4.2).
    #[serde(rename = "agentId", alias = "agent_id", default)]
    agent_id: Option<String>,
    #[serde(rename = "beforeId", default)]
    before_id: Option<String>,
    #[serde(rename = "afterId", default)]
    after_id: Option<String>,
    #[serde(rename = "pageSize", default = "default_message_page_size")]
    page_size: usize,
}

/// `message.list` result: one page of the session's history
/// (`rust:{messages,hasMore}` — the TSV-authoritative shape, unchanged).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ListMessagesResult {
    messages: Vec<Value>,
    has_more: bool,
}

/// One history entry row of the session's message read faces — the **shared
/// row builder** both `message.list` and `session.snapshot` serialize through
/// (the two read faces must not drift apart). Keys are camelCase; `content`
/// is projected onto the TS protocol content-part shapes (see
/// [`message_content_to_protocol_json`]) — plain-text rows still collapse to
/// a string, everything structured becomes an array of protocol parts.
///
/// The row `id` is `msg-<entry_seq>` — anchored to the session-cumulative
/// entry number (ADR-007 numbering), not the array position. Entry numbers
/// survive compaction and undo (survivors keep theirs, removed ones are
/// never recycled), and a resume re-derives the same numbers at the same
/// materialization points, so the id is stable across restarts and stable
/// within a data state — a page cursor can rely on it. `createdAt` is a
/// derived, order-faithful timestamp (base = the session's creation time +
/// position, the TS read-face precedent): the engine records no per-message
/// wall clock.
fn message_list_row(
    message: &mirri_core::context::ContextMessage,
    agent_id: &str,
    base_created_at: chrono::DateTime<chrono::Utc>,
    position: usize,
) -> Value {
    let mut row = json!({
        "id": format!("msg-{}", message.entry_seq),
        "role": serde_json::to_value(&message.message.role).unwrap_or(Value::Null),
        "content": message_content_to_protocol_json(message),
        "agentId": agent_id,
        "createdAt": (base_created_at + chrono::Duration::milliseconds(position as i64))
            .to_rfc3339(),
    });
    // TS toProtocolMessage parity: origin rides under `metadata.origin` so
    // clients can filter system-injected turns out of the transcript.
    if let Some(origin) = &message.origin {
        row["metadata"] = json!({ "origin": origin });
    }
    // TS toProtocolMessage parity: skills attached to a rewritten user
    // message ride as a TOP-LEVEL wire array (ADR-006), never under
    // `metadata` — the web's `MessageRow.with_skills` contract mirrors the
    // TS projector exactly.
    if let Some(with_skills) = &message.with_skills
        && !with_skills.is_empty()
    {
        row["with_skills"] =
            serde_json::to_value(with_skills).unwrap_or(Value::Null);
    }
    row
}

/// Mint the item/option ids the v2 REST contract (`questionRequestSchema`)
/// requires onto the raw question items a `QuestionSnapshot` carries (the
/// AskUserQuestion tool input — ids absent by construction). Existing ids
/// pass through untouched; non-object items and non-array inputs are
/// forwarded as-is (honest pass-through, never a panic).
///
/// The option id is the option's LABEL, mirroring the live `question.ask`
/// bridge (`questionRequestFromAskParams` mints the same way): the rust
/// `question.answer` handler resolves the gate with the answer string
/// as-is (no id→label translation layer like the TS edge's
/// `toInProcessResponse`), so an id-minted label is the only form that
/// keeps the model-facing tool result readable when a snapshot-restored
/// card is answered. Items mint `q_<idx>` (the answer map key — never fed
/// to the model as text); an empty label falls back to `opt_<item>_<opt>`.
fn mint_question_ids(questions: &Value) -> Value {
    let Some(items) = questions.as_array() else {
        return questions.clone();
    };
    Value::Array(
        items
            .iter()
            .enumerate()
            .map(|(item_idx, item)| {
                let Some(obj) = item.as_object() else {
                    return item.clone();
                };
                let mut out = obj.clone();
                if out
                    .get("id")
                    .is_none_or(|id| id.as_str().is_none_or(|s| s.is_empty()))
                {
                    out.insert("id".to_string(), json!(format!("q_{item_idx}")));
                }
                if let Some(options) = out.get("options").and_then(Value::as_array) {
                    let minted: Vec<Value> = options
                        .iter()
                        .enumerate()
                        .map(|(opt_idx, option)| {
                            let Some(option_obj) = option.as_object() else {
                                return option.clone();
                            };
                            let mut option_out = option_obj.clone();
                            if option_out
                                .get("id")
                                .is_none_or(|id| id.as_str().is_none_or(|s| s.is_empty()))
                            {
                                let label = option_out
                                    .get("label")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default();
                                let id = if label.is_empty() {
                                    format!("opt_{item_idx}_{opt_idx}")
                                } else {
                                    label.to_string()
                                };
                                option_out.insert("id".to_string(), json!(id));
                            }
                            Value::Object(option_out)
                        })
                        .collect();
                    out.insert("options".to_string(), Value::Array(minted));
                }
                Value::Object(out)
            })
            .collect(),
    )
}

/// The per-turn usage read face shared by `session.snapshot` and
/// `message.list`: fold the session's wire log (read-only) into per-turn
/// usage records and attribute assistant history rows to their turn's
/// prompt id — plus the `entry_seq → durationMs` map for the row stamps
/// (last assistant row of each duration-carrying turn). Also surfaces the
/// still-open tail bracket's turn key (the in-flight turn) so
/// `session.snapshot` can stamp it as `current_prompt_id`. Degrades to
/// empty when no manager or log is available — the pill simply stays
/// hidden, exactly as before this read face.
async fn turn_usage_read_face(
    manager: Option<&SessionManager>,
    session_id: &str,
    history: &[mirri_core::context::ContextMessage],
) -> (
    Vec<Value>,
    HashMap<u64, String>,
    Option<String>,
    HashMap<u64, u64>,
) {
    let Some(manager) = manager else {
        return (Vec::new(), HashMap::new(), None, HashMap::new());
    };
    let fold = manager
        .read_only_turn_usages(&SessionId::new(session_id))
        .await
        .unwrap_or_default();
    // Attribution keyed by fold bracket identity (not the turn-key string,
    // which collides on legacy logs whose allocator restarted) — the string
    // prompt-id view rides along for the transcript's `promptId` stamps.
    let assignment =
        mirri_core::session::turn_usage::assign_turn_rows(&fold, history);
    let duration_rows =
        mirri_core::session::turn_usage::assign_turn_durations(&fold, &assignment);
    let turns = fold
        .turns
        .iter()
        .map(|record| {
            let mut turn = json!({
                "promptId": record.turn_key,
                "inputTokens": record.input_tokens,
                "outputTokens": record.output_tokens,
                "cacheReadTokens": record.cache_read_tokens,
                "cacheCreationTokens": record.cache_creation_tokens,
                "models": record.models,
            });
            if let Some(duration_ms) = record.duration_ms {
                turn["durationMs"] = json!(duration_ms);
            }
            turn
        })
        .collect();
    (turns, assignment.prompt_ids, fold.open_turn_key, duration_rows)
}

/// Stamp `promptId` on an assistant row when the fold attributed its
/// entry to a turn. Returns the row for chaining.
fn stamp_row_prompt_id(
    mut row: Value,
    message: &mirri_core::context::ContextMessage,
    prompt_ids: &HashMap<u64, String>,
) -> Value {
    if message.message.role == mirri_core::message::Role::Assistant
        && let Some(prompt_id) = prompt_ids.get(&message.entry_seq)
    {
        row["promptId"] = json!(prompt_id);
    }
    row
}

/// Stamp `durationMs` on the LAST assistant row of a completed turn when
/// the fold's `turn.ended` record carried a duration — the same row the
/// live face stamps, so a restored transcript renders the same
/// "Worked for X" pill the live transcript shows. Returns the row for
/// chaining.
fn stamp_row_duration_ms(
    mut row: Value,
    message: &mirri_core::context::ContextMessage,
    duration_rows: &HashMap<u64, u64>,
) -> Value {
    if message.message.role == mirri_core::message::Role::Assistant
        && let Some(duration_ms) = duration_rows.get(&message.entry_seq)
    {
        row["durationMs"] = json!(duration_ms);
    }
    row
}

/// Resolve a `msg-<entry_seq>` cursor id to its position in the history
/// slice. Compaction and undo renumber nothing (survivors keep their entry
/// numbers), so a position can always be re-found while the anchor message
/// is still in the history; an anchor that no longer resolves (the message
/// it pointed at is gone — compacted away, undone, or from an older data
/// state) yields `None`, and the callers return an empty terminal page
/// rather than re-serving the newest page (the TS read face's known
/// "re-serve from the top" trap).
fn resolve_message_position(
    history: &[mirri_core::context::ContextMessage],
    id: &str,
) -> Option<usize> {
    let seq = parse_message_seq(id).ok()? as u64;
    history.iter().position(|message| message.entry_seq == seq)
}

#[async_trait]
impl RpcHandler for ListMessagesHandler {
    fn method(&self) -> &'static str {
        "message.list"
    }

    async fn handle(&self, params: Value) -> RpcResult<Value> {
        let params: ListMessagesParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| RpcError::InvalidParams(e.to_string()))?;

        if params.before_id.is_some() && params.after_id.is_some() {
            return Err(RpcError::InvalidParams(
                "beforeId and afterId are mutually exclusive".to_string(),
            ));
        }
        if params.page_size == 0 {
            return Err(RpcError::InvalidParams(
                "pageSize must be at least 1".to_string(),
            ));
        }
        let page_size = params.page_size.min(MAX_MESSAGE_PAGE_SIZE);

        let session = live_or_restored_session(self.routing.as_ref(), &params.session_id)
            .await
            .ok_or_else(|| RpcError::NotFound(params.session_id.clone()))?;

        // A session without agents has no history to page through. A named
        // agent that does not resolve is NotFound — never silently fall back
        // to the main history (an addressed read must be honest).
        let Some(agent) = resolve_session_agent(&session, params.agent_id.as_deref()).await else {
            if params.agent_id.is_some() {
                return Err(RpcError::NotFound(
                    params.agent_id.clone().unwrap_or_default(),
                ));
            }
            return to_json(ListMessagesResult {
                messages: Vec::new(),
                has_more: false,
            });
        };
        let agent_id = agent.id().as_str().to_string();
        // The per-turn attribution stamps the rows (assistant-only) with
        // their turn's prompt id — `message.list` does not return the
        // usage envelope; that is `session.snapshot`'s share of the feature.
        // The fold is read first (its own await, holding no agent locks),
        // then one memory-lock scope borrows the history to compute the
        // attribution and slice the page window — a paginated read must
        // never deep-copy the whole history for a one-page answer.
        let fold = match self.routing.owning_manager(&params.session_id) {
            Some(manager) => manager
                .read_only_turn_usages(&SessionId::new(&params.session_id))
                .await
                .unwrap_or_default(),
            None => Default::default(),
        };
        let base_created_at = session.metadata().await.created_at;
        let (messages, has_more) = {
            let memory = agent.context_memory();
            let memory = memory.lock().unwrap_or_else(|e| e.into_inner());
            let history = memory.history();
            let assignment =
                mirri_core::session::turn_usage::assign_turn_rows(&fold, history);
            let duration_rows =
                mirri_core::session::turn_usage::assign_turn_durations(&fold, &assignment);
            let prompt_ids = assignment.prompt_ids;
            let history_len = history.len();

            // Compute the window. `beforeId`/`afterId` refer to the message
            // id (an entry-seq anchor) of a boundary message; the window
            // excludes that message. An anchor that no longer resolves is
            // an empty terminal page — never a re-serve of the newest page.
            let (start, end, has_more) = match (&params.before_id, &params.after_id) {
                (Some(before_id), None) => match resolve_message_position(history, before_id) {
                    Some(end) => {
                        let start = end.saturating_sub(page_size);
                        (start, end, start > 0)
                    }
                    // An unresolvable anchor is an empty terminal page —
                    // never a re-serve of the newest page.
                    None => (0, 0, false),
                },
                (None, Some(after_id)) => match resolve_message_position(history, after_id) {
                    Some(anchor) => {
                        let start = anchor.saturating_add(1).min(history_len);
                        let end = (start + page_size).min(history_len);
                        (start, end, end < history_len)
                    }
                    None => (0, 0, false),
                },
                (None, None) => {
                    let start = history_len.saturating_sub(page_size);
                    (start, history_len, history_len > page_size)
                }
                // Rejected upfront: beforeId and afterId are mutually
                // exclusive.
                (Some(_), Some(_)) => {
                    unreachable!("beforeId/afterId combination rejected above")
                }
            };

            let messages: Vec<Value> = history[start..end]
                .iter()
                .enumerate()
                .map(|(offset, message)| {
                    let row =
                        message_list_row(message, &agent_id, base_created_at, start + offset);
                    let row = stamp_row_prompt_id(row, message, &prompt_ids);
                    stamp_row_duration_ms(row, message, &duration_rows)
                })
                .collect();
            (messages, has_more)
        };
        to_json(ListMessagesResult { messages, has_more })
    }
}

// ===========================================================================
// message.abort
// ===========================================================================

/// `message.abort` — cancel in-flight fire-mode engine turns of a session.
///
/// Turn ids map to tasks through the shared in-flight registry
/// (`"{session_id}:{agent_id}:{turn_id}"`). A specific `turnId` cancels only
/// that turn (an unknown `turnId` is a [`RpcError::NotFound`]); omitting it
/// cancels every in-flight turn of the session. Cancelled turns publish a
/// [`SessionEvent::TurnFailed`] with an "aborted" message so subscribers
/// waiting for a turn to end can stop waiting.
///
/// Limitation: the engine turn itself runs on the blocking pool and cannot be
/// unwound mid-LLM-call; the abort drops the client-visible task and its
/// registration, while the blocking turn stays bounded by
/// [`ENGINE_TURN_TIMEOUT`].
pub struct CatalogToolsHandler {
    manager: Arc<SessionManager>,
}

impl CatalogToolsHandler {
    pub fn new(manager: Arc<SessionManager>) -> Self {
        Self { manager }
    }
}

#[async_trait]
impl RpcHandler for CatalogToolsHandler {
    fn method(&self) -> &'static str {
        "tool.catalog"
    }

    async fn handle(&self, _params: Value) -> RpcResult<Value> {
        let mut catalog: BTreeMap<String, ToolCatalogEntryView> = BTreeMap::new();
        // The static builtin rows come first: the catalog is the
        // session-independent directory configuration UIs (the profile
        // editor's tool picker) read, so it must answer with the full
        // builtin set even when no session is live (a fresh daemon, or
        // sessions that live in other workspaces / only on disk).
        for (name, description) in mirri_core::tools::BUILTIN_TOOL_CATALOG {
            catalog.insert(
                (*name).to_string(),
                ToolCatalogEntryView {
                    name: (*name).to_string(),
                    description: (*description).to_string(),
                    parameters: Value::Null,
                    deferred: false,
                    source: "builtin".to_string(),
                },
            );
        }
        // Live sessions then contribute what the static table cannot know —
        // MCP and user tools surfaced through an agent's loop table, with
        // their full parameter schemas.
        for info in self.manager.list_sessions().await {
            let Some(session) = self.manager.get_session(&SessionId::new(&info.id)).await else {
                continue;
            };
            for agent_id in session.list_agents().await {
                let Some(agent) = session.get_agent(&agent_id).await else {
                    continue;
                };
                let definitions = agent.tool_manager().read().loop_tools();
                for definition in definitions {
                    if !catalog.contains_key(&definition.name) {
                        catalog.insert(
                            definition.name.clone(),
                            ToolCatalogEntryView {
                                name: definition.name,
                                description: definition.description,
                                parameters: definition.parameters,
                                deferred: definition.deferred,
                                source: "builtin".to_string(),
                            },
                        );
                    }
                }
            }
        }
        to_json(ToolCatalogResult {
            tools: catalog.into_values().collect::<Vec<_>>(),
        })
    }
}

/// `tool.catalog` result: the workspace-level tool directory — the outer
/// `rust:{tools}` wrapper is the TSV-authoritative shape (RustFirst) and is
/// unchanged; only the entries are typed. (The Web client's bare-array
/// expectation was a Web bug, fixed with the batch-⑥ web sync.)
#[derive(Debug, Serialize)]
struct ToolCatalogResult {
    tools: Vec<ToolCatalogEntryView>,
}

/// One entry of the `tool.catalog` directory (current wire shape —
/// [`ToolSummaryView`] plus the workspace-level `source` attribution).
#[derive(Debug, Serialize)]
struct ToolCatalogEntryView {
    name: String,
    description: String,
    parameters: Value,
    deferred: bool,
    source: String,
}

// ===========================================================================
// skill.list
// ===========================================================================

/// `skill.list` — the skills discovered in the workspace's skill catalog.
///
/// Skills are workspace-scoped (one catalog per workspace), so the optional
/// `sessionId` is accepted for protocol compatibility but does not change the
/// result.
const MAX_FILE_READ_BYTES: u64 = 10 * 1024 * 1024;

/// Common params for `file.list` / `file.read`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FilePathParams {
    #[serde(rename = "sessionId")]
    session_id: String,
    path: String,
}

/// `file.list` — list the entries of a workspace directory.
///
/// The path is resolved against the session's file root (the session's own
/// workspace root when set, else the workspace cwd) and must stay inside it —
/// the same [`validate_workspace_path`] containment check `session.export`
/// uses. Entries are sorted by name; `kind` is `"dir"` or `"file"`.
///
/// The Web client also sends `depth` / `includeGitStatus` params, which the
/// server accepts-and-ignores (serde drops unknown fields by default) —
/// depth traversal and per-entry git status are future engine work. The
/// Web's optional `childrenByPath` field is likewise not emitted yet.
pub struct ListFilesHandler {
    manager: Arc<SessionManager>,
    routing: Arc<dyn SessionRouting>,
}

impl ListFilesHandler {
    pub fn new(manager: Arc<SessionManager>) -> Self {
        Self {
            routing: Arc::new(SingleManagerRouting::live(Arc::clone(&manager))),
            manager,
        }
    }

    /// Route session resolutions through an explicit [`SessionRouting`]
    /// implementation (the composition root's registry-backed routing).
    pub fn with_routing(mut self, routing: Arc<dyn SessionRouting>) -> Self {
        self.routing = routing;
        self
    }
}

#[async_trait]
impl RpcHandler for ListFilesHandler {
    fn method(&self) -> &'static str {
        "file.list"
    }

    async fn handle(&self, params: Value) -> RpcResult<Value> {
        let params: FilePathParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| RpcError::InvalidParams(e.to_string()))?;

        let session = live_or_restored_session(self.routing.as_ref(), &params.session_id)
            .await
            .ok_or_else(|| RpcError::NotFound(params.session_id.clone()))?;
        let root = session_file_root(&session, &self.manager);
        let requested = validate_workspace_path(&root, Path::new(&params.path))
            .map_err(|e| RpcError::InvalidParams(format!("path rejected: {e}")))?;
        if !requested.is_dir() {
            return Err(RpcError::NotFound(format!(
                "'{}' is not a directory",
                params.path
            )));
        }

        let read_dir = std::fs::read_dir(&requested).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => RpcError::NotFound(params.path.clone()),
            _ => RpcError::internal(format!("failed to list '{}': {e}", params.path)),
        })?;

        // Entry paths are emitted relative to the file root so the Web client
        // can join them onto its own workspace path.
        let root_canonical = root.canonicalize().unwrap_or_else(|_| root.clone());
        let relative_base = requested
            .strip_prefix(&root_canonical)
            .unwrap_or(Path::new(""));

        let mut items: Vec<FileEntryView> = Vec::new();
        for entry in read_dir {
            let entry = entry.map_err(|e| {
                RpcError::internal(format!(
                    "failed to read a directory entry under '{}': {e}",
                    params.path
                ))
            })?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let file_type = entry
                .file_type()
                .map_err(|e| RpcError::internal(format!("failed to stat '{}': {e}", name)))?;
            let metadata = entry.metadata().ok();
            items.push(FileEntryView {
                name: name.clone(),
                path: relative_base.join(&name).to_string_lossy().into_owned(),
                kind: if file_type.is_dir() { "dir" } else { "file" },
                size: metadata.as_ref().map(|m| m.len()),
                modified_at: metadata
                    .as_ref()
                    .and_then(|m| m.modified().ok())
                    .map(DateTime::<Utc>::from),
            });
        }
        items.sort_by(|a, b| a.name.cmp(&b.name));
        // The wire shape is `{items, truncated}` (the Web client's
        // `listDirectory` reads `result.items` / `result.truncated`) — NOT
        // the `{"entries": [...]}` wrapper the server previously sent, which
        // the Web's `items ?? []` silently swallowed as an empty list. The
        // listing has no cap, so `truncated` is honestly always `false`.
        to_json(ListFilesResult {
            items,
            truncated: false,
        })
    }
}

/// The `file.list` result — the directory listing the Web client's
/// `listDirectory` reads (current shape: items/truncated; the optional
/// `childrenByPath` is not emitted — depth traversal is future engine
/// work).
#[derive(Debug, Serialize)]
struct ListFilesResult {
    items: Vec<FileEntryView>,
    truncated: bool,
}

/// One entry of the `file.list` result — a directory or file in the listed
/// directory (current element shape: name/path/kind/size/modifiedAt, with
/// size/modifiedAt `null` when the entry's metadata cannot be read).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FileEntryView {
    name: String,
    path: String,
    kind: &'static str,
    size: Option<u64>,
    modified_at: Option<DateTime<Utc>>,
}

/// `file.read` — read a file from the workspace.
///
/// The path is confined to the session's file root (see
/// [`ListFilesHandler`]). Files larger than [`MAX_FILE_READ_BYTES`] are
/// rejected with a clear error. UTF-8 text is returned with
/// `encoding: "utf-8"`; any other file is returned base64-encoded with
/// `encoding: "base64"` and `isBinary: true` — the Web client builds
/// `data:` URLs for image/PDF previews from that payload.
///
/// The result carries the full v2 `fs:read` contract shape
/// (path/size/mime/languageId/lineCount/isBinary/truncated/etag) so the
/// Web file-preview panel can pick the right renderer — the old
/// content/encoding-only shape left every preview field `undefined`, which
/// rendered as "Binary file · unknown type · NaN MB bytes · preview
/// unavailable".
pub struct ReadFileHandler {
    manager: Arc<SessionManager>,
    routing: Arc<dyn SessionRouting>,
}

impl ReadFileHandler {
    pub fn new(manager: Arc<SessionManager>) -> Self {
        Self {
            routing: Arc::new(SingleManagerRouting::live(Arc::clone(&manager))),
            manager,
        }
    }

    /// Route session resolutions through an explicit [`SessionRouting`]
    /// implementation (the composition root's registry-backed routing).
    pub fn with_routing(mut self, routing: Arc<dyn SessionRouting>) -> Self {
        self.routing = routing;
        self
    }
}

#[async_trait]
impl RpcHandler for ReadFileHandler {
    fn method(&self) -> &'static str {
        "file.read"
    }

    async fn handle(&self, params: Value) -> RpcResult<Value> {
        let params: FilePathParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| RpcError::InvalidParams(e.to_string()))?;

        let session = live_or_restored_session(self.routing.as_ref(), &params.session_id)
            .await
            .ok_or_else(|| RpcError::NotFound(params.session_id.clone()))?;
        let root = session_file_root(&session, &self.manager);
        let requested = validate_workspace_path(&root, Path::new(&params.path))
            .map_err(|e| RpcError::InvalidParams(format!("path rejected: {e}")))?;
        if !requested.is_file() {
            return Err(RpcError::NotFound(format!(
                "'{}' is not a file",
                params.path
            )));
        }

        let metadata = std::fs::metadata(&requested).map_err(|e| {
            RpcError::internal(format!("failed to stat '{}': {e}", params.path))
        })?;
        let size = metadata.len();
        if size > MAX_FILE_READ_BYTES {
            return Err(RpcError::InvalidParams(format!(
                "'{}' is {size} bytes; reads are capped at {MAX_FILE_READ_BYTES} bytes",
                params.path
            )));
        }

        let modified_at = metadata
            .modified()
            .ok()
            .map(|m| DateTime::<Utc>::from(m).timestamp_millis());

        let bytes = std::fs::read(&requested)
            .map_err(|e| RpcError::internal(format!("failed to read '{}': {e}", params.path)))?;
        let is_binary = std::str::from_utf8(&bytes).is_err();
        let (content, line_count) = if is_binary {
            (
                base64::engine::general_purpose::STANDARD.encode(&bytes),
                None,
            )
        } else {
            // The utf-8 check above held, so this unwrap is the same check
            // replayed on an unchanged slice.
            let text = String::from_utf8(bytes).expect("utf-8 validated above");
            let count = count_text_lines(&text);
            (text, Some(count))
        };
        let etag = build_read_etag(size, modified_at);
        to_json(ReadFileResult {
            path: params.path.clone(),
            content,
            encoding: if is_binary { "base64" } else { "utf-8" },
            size,
            truncated: false,
            etag,
            mime: guess_file_mime(&requested, is_binary).to_string(),
            language_id: guess_language_id(&requested),
            line_count,
            is_binary,
        })
    }
}

/// The `file.read` result — the file's content plus the metadata fields the
/// Web file-preview panel renders (camelCase, matching the `file.list` wire
/// style the Web client's `readFile` consumes directly).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReadFileResult {
    path: String,
    content: String,
    encoding: &'static str,
    size: u64,
    truncated: bool,
    etag: String,
    mime: String,
    language_id: Option<&'static str>,
    line_count: Option<u64>,
    is_binary: bool,
}

/// Line count of decoded text — a trailing newline does not open a new line
/// (mirrors the v2 `countLines`).
fn count_text_lines(text: &str) -> u64 {
    if text.is_empty() {
        return 0;
    }
    let n = text.bytes().filter(|b| *b == b'\n').count() as u64;
    if text.ends_with('\n') {
        n
    } else {
        n + 1
    }
}

/// The etag for a read result — mtime-ms / size / 0 in base-36 (the v2
/// `buildEtag` shape; the read result only has the size/mtime pair, so the
/// inode component is 0).
fn build_read_etag(size: u64, modified_at_ms: Option<i64>) -> String {
    format!(
        "{}-{}-0",
        radix36(modified_at_ms.unwrap_or(0).unsigned_abs()),
        radix36(size)
    )
}

/// Base-36 of a u64 (mtime ms / size are far below u64::MAX in practice).
fn radix36(mut value: u64) -> String {
    if value == 0 {
        return "0".to_string();
    }
    const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    while value > 0 {
        out.push(ALPHABET[(value % 36) as usize]);
        value /= 36;
    }
    out.reverse();
    String::from_utf8(out).expect("alphabet is ASCII")
}

/// Extension → mime mapping, mirroring the v2 `EXT_TO_MIME` table.
fn guess_file_mime(path: &Path, is_binary: bool) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "ts" | "tsx" => "text/typescript",
        "js" | "jsx" | "mjs" | "cjs" => "text/javascript",
        "json" => "application/json",
        "md" => "text/markdown",
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "pdf" => "application/pdf",
        "yaml" | "yml" => "text/yaml",
        "toml" => "application/toml",
        "sh" => "text/x-shellscript",
        "py" => "text/x-python",
        "rs" => "text/rust",
        "go" => "text/x-go",
        _ => {
            if is_binary {
                "application/octet-stream"
            } else {
                "text/plain"
            }
        }
    }
}

/// Extension → languageId mapping, mirroring the v2 `EXT_TO_LANGUAGE` table.
fn guess_language_id(path: &Path) -> Option<&'static str> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    let lang = match ext.as_str() {
        "ts" => "typescript",
        "tsx" => "typescriptreact",
        "js" | "mjs" | "cjs" => "javascript",
        "jsx" => "javascriptreact",
        "json" => "json",
        "md" => "markdown",
        "html" => "html",
        "css" => "css",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "sh" => "shellscript",
        "py" => "python",
        "rs" => "rust",
        "go" => "go",
        _ => return None,
    };
    Some(lang)
}

// ===========================================================================
// file.search / file.grep / file.git_status / file.diff
// ===========================================================================

/// Cap on walkdir depth for `file.search`: deep dependency trees are not
/// useful search targets and bounding the walk keeps the request cheap.
const MAX_SEARCH_DEPTH: usize = 10;

/// Cap on `file.diff` payloads: a diff over 100 KiB is almost certainly a
/// machine-generated blob; the response is truncated instead of shipped.
const MAX_DIFF_BYTES: usize = 100 * 1024;

/// `file.search` — fuzzy file-name search within the session's workspace.
///
/// Every file under the workspace root (bounded by [`MAX_SEARCH_DEPTH`],
/// skipping `.git`) is matched by case-insensitive substring on its file
/// name; prefix matches rank above containment matches. Items carry paths
/// relative to the workspace root so the client can join them onto its own
/// path, like [`ListFilesHandler`]. `include_globs` / `exclude_globs` filter
/// the relative path with [`glob_lite`]. Each item's `matchPositions` is
/// always an empty array today — the Web client reads the camelCase key,
/// and position tracking is future engine work.
pub struct SearchFilesHandler {
    manager: Arc<SessionManager>,
    routing: Arc<dyn SessionRouting>,
}

impl SearchFilesHandler {
    pub fn new(manager: Arc<SessionManager>) -> Self {
        Self {
            routing: Arc::new(SingleManagerRouting::live(Arc::clone(&manager))),
            manager,
        }
    }

    /// Route session resolutions through an explicit [`SessionRouting`]
    /// implementation (the composition root's registry-backed routing).
    pub fn with_routing(mut self, routing: Arc<dyn SessionRouting>) -> Self {
        self.routing = routing;
        self
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SearchFilesParams {
    #[serde(rename = "sessionId")]
    session_id: String,
    query: String,
    #[serde(default = "default_search_limit")]
    limit: usize,
    #[serde(default)]
    include_globs: Vec<String>,
    #[serde(default)]
    exclude_globs: Vec<String>,
}

fn default_search_limit() -> usize {
    50
}

#[async_trait]
impl RpcHandler for SearchFilesHandler {
    fn method(&self) -> &'static str {
        "file.search"
    }

    async fn handle(&self, params: Value) -> RpcResult<Value> {
        let params: SearchFilesParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| RpcError::InvalidParams(e.to_string()))?;

        let session = live_or_restored_session(self.routing.as_ref(), &params.session_id)
            .await
            .ok_or_else(|| RpcError::NotFound(params.session_id.clone()))?;
        let root = session_file_root(&session, &self.manager);

        let query = params.query.trim().to_lowercase();
        if query.is_empty() || params.limit == 0 {
            // An empty query matches every file name; treat it as "no
            // filter" and answer an empty list instead of walking the tree.
            return to_json(SearchFilesResult {
                items: Vec::new(),
                truncated: false,
            });
        }

        // (score, workspace-relative path, file name) — score 1.0 for a
        // prefix match, 0.5 for a containment match.
        let mut matches: Vec<(f64, String, String)> = Vec::new();
        for entry in walkdir::WalkDir::new(&root)
            .max_depth(MAX_SEARCH_DEPTH)
            .into_iter()
            .filter_entry(|entry| !is_git_directory(entry))
        {
            let Ok(entry) = entry else {
                continue; // unreadable entry — skip it, keep the walk going
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let name_lower = name.to_lowercase();
            if !name_lower.contains(&query) {
                continue;
            }
            let relative = entry
                .path()
                .strip_prefix(&root)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .replace('\\', "/");
            if !params.include_globs.is_empty()
                && !matches_any_glob(&params.include_globs, &relative)
            {
                continue;
            }
            if matches_any_glob(&params.exclude_globs, &relative) {
                continue;
            }
            let score = if name_lower.starts_with(&query) {
                1.0
            } else {
                0.5
            };
            matches.push((score, relative, name));
        }

        // Prefix matches first, then path order for a deterministic listing.
        matches.sort_by(|a, b| {
            b.0.partial_cmp(&a.0)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.1.cmp(&b.1))
        });
        let truncated = matches.len() > params.limit;
        matches.truncate(params.limit);
        let items: Vec<SearchItemView> = matches
            .into_iter()
            .map(|(score, path, name)| SearchItemView {
                path,
                name,
                kind: "file",
                score,
                // Always empty today — see the handler doc comment.
                match_positions: Vec::new(),
            })
            .collect();
        // The wire shape is `{items, truncated}` (unchanged); the element
        // key `matchPositions` is now camelCase — the Web client's
        // `searchFiles` reads the camelCase key the snake_case original
        // never matched.
        to_json(SearchFilesResult { items, truncated })
    }
}

/// The `file.search` result — the ranked matches the Web client's
/// `searchFiles` reads (current shape: items/truncated).
#[derive(Debug, Serialize)]
struct SearchFilesResult {
    items: Vec<SearchItemView>,
    truncated: bool,
}

/// One item of the `file.search` result — a file whose name matched the
/// query (current element shape: path/name/kind/score/matchPositions, with
/// `matchPositions` always an empty array today).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SearchItemView {
    path: String,
    name: String,
    kind: &'static str,
    score: f64,
    match_positions: Vec<u64>,
}

/// Whether a walk entry is the `.git` directory — never a search target.
fn is_git_directory(entry: &walkdir::DirEntry) -> bool {
    entry.file_type().is_dir() && entry.file_name() == std::ffi::OsStr::new(".git")
}

/// Whether `path` (relative to the workspace root) matches any pattern in
/// `globs`. An empty list matches nothing.
fn matches_any_glob(globs: &[String], path: &str) -> bool {
    globs.iter().any(|pattern| glob_lite(pattern, path))
}

/// Lightweight glob matching for `file.search`'s `include_globs` /
/// `exclude_globs`: `*` matches any run of characters, `?` matches a single
/// character, and matching is case-insensitive over the workspace-relative
/// path. `**` is collapsed into `*` (the walk already bounds depth, so
/// "any directories" and "any characters" are equivalent here); a pattern
/// without wildcards must appear as a substring of the path.
fn glob_lite(pattern: &str, path: &str) -> bool {
    let pattern = pattern.replace("**", "*").to_lowercase();
    let path = path.to_lowercase();
    glob_lite_match(pattern.as_bytes(), path.as_bytes())
}

fn glob_lite_match(pattern: &[u8], path: &[u8]) -> bool {
    match pattern.first() {
        None => path.is_empty(),
        Some(b'*') => {
            let rest = &pattern[1..];
            (0..=path.len()).any(|i| glob_lite_match(rest, &path[i..]))
        }
        Some(b'?') => path
            .first()
            .is_some_and(|_| glob_lite_match(&pattern[1..], &path[1..])),
        Some(expected) => path
            .first()
            .is_some_and(|actual| actual == expected && glob_lite_match(&pattern[1..], &path[1..])),
    }
}

/// `file.grep` — content search with ripgrep within the session's workspace.
///
/// `rg --json` is spawned with the workspace root as its working directory;
/// match and context lines are reassembled into per-match before/after
/// windows. Ripgrep exits `1` when nothing matches, which is a successful
/// empty result rather than a failure; a missing `rg` binary is an internal
/// error. The result's `filesScanned` / `elapsedMs` keys are camelCase —
/// the Web client's `grepFiles` reads the camelCase keys the snake_case
/// originals never matched.
pub struct GrepFilesHandler {
    manager: Arc<SessionManager>,
    routing: Arc<dyn SessionRouting>,
}

impl GrepFilesHandler {
    pub fn new(manager: Arc<SessionManager>) -> Self {
        Self {
            routing: Arc::new(SingleManagerRouting::live(Arc::clone(&manager))),
            manager,
        }
    }

    /// Route session resolutions through an explicit [`SessionRouting`]
    /// implementation (the composition root's registry-backed routing).
    pub fn with_routing(mut self, routing: Arc<dyn SessionRouting>) -> Self {
        self.routing = routing;
        self
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GrepFilesParams {
    #[serde(rename = "sessionId")]
    session_id: String,
    pattern: String,
    #[serde(default)]
    regex: bool,
    #[serde(default = "default_true")]
    case_sensitive: bool,
    #[serde(default)]
    include_globs: Vec<String>,
    #[serde(default)]
    exclude_globs: Vec<String>,
    #[serde(default = "default_grep_max_files")]
    max_files: usize,
    #[serde(default = "default_grep_max_matches_per_file")]
    max_matches_per_file: usize,
    #[serde(default = "default_grep_context_lines")]
    context_lines: usize,
}

fn default_true() -> bool {
    true
}

fn default_grep_max_files() -> usize {
    200
}

fn default_grep_max_matches_per_file() -> usize {
    50
}

fn default_grep_context_lines() -> usize {
    2
}

/// One grep match, wire-ready: 1-based line/column, the matched line's text,
/// and the surrounding context windows (already resolved into line text).
#[derive(Debug)]
struct GrepMatchEntry {
    line: u64,
    col: u64,
    text: String,
    before: Vec<String>,
    after: Vec<String>,
}

/// Files scanned plus per-file match entries, in ripgrep's output order.
type RgFilesResult = (u64, Vec<(String, Vec<GrepMatchEntry>)>);

#[async_trait]
impl RpcHandler for GrepFilesHandler {
    fn method(&self) -> &'static str {
        "file.grep"
    }

    async fn handle(&self, params: Value) -> RpcResult<Value> {
        let params: GrepFilesParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| RpcError::InvalidParams(e.to_string()))?;
        if params.pattern.is_empty() {
            return Err(RpcError::InvalidParams(
                "pattern must not be empty".to_string(),
            ));
        }

        let session = live_or_restored_session(self.routing.as_ref(), &params.session_id)
            .await
            .ok_or_else(|| RpcError::NotFound(params.session_id.clone()))?;
        let root = session_file_root(&session, &self.manager);

        let max_files = params.max_files;
        let max_matches_per_file = params.max_matches_per_file;
        let started = std::time::Instant::now();
        let (files_scanned, file_matches) =
            tokio::task::spawn_blocking(move || run_ripgrep(&root, &params))
                .await
                .map_err(|e| RpcError::internal(format!("ripgrep task failed: {e}")))??;
        let elapsed_ms = started.elapsed().as_millis() as u64;

        let truncated = file_matches.len() > max_files
            || file_matches
                .iter()
                .any(|(_, matches)| matches.len() > max_matches_per_file);
        let files: Vec<GrepFileView> = file_matches
            .into_iter()
            .take(max_files)
            .map(|(path, matches)| GrepFileView {
                path,
                matches: matches
                    .into_iter()
                    .take(max_matches_per_file)
                    .map(|entry| GrepMatchView {
                        line: entry.line,
                        col: entry.col,
                        text: entry.text,
                        before: entry.before,
                        after: entry.after,
                    })
                    .collect(),
            })
            .collect();
        // The wire shape is `{files, filesScanned, truncated, elapsedMs}`
        // (the Web client's `grepFiles` contract) — the `filesScanned` /
        // `elapsedMs` keys are now camelCase, fixing the snake_case drift
        // the Web never read.
        to_json(GrepFilesResult {
            files,
            files_scanned,
            truncated,
            elapsed_ms,
        })
    }
}

/// The `file.grep` result — the ripgrep matches the Web client's
/// `grepFiles` reads (current shape: files/filesScanned/truncated/elapsedMs,
/// camelCase).
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct GrepFilesResult {
    files: Vec<GrepFileView>,
    files_scanned: u64,
    truncated: bool,
    elapsed_ms: u64,
}

/// One matched file of the `file.grep` result (current element shape:
/// path/matches).
#[derive(Debug, Serialize)]
struct GrepFileView {
    path: String,
    matches: Vec<GrepMatchView>,
}

/// One match within a `file.grep` file (current element shape:
/// line/col/text/before/after) — the wire view of [`GrepMatchEntry`].
#[derive(Debug, Serialize)]
struct GrepMatchView {
    line: u64,
    col: u64,
    text: String,
    before: Vec<String>,
    after: Vec<String>,
}

/// Spawn one `rg --json` invocation in `root` (blocking — callers run this
/// on the blocking pool) and return the number of files ripgrep scanned plus
/// the per-file match list, in ripgrep's output order.
fn run_ripgrep(root: &Path, params: &GrepFilesParams) -> RpcResult<RgFilesResult> {
    let mut command = std::process::Command::new("rg");
    command
        .arg("--json")
        .arg("--line-number")
        .arg("--with-filename")
        .current_dir(root);
    if !params.case_sensitive {
        command.arg("--ignore-case");
    }
    if params.context_lines > 0 {
        command.arg("-C").arg(params.context_lines.to_string());
    }
    if !params.regex {
        command.arg("-F");
    }
    for include in &params.include_globs {
        command.arg("-g").arg(include);
    }
    for exclude in &params.exclude_globs {
        command.arg("-g").arg(format!("!{exclude}"));
    }
    command.arg("--").arg(&params.pattern);

    let output = command.output().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            RpcError::internal("ripgrep not found")
        } else {
            RpcError::internal(format!("failed to spawn ripgrep: {e}"))
        }
    })?;

    // Exit code 1 means "no matches" — a valid empty result, not a failure.
    if output.status.code().is_some_and(|code| code > 1) {
        return Err(RpcError::internal(format!(
            "ripgrep failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim(),
        )));
    }
    Ok(parse_ripgrep_json(&output.stdout, params.context_lines))
}

/// Parse `rg --json` output: `match` lines become entries; `context` lines
/// feed each match's before/after windows (match lines are merged in too, so
/// a line that is itself a match still counts as context for a neighbouring
/// match). The trailing `summary` line supplies the scanned-file count, with
/// the distinct matched paths as a fallback.
fn parse_ripgrep_json(stdout: &[u8], context_lines: usize) -> RgFilesResult {
    let mut files_scanned: Option<u64> = None;
    let mut file_order: Vec<String> = Vec::new();
    let mut file_index: HashMap<String, usize> = HashMap::new();
    let mut matches_by_file: Vec<Vec<(u64, u64, String)>> = Vec::new();
    let mut lines_by_file: Vec<BTreeMap<u64, String>> = Vec::new();

    for line in String::from_utf8_lossy(stdout).lines() {
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(kind) = value.get("type").and_then(Value::as_str) else {
            continue;
        };
        match kind {
            "summary" => {
                files_scanned = value
                    .pointer("/data/stats/searches")
                    .and_then(Value::as_u64);
            }
            "match" | "context" => {
                let Some(path) = value
                    .pointer("/data/path/text")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                else {
                    continue;
                };
                let Some(line_number) = value.pointer("/data/line_number").and_then(Value::as_u64)
                else {
                    continue;
                };
                let Some(text) = value
                    .pointer("/data/lines/text")
                    .and_then(Value::as_str)
                    .map(|t| t.trim_end_matches(['\n', '\r']).to_string())
                else {
                    continue;
                };
                let index = match file_index.get(&path) {
                    Some(&index) => index,
                    None => {
                        file_index.insert(path.clone(), file_order.len());
                        file_order.push(path);
                        matches_by_file.push(Vec::new());
                        lines_by_file.push(BTreeMap::new());
                        file_order.len() - 1
                    }
                };
                lines_by_file[index].insert(line_number, text.clone());
                if kind == "match" {
                    let col = value
                        .pointer("/data/submatches/0/start")
                        .and_then(Value::as_u64)
                        .map(|start| start + 1)
                        .unwrap_or(1);
                    matches_by_file[index].push((line_number, col, text));
                }
            }
            _ => {}
        }
    }
    let files_scanned = files_scanned.unwrap_or(file_order.len() as u64);

    let window = context_lines as u64;
    let files = file_order
        .into_iter()
        .map(|path| {
            let index = file_index[&path];
            let entries = matches_by_file[index]
                .iter()
                .map(|(line, col, text)| {
                    let before: Vec<String> = lines_by_file[index]
                        .range(line.saturating_sub(window)..*line)
                        .map(|(_, text)| text.clone())
                        .collect();
                    let after: Vec<String> = lines_by_file[index]
                        .range((*line + 1)..=(*line + window))
                        .map(|(_, text)| text.clone())
                        .collect();
                    GrepMatchEntry {
                        line: *line,
                        col: *col,
                        text: text.clone(),
                        before,
                        after,
                    }
                })
                .collect();
            (path, entries)
        })
        .collect();
    (files_scanned, files)
}

/// `file.git_status` — git status of the session's workspace.
///
/// The workspace must be inside a git work tree: `git rev-parse
/// --is-inside-work-tree` is checked first, and a non-repo workspace yields
/// the empty status object rather than an error. Status is read with
/// `git status --porcelain=v1 --branch`; each porcelain XY code is collapsed
/// to the wire state the TS client renders (`modified` / `added` / `deleted`
/// / `renamed` / `untracked` / `ignored` / `conflicted`), and
/// `additions`/`deletions` come from `git diff --numstat HEAD`. The Web
/// client's type also declares a `pullRequest` field, which the server does
/// not emit — an engine-feature gap, recorded separately.
pub struct GitStatusFilesHandler {
    manager: Arc<SessionManager>,
    routing: Arc<dyn SessionRouting>,
}

impl GitStatusFilesHandler {
    pub fn new(manager: Arc<SessionManager>) -> Self {
        Self {
            routing: Arc::new(SingleManagerRouting::live(Arc::clone(&manager))),
            manager,
        }
    }

    /// Route session resolutions through an explicit [`SessionRouting`]
    /// implementation (the composition root's registry-backed routing).
    pub fn with_routing(mut self, routing: Arc<dyn SessionRouting>) -> Self {
        self.routing = routing;
        self
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GitStatusFilesParams {
    #[serde(rename = "sessionId")]
    session_id: String,
    // Accepted for protocol-shape validation; the status is currently
    // computed for the whole workspace regardless of the requested paths.
    #[serde(rename = "paths", default)]
    _paths: Option<Vec<String>>,
}

#[async_trait]
impl RpcHandler for GitStatusFilesHandler {
    fn method(&self) -> &'static str {
        "file.git_status"
    }

    async fn handle(&self, params: Value) -> RpcResult<Value> {
        let params: GitStatusFilesParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| RpcError::InvalidParams(e.to_string()))?;

        let session = live_or_restored_session(self.routing.as_ref(), &params.session_id)
            .await
            .ok_or_else(|| RpcError::NotFound(params.session_id.clone()))?;
        let root = session_file_root(&session, &self.manager);

        let status = tokio::task::spawn_blocking(move || git_status(&root))
            .await
            .map_err(|e| RpcError::internal(format!("git status task failed: {e}")))??;
        // The wire shape is `{branch, ahead, behind, entries, additions,
        // deletions}` — unchanged; this is only a typed re-serialization.
        to_json(status)
    }
}

/// Run `git <args>` in `root` and capture the output.
fn git_output(root: &Path, args: &[&str]) -> std::io::Result<std::process::Output> {
    std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
}

/// The `file.git_status` result — the workspace's git status the Web client
/// reads (current shape: branch/ahead/behind/entries/additions/deletions;
/// the Web's type also declares `pullRequest`, which the server does not
/// emit — an engine-feature gap, recorded separately). `entries` maps each
/// changed workspace-relative path to its rendered state.
#[derive(Debug, Default, Serialize)]
struct GitStatusView {
    branch: String,
    ahead: u64,
    behind: u64,
    entries: BTreeMap<String, String>,
    additions: u64,
    deletions: u64,
}

/// The wire shape of the status reported for a workspace outside any git
/// work tree.
fn empty_git_status() -> GitStatusView {
    GitStatusView::default()
}

/// Compute the workspace's git status; a workspace that is not inside a git
/// work tree yields [`empty_git_status`].
fn git_status(root: &Path) -> RpcResult<GitStatusView> {
    let inside = git_output(root, &["rev-parse", "--is-inside-work-tree"])
        .map_err(|e| RpcError::internal(format!("failed to spawn git: {e}")))?;
    if !inside.status.success() || inside.stdout.trim_ascii() != b"true" {
        return Ok(empty_git_status());
    }

    let porcelain = git_output(root, &["status", "--porcelain=v1", "--branch"])
        .map_err(|e| RpcError::internal(format!("failed to spawn git: {e}")))?;
    if !porcelain.status.success() {
        return Ok(empty_git_status());
    }

    let mut branch = String::new();
    let mut ahead: u64 = 0;
    let mut behind: u64 = 0;
    let mut entries: BTreeMap<String, String> = BTreeMap::new();
    for raw in String::from_utf8_lossy(&porcelain.stdout).lines() {
        if let Some(header) = raw.strip_prefix("## ") {
            parse_branch_header(header, &mut branch, &mut ahead, &mut behind);
            continue;
        }
        if raw.len() < 3 {
            continue;
        }
        let (xy, raw_path) = raw.split_at(2);
        let Some(state) = porcelain_status_to_state(xy) else {
            continue;
        };
        entries.insert(entry_path(raw_path.trim()).to_string(), state.to_string());
    }

    // additions / deletions vs HEAD: numstat lines are `<added>\t<deleted>\t<path>`.
    let mut additions: u64 = 0;
    let mut deletions: u64 = 0;
    if let Ok(numstat) = git_output(root, &["diff", "--numstat", "HEAD"])
        && numstat.status.success()
    {
        for line in String::from_utf8_lossy(&numstat.stdout).lines() {
            let mut fields = line.split('\t');
            let added = fields.next().unwrap_or("-");
            let deleted = fields.next().unwrap_or("-");
            if let (Ok(added), Ok(deleted)) = (added.parse::<u64>(), deleted.parse::<u64>()) {
                additions += added;
                deletions += deleted;
            }
        }
    }

    Ok(GitStatusView {
        branch,
        ahead,
        behind,
        entries,
        additions,
        deletions,
    })
}

/// Parse `git status --porcelain=v1 --branch`'s header line, e.g.
/// `## main...origin/main [ahead 1, behind 2]` or `## No commits yet on main`.
/// The local branch name is everything before `...` (with the unborn-branch
/// prefix stripped); `ahead`/`behind` come from the bracket markers and stay
/// `0` when absent.
fn parse_branch_header(header: &str, branch: &mut String, ahead: &mut u64, behind: &mut u64) {
    let name = header.split("...").next().unwrap_or(header);
    let name = name.strip_prefix("No commits yet on ").unwrap_or(name);
    *branch = name.to_string();
    *ahead = 0;
    *behind = 0;
    let Some(bracket_start) = header.find('[') else {
        return;
    };
    for marker in header[bracket_start..].trim_matches(['[', ']']).split(',') {
        let marker = marker.trim();
        if let Some(count) = marker.strip_prefix("ahead ") {
            *ahead = count.trim().parse().unwrap_or(0);
        } else if let Some(count) = marker.strip_prefix("behind ") {
            *behind = count.trim().parse().unwrap_or(0);
        }
    }
}

/// Collapse a porcelain v1 XY code to the wire state the TS client renders.
///
/// Unmerged (U) wins over everything, then added/deleted/renamed/untracked/
/// ignored, then modified — this covers the exact codes the client maps
/// (` M`, `A `, `D `, `R `, `??`, `!!`, `UU`) plus the ordinary
/// staged/unstaged combinations (`MM`, `AM`, ` D`, ...).
fn porcelain_status_to_state(xy: &str) -> Option<&'static str> {
    let bytes = xy.as_bytes();
    let (Some(x), Some(y)) = (bytes.first(), bytes.get(1)) else {
        return None;
    };
    for code in [x, y] {
        match *code {
            b'U' => return Some("conflicted"),
            b'A' => return Some("added"),
            b'D' => return Some("deleted"),
            b'R' => return Some("renamed"),
            b'?' => return Some("untracked"),
            b'!' => return Some("ignored"),
            _ => {}
        }
    }
    if x == &b'M' || y == &b'M' {
        Some("modified")
    } else {
        None
    }
}

/// The tracked path of a porcelain v1 entry: rename entries read
/// `old -> new`, and only the destination path is a live workspace path.
fn entry_path(raw: &str) -> &str {
    match raw.rsplit_once(" -> ") {
        Some((_, new_path)) => new_path,
        None => raw,
    }
}

/// `file.diff` — git diff of a specific file in the session's workspace.
///
/// The requested path is confined to the workspace root (the same
/// [`validate_workspace_path`] containment check `file.read` uses) and
/// passed to `git diff HEAD` as a path relative to it. Diffs larger than
/// [`MAX_DIFF_BYTES`] are truncated with `truncated: true`. A workspace
/// outside a git work tree fails with an internal error.
pub struct DiffFilesHandler {
    manager: Arc<SessionManager>,
    routing: Arc<dyn SessionRouting>,
}

impl DiffFilesHandler {
    pub fn new(manager: Arc<SessionManager>) -> Self {
        Self {
            routing: Arc::new(SingleManagerRouting::live(Arc::clone(&manager))),
            manager,
        }
    }

    /// Route session resolutions through an explicit [`SessionRouting`]
    /// implementation (the composition root's registry-backed routing).
    pub fn with_routing(mut self, routing: Arc<dyn SessionRouting>) -> Self {
        self.routing = routing;
        self
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiffFilesParams {
    #[serde(rename = "sessionId")]
    session_id: String,
    path: String,
}

#[async_trait]
impl RpcHandler for DiffFilesHandler {
    fn method(&self) -> &'static str {
        "file.diff"
    }

    async fn handle(&self, params: Value) -> RpcResult<Value> {
        let params: DiffFilesParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| RpcError::InvalidParams(e.to_string()))?;

        let session = live_or_restored_session(self.routing.as_ref(), &params.session_id)
            .await
            .ok_or_else(|| RpcError::NotFound(params.session_id.clone()))?;
        let root = session_file_root(&session, &self.manager);
        let requested = validate_workspace_path(&root, Path::new(&params.path))
            .map_err(|e| RpcError::InvalidParams(format!("path rejected: {e}")))?;
        let root_canonical = root.canonicalize().unwrap_or_else(|_| root.clone());
        let relative = requested
            .strip_prefix(&root_canonical)
            .unwrap_or(Path::new(&params.path))
            .to_string_lossy()
            .into_owned();

        let (diff, truncated) = tokio::task::spawn_blocking(move || git_diff(&root, &relative))
            .await
            .map_err(|e| RpcError::internal(format!("git diff task failed: {e}")))??;

        // The wire shape is `{path, diff, truncated}` — unchanged; this is
        // only a typed re-serialization.
        to_json(DiffFilesResult {
            path: params.path,
            diff,
            truncated,
        })
    }
}

/// The `file.diff` result — the unified diff of one requested file (current
/// shape: path/diff/truncated).
#[derive(Debug, Serialize)]
struct DiffFilesResult {
    path: String,
    diff: String,
    truncated: bool,
}

/// Run `git diff HEAD -- <relative>` in `root` and return the diff text (or
/// a [`MAX_DIFF_BYTES`]-capped prefix) plus whether it was truncated.
///
/// `git diff` exits `1` when there are differences — a normal result, not an
/// error, so only exit codes above `1` are treated as failures. A workspace
/// outside a git work tree is an internal error.
fn git_diff(root: &Path, relative: &str) -> RpcResult<(String, bool)> {
    let inside = git_output(root, &["rev-parse", "--is-inside-work-tree"])
        .map_err(|e| RpcError::internal(format!("failed to spawn git: {e}")))?;
    if !inside.status.success() || inside.stdout.trim_ascii() != b"true" {
        return Err(RpcError::internal("not a git repository"));
    }

    let output = git_output(root, &["diff", "HEAD", "--", relative])
        .map_err(|e| RpcError::internal(format!("failed to spawn git: {e}")))?;
    if output.status.code().is_some_and(|code| code > 1) {
        return Err(RpcError::internal(format!(
            "git diff failed with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim(),
        )));
    }

    let diff = String::from_utf8_lossy(&output.stdout).into_owned();
    if diff.len() > MAX_DIFF_BYTES {
        // Cap at the byte budget without splitting a UTF-8 character.
        let mut truncated = String::with_capacity(MAX_DIFF_BYTES);
        for ch in diff.chars() {
            if truncated.len() + ch.len_utf8() > MAX_DIFF_BYTES {
                break;
            }
            truncated.push(ch);
        }
        Ok((truncated, true))
    } else {
        Ok((diff, false))
    }
}

// ===========================================================================
// task.list / task.cancel
// ===========================================================================

/// `task.list` — the session's background tasks (e.g. long-running Bash
/// invocations) tracked by the session's [`TaskManager`](mirri_core::TaskManager).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionIdDualParams {
    #[serde(rename = "sessionId", alias = "session_id")]
    session_id: String,
}

/// `session.restore` — restore (resume) a persisted session into a live one.
///
/// Semantics mirror `session.resume`: the session is replayed from its wire
/// log / state snapshot and returned as the live session view.
pub struct RestoreSessionHandler {
    routing: Arc<dyn SessionRouting>,
    in_flight_turns: InFlightTurns,
    event_channels: Arc<SessionEventChannelRegistry>,
    epoch: Arc<str>,
    /// The engine-side in-flight turn registry (driver-injected turns the
    /// transport never tracked) — unioned into the view's busy flag.
    engine: Arc<dyn mirri_transport::adapter::EngineAdapter>,
}

impl RestoreSessionHandler {
    pub fn new(
        manager: Arc<SessionManager>,
        in_flight_turns: InFlightTurns,
        event_channels: Arc<SessionEventChannelRegistry>,
        epoch: Arc<str>,
        engine: Arc<dyn mirri_transport::adapter::EngineAdapter>,
    ) -> Self {
        Self {
            routing: Arc::new(SingleManagerRouting::live(manager)),
            in_flight_turns,
            event_channels,
            epoch,
            engine,
        }
    }

    /// Route session resolutions through an explicit [`SessionRouting`]
    /// implementation (the composition root's registry-backed routing).
    pub fn with_routing(mut self, routing: Arc<dyn SessionRouting>) -> Self {
        self.routing = routing;
        self
    }
}

#[async_trait]
impl RpcHandler for RestoreSessionHandler {
    fn method(&self) -> &'static str {
        "session.restore"
    }

    async fn handle(&self, params: Value) -> RpcResult<Value> {
        let params: SessionIdDualParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| RpcError::InvalidParams(e.to_string()))?;

        // Route first (W3): the owning workspace's manager replays the
        // session — a persisted session of a non-default workspace resumes
        // on its own runtime, never on the default's.
        let owner_manager = self
            .routing
            .owning_manager(&params.session_id)
            .ok_or_else(|| RpcError::NotFound(params.session_id.clone()))?;

        // Resume-time capacity limits for the fold simulation: the same
        // strategy caps the session's live turns ran with (`TurnFlow`
        // applies them via `set_limits` every turn), derived from the
        // engine's live server-config snapshot the manager carries (the
        // shared `Arc` the composition root wired — hot-reload swaps reach
        // it; the message-count caps do not depend on the model window).
        // `None` (a bare manager without a wired snapshot) keeps the
        // default constants (10 000 / 1 000), the historical behavior.
        let replay_limits = owner_manager.replay_limits();
        // The resume base config's model — the config snapshot's
        // `default_model` alias (spec 11 R9): a session whose wire log
        // carries no model record resumes on the configured default,
        // exactly like a fresh create would (the same base-config seeding
        // the embedded host's resume path performs; T1 review I-1 — a
        // bare default base here would make the no-record resume fail
        // where the embedded face resolves the default).
        let base_config = mirri_core::AgentScopeConfig {
            model_id: owner_manager.resume_default_model().unwrap_or_default(),
            ..mirri_core::AgentScopeConfig::default()
        };
        let session = owner_manager
            .resume_session_with_limits(
                &SessionId::new(&params.session_id),
                base_config,
                replay_limits,
            )
            .await
            .map_err(RpcError::from)?;

        to_json(
            session_view(
                &owner_manager,
                &session,
                &self.in_flight_turns,
                &self.event_channels,
                &self.epoch,
                &self.engine,
            )
            .await,
        )
    }
}

/// `session.reload` — reload a session: when the session is already live,
/// return its view unchanged; otherwise restore (resume) it from persistence
/// first.
///
/// The optional `forcePluginSessionStartReminder` flag is accepted for
/// protocol compatibility and ignored — the Rust kernel has no plugin
/// reminder system.
/// `btw.start` — fork the session's main agent into a behind-the-wheel
/// ("BTW") side-channel agent.
///
/// The fork inherits the main agent's model, provider, and context history
/// (via [`AgentLifecycle::fork_agent`]) but runs with all tools disabled
/// (`enabled_tools: []`) and a system reminder explaining the side-channel
/// role, so it can answer side questions without steering or disturbing the
/// main agent's turn. Returns the new agent's id.
pub struct StartBtwHandler {
    routing: Arc<dyn SessionRouting>,
}

impl StartBtwHandler {
    pub fn new(manager: Arc<SessionManager>) -> Self {
        Self {
            routing: Arc::new(SingleManagerRouting::live(manager)),
        }
    }

    /// Route session resolutions through an explicit [`SessionRouting`]
    /// implementation (the composition root's registry-backed routing).
    pub fn with_routing(mut self, routing: Arc<dyn SessionRouting>) -> Self {
        self.routing = routing;
        self
    }
}

/// System reminder appended to a BTW agent's context memory at fork time —
/// mirrors the TS kernel's `SIDE_QUESTION_SYSTEM_REMINDER`
/// (`packages/agent-core/src/session/subagent-host.ts`).
const BTW_SYSTEM_REMINDER: &str = "\
This is a side-channel conversation with the user. You should answer user questions directly based on what you already know.

IMPORTANT:
- You are a separate, lightweight instance.
- The main agent continues independently; do not reference being interrupted.
- Do not call any tools. All tool calls are disabled and will be rejected.
- Respond only with text based on what you already know from the conversation and this side-channel conversation.
- Follow-up turns may happen in this side-channel conversation.
- If you do not know the answer, say so directly.";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartBtwParams {
    #[serde(rename = "sessionId", alias = "session_id")]
    session_id: String,
}

/// `btw.start` result: the new agent's id under the `agent_id` key
/// (current wire shape — the historical snake_case key; a camelCase move is
/// a deliberate shape change, not a drive-by).
#[derive(Debug, Serialize)]
struct StartBtwResult {
    agent_id: String,
}

#[async_trait]
impl RpcHandler for StartBtwHandler {
    fn method(&self) -> &'static str {
        "btw.start"
    }

    async fn handle(&self, params: Value) -> RpcResult<Value> {
        let params: StartBtwParams = serde_json::from_value(normalize_params(params))
            .map_err(|e| RpcError::InvalidParams(e.to_string()))?;

        let session = live_or_restored_session(self.routing.as_ref(), &params.session_id)
            .await
            .ok_or_else(|| RpcError::NotFound(params.session_id.clone()))?;

        // The session's first agent is the main agent — the same addressing
        // rule `message.send` uses when `agentId` is omitted.
        let main_agent = require_session_agent(&session, &params.session_id, None).await?;

        // The BTW fork inherits the main agent's configuration except for the
        // tool set: all tools are disabled so the side channel answers from
        // context alone.
        let btw_config = {
            let config_arc = main_agent.config();
            let mut btw_config = config_arc.read().unwrap_or_else(|e| e.into_inner()).clone();
            btw_config.enabled_tools = Some(Vec::new());
            btw_config
        };

        let lifecycle = AgentLifecycle::new(Arc::clone(&session));
        let btw_agent = lifecycle
            .fork_agent(&main_agent, btw_config)
            .await
            .map_err(RpcError::from)?;

        let memory = btw_agent.context_memory();
        memory
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .append_system_reminder(BTW_SYSTEM_REMINDER.to_string());

        to_json(StartBtwResult {
            agent_id: btw_agent.id().as_str().to_string(),
        })
    }
}

impl From<mirri_transport::adapter::EngineError> for RpcError {
    /// Convert an engine-call error (from the engine fall-through in
    /// [`ServerState::dispatch`](crate::state::ServerState)) back into the
    /// RPC error surface the transports already speak.
    ///
    /// The engine already emitted the final wire facts (JSON-RPC code via the
    /// shared D5 mapping and the `domainCode` payload), so the conversion
    /// rebuilds the variant whose `json_rpc_code()` / `domain_code()` /
    /// envelope serialization reproduce them exactly: instance errors become
    /// `RpcError::NotFound`, session/agent not-found errors stay session
    /// errors (the `session.not_found`/`agent.not_found` domain codes the
    /// clients branch on), and the engine's `request.invalid`,
    /// `provider.api_error`, `internal` and `not_implemented` domain codes
    /// map back onto the corresponding domain variants. Anything the engine
    /// emitted outside that D5 surface (or a transport-level failure)
    /// collapses to a correlation-id-carrying internal error.
    fn from(error: mirri_transport::adapter::EngineError) -> Self {
        match error {
            mirri_transport::adapter::EngineError::MethodNotFound(method) => {
                RpcError::MethodNotFound(method)
            }
            mirri_transport::adapter::EngineError::Rpc {
                code,
                message,
                data,
            } => {
                // The domain-specific arms come first so the not-found code
                // keeps its domain decoration where the engine attached one.
                match data
                    .as_ref()
                    .and_then(|data| data.get("domainCode"))
                    .and_then(Value::as_str)
                {
                    Some("session.not_found") => {
                        RpcError::Session(mirri_core::SessionError::NotFound(message))
                    }
                    Some("agent.not_found") => {
                        RpcError::Session(mirri_core::SessionError::AgentNotFound(message))
                    }
                    Some("request.invalid") => {
                        RpcError::Engine(mirri_core::EngineError::RequestInvalid(message))
                    }
                    Some("provider.api_error") => {
                        RpcError::Engine(mirri_core::EngineError::ProviderError(message))
                    }
                    // The quota-exhausted arm (P11a F-P8-13): preserve the
                    // domain code AND the provider's Retry-After hint — the
                    // engine attached both to the data payload, and the
                    // response envelope re-surfaces the hint so clients can
                    // show "retry after N" without re-parsing the message.
                    Some("provider.quota_exhausted") => {
                        let retry_after_ms = data
                            .as_ref()
                            .and_then(|data| data.get("retryAfterMs"))
                            .and_then(Value::as_u64);
                        RpcError::Engine(mirri_core::EngineError::ProviderQuotaExhausted {
                            message,
                            retry_after_ms,
                        })
                    }
                    Some("internal") => RpcError::internal(message),
                    Some("not_implemented") => {
                        RpcError::Engine(mirri_core::EngineError::NotImplemented(message))
                    }
                    _ => {
                        if code == mirri_proto::rpc::JSON_RPC_NOT_FOUND {
                            RpcError::NotFound(message)
                        } else if code == mirri_proto::rpc::JSON_RPC_INVALID_PARAMS {
                            RpcError::InvalidParams(message)
                        } else {
                            RpcError::internal(message)
                        }
                    }
                }
            }
            other => RpcError::internal(other.to_string()),
        }
    }
}

// ===========================================================================
// Transport registry
// ===========================================================================

/// Populate the transport registry — the handlers that stay on the server.
///
/// After the M5-B3a thinning exactly 25 handlers are registered here (see
/// the module docs): the 24 server-surface rows of [`mirri_proto::rpc`]
/// (the 23 server-owned rows of the M5-B3a set plus the M7 `mcp.subscribe`
/// row) plus `btw.start` (a `both`-registered method: the embedded engine
/// forks the same side-channel agent, and this handler is the serve wire's
/// own authoritative fork — the cross-backend contract asserts the two
/// shapes agree). Everything else on the serve wire surface is an
/// engine-domain method dispatched by [`ServerState::dispatch`] through the
/// shared [`EngineAdapter`](mirri_transport::adapter::EngineAdapter).
///
/// The one shared mutable registry created here is the in-flight turn map,
/// owned by [`ServerState`](crate::state::ServerState): `session.status` and
/// `session.snapshot` derive their `running`/`busy` flags from it, and the
/// engine fall-through inserts and reaps fire-mode `message.send` entries.
/// The engine adapter is carried into the session read faces too, so their
/// busy/running derivations also union the engine-side in-flight registry
/// (driver-injected turns: parked-notification reactivations, cron).
pub fn build_transport_registry(
    engine: Arc<dyn mirri_transport::adapter::EngineAdapter>,
    session_manager: Arc<SessionManager>,
    server_id: String,
    session_event_channels: Arc<SessionEventChannelRegistry>,
    in_flight_turns: InFlightTurns,
    routing: Arc<dyn SessionRouting>,
    config: Arc<RwLock<ServerConfig>>,
) -> RpcRegistry {
    let handlers: Vec<Arc<dyn RpcHandler>> = vec![
        // Session domain — the transport-owned rows.
        Arc::new(
            GetSessionHandler::new(
                Arc::clone(&session_manager),
                Arc::clone(&in_flight_turns),
                Arc::clone(&session_event_channels),
                server_id.clone().into(),
                Arc::clone(&engine),
            )
            .with_routing(Arc::clone(&routing)),
        ),
        Arc::new(
            RestoreSessionHandler::new(
                Arc::clone(&session_manager),
                Arc::clone(&in_flight_turns),
                Arc::clone(&session_event_channels),
                server_id.clone().into(),
                Arc::clone(&engine),
            )
            .with_routing(Arc::clone(&routing)),
        ),
        Arc::new(
            ChildrenSessionHandler::new(Arc::clone(&session_manager))
                .with_routing(Arc::clone(&routing)),
        ),
        Arc::new(
            StatusSessionHandler::new(
                Arc::clone(&session_manager),
                Arc::clone(&in_flight_turns),
                Arc::clone(&engine),
            )
            .with_routing(Arc::clone(&routing))
            .with_config(Arc::clone(&config)),
        ),
        // Session domain — snapshot (same event log and epoch the WS
        // subscribe path uses, so `as_of_seq`/`epoch` line up with cursors).
        Arc::new(
            SnapshotSessionHandler::new(
                Arc::clone(&session_manager),
                Arc::clone(&session_event_channels),
                Arc::clone(&in_flight_turns),
                server_id.into(),
                engine,
            )
            .with_routing(Arc::clone(&routing)),
        ),
        // Message domain.
        Arc::new(
            ListMessagesHandler::new(Arc::clone(&session_manager))
                .with_routing(Arc::clone(&routing)),
        ),
        // Approval / question domain — resolve and dismiss.
        Arc::new(
            ResolveApprovalHandler::new(Arc::clone(&session_manager))
                .with_routing(Arc::clone(&routing)),
        ),
        Arc::new(
            AnswerQuestionHandler::new(Arc::clone(&session_manager))
                .with_routing(Arc::clone(&routing)),
        ),
        Arc::new(
            DismissQuestionHandler::new(Arc::clone(&session_manager))
                .with_routing(Arc::clone(&routing)),
        ),
        // Agent domain — the transport-owned rows.
        Arc::new(
            ListAgentsHandler::new(Arc::clone(&session_manager)).with_routing(Arc::clone(&routing)),
        ),
        Arc::new(
            GetAgentHandler::new(Arc::clone(&session_manager)).with_routing(Arc::clone(&routing)),
        ),
        // Tool domain — the rich catalog.
        Arc::new(CatalogToolsHandler::new(Arc::clone(&session_manager))),
        // Config domain — the watch stream.
        Arc::new(WatchConfigHandler::new()),
        // MCP domain — the app-level event subscription (M7 F-9). The
        // WebSocket endpoint handles `mcp.subscribe` connection-locally
        // (ws_rpc.rs, the session.subscribe pattern); this shared handler
        // answers the transports with no notification channel.
        Arc::new(McpSubscribeHandler::new()),
        // File domain — list / read / search / grep / git status / diff.
        Arc::new(
            ListFilesHandler::new(Arc::clone(&session_manager)).with_routing(Arc::clone(&routing)),
        ),
        Arc::new(
            ReadFileHandler::new(Arc::clone(&session_manager)).with_routing(Arc::clone(&routing)),
        ),
        Arc::new(
            SearchFilesHandler::new(Arc::clone(&session_manager))
                .with_routing(Arc::clone(&routing)),
        ),
        Arc::new(
            GrepFilesHandler::new(Arc::clone(&session_manager)).with_routing(Arc::clone(&routing)),
        ),
        Arc::new(
            GitStatusFilesHandler::new(Arc::clone(&session_manager))
                .with_routing(Arc::clone(&routing)),
        ),
        Arc::new(
            DiffFilesHandler::new(Arc::clone(&session_manager)).with_routing(Arc::clone(&routing)),
        ),
        // BTW domain — the serve wire's own fork (the embedded engine
        // implements the same fork semantics; see the module docs).
        Arc::new(
            StartBtwHandler::new(Arc::clone(&session_manager)).with_routing(Arc::clone(&routing)),
        ),
    ];
    let mut registry = RpcRegistry::new();
    for handler in handlers {
        registry.register(handler);
    }
    registry
}

#[cfg(test)]
mod tests {
    use base64::Engine;

    use super::*;
    use std::collections::{HashMap, HashSet};

    use mirri_core::{
        AgentId, AgentScopeConfig, ContentPart, ContextMessage, EngineError, Message, PromptOrigin,
        Role, SessionEvent, SessionId, SessionMetadata, SessionScopeConfig, TaskKind, ToolCall,
        ToolCallFunction, WithSkillsEntry, WorkspaceId, WorkspaceScope, WorkspaceScopeConfig,
    };
    use mirri_persistence::AppendLogStore;
    use mirri_proto::rpc::{BackendAvailability, RpcBackend};

    #[test]
    fn should_include_error_id_in_message_and_data_when_internal_error_built() {
        // Given an internal error built through the constructor
        let error = RpcError::internal("database unavailable");
        let RpcError::Internal { message, error_id } = &error else {
            panic!("internal must produce RpcError::Internal");
        };

        // Then the id is embedded in the message and echoed into the envelope
        assert!(message.contains(&format!("[id: {error_id}]")));
        assert!(message.contains("database unavailable"));

        let envelope = ApiErrorResponse::error(&error, "req-123");
        assert_eq!(envelope.request_id, "req-123");
        assert_eq!(envelope.code, -32603);
        assert_eq!(envelope.data, json!({ "error_id": error_id }));
        assert_eq!(envelope.domain_code, None);
    }

    #[test]
    fn session_usage_view_serializes_first_call_in_camel_case() {
        let usage = SessionUsageView {
            first_call: Some(SessionFirstCallView {
                input_tokens: 100,
                output_tokens: 50,
                cache_read_tokens: 30,
                cache_creation_tokens: 5,
            }),
            ..SessionUsageView::from(mirri_core::TokenUsage {
                input_tokens: 300,
                output_tokens: 80,
                cache_read_tokens: 40,
                cache_creation_tokens: 7,
            })
        };
        let json = serde_json::to_value(&usage).unwrap();
        assert_eq!(json["firstCall"]["inputTokens"], 100);
        assert_eq!(json["firstCall"]["cacheReadTokens"], 30);
        assert_eq!(json["inputTokens"], 300);
    }

    #[test]
    fn should_include_domain_code_when_engine_error_surfaced_over_rpc() {
        // Given an engine error converted into an RPC error
        let error = RpcError::from(EngineError::ProviderError("upstream 503".to_string()));

        // When the error envelope is built
        let envelope = ApiErrorResponse::error(&error, "req-123");

        // Then the engine domain code is serialized alongside the JSON-RPC code
        assert_eq!(envelope.code, -32603);
        assert_eq!(envelope.msg, "Provider error: upstream 503");
        assert_eq!(envelope.domain_code.as_deref(), Some("provider.api_error"));
        assert_eq!(envelope.data, Value::Null);
    }

    #[test]
    fn should_keep_data_null_and_no_domain_code_for_ordinary_rpc_errors() {
        // Given a plain application error (not internal, not engine)
        let error = RpcError::NotFound("sess-1".to_string());

        // When the error envelope is built
        let envelope = ApiErrorResponse::error(&error, "req-123");

        // Then no error_id/domain_code decorations are present
        assert_eq!(envelope.code, -32001);
        assert_eq!(envelope.data, Value::Null);
        assert_eq!(envelope.domain_code, None);
    }

    #[test]
    fn should_preserve_session_not_found_as_not_found_jsonrpc_code_with_domain() {
        // Given a session-not-found error wrapped via #[from]
        let error = RpcError::from(mirri_core::SessionError::NotFound("sess-404".to_string()));

        // When the error envelope is built
        let envelope = ApiErrorResponse::error(&error, "req-123");

        // Then the JSON-RPC code is -32001 (NotFound) preserving client
        // compatibility, and the domain code carries the session-specific code
        assert_eq!(envelope.code, -32001);
        assert_eq!(envelope.domain_code.as_deref(), Some("session.not_found"));
    }

    #[test]
    fn should_map_session_internal_error_to_application_jsonrpc_code() {
        // Given a session error that is not NotFound/AgentNotFound
        let error = RpcError::from(mirri_core::SessionError::AlreadyExists(
            "sess-1".to_string(),
        ));

        // When the error envelope is built
        let envelope = ApiErrorResponse::error(&error, "req-123");

        // Then the JSON-RPC code is -32603 (application error) with domain code
        assert_eq!(envelope.code, -32603);
        assert_eq!(
            envelope.domain_code.as_deref(),
            Some("session.already_exists")
        );
    }

    #[test]
    fn should_map_request_invalid_engine_error_to_invalid_params_code() {
        // Given an engine request.invalid error surfaced over RPC
        let error = RpcError::from(EngineError::RequestInvalid("bad payload".to_string()));

        // Then the shared D5 mapping answers -32602 (invalid params) — the
        // same code the embedded engine sends for request.invalid. This
        // closes the registered server divergence (mirri-proto's
        // error.rs ledger): before the M2 session migration the server
        // collapsed every engine error onto -32603.
        assert_eq!(error.json_rpc_code(), -32602);
        assert_eq!(error.domain_code(), Some("request.invalid"));
    }

    #[test]
    fn should_keep_engine_provider_errors_on_the_internal_code() {
        // Given an engine provider error surfaced over RPC
        let error = RpcError::from(EngineError::ProviderError("upstream 503".to_string()));

        // Then the shared D5 mapping still answers -32603 — only
        // request.invalid moved to the client-correctable code.
        assert_eq!(error.json_rpc_code(), -32603);
        assert_eq!(error.domain_code(), Some("provider.api_error"));
    }

    #[test]
    fn should_preserve_adapter_domain_code_when_rpc_error_is_converted_via_from() {
        // Given an adapter-level `EngineError::Rpc` carrying a
        // `session.not_found` domain payload — exactly the shape the embedded
        // engine's `session_error` builder emits for a missing session
        let adapter_error = mirri_transport::adapter::EngineError::Rpc {
            code: mirri_proto::rpc::JSON_RPC_NOT_FOUND,
            message: "session not found: sess-404".to_string(),
            data: Some(json!({ "domainCode": "session.not_found" })),
        };

        // When converted through the shared From impl
        let error = RpcError::from(adapter_error);

        // Then the JSON-RPC code, message, and domain code survive the round
        // trip exactly — nothing is flattened onto a generic internal error
        assert_eq!(error.json_rpc_code(), -32001);
        assert_eq!(error.domain_code(), Some("session.not_found"));
        assert_eq!(
            error.to_string(),
            "Session not found: session not found: sess-404"
        );

        // And the HTTP envelope serializes the same facts the engine emitted
        let envelope = ApiErrorResponse::error(&error, "req-123");
        assert_eq!(envelope.code, -32001);
        assert_eq!(envelope.domain_code.as_deref(), Some("session.not_found"));
        assert_eq!(envelope.data, Value::Null);
    }

    #[test]
    fn should_preserve_the_retry_after_hint_when_a_quota_error_crosses_the_serve_wire() {
        // Given an adapter-level `EngineError::Rpc` carrying the
        // quota-exhausted domain payload with the provider's Retry-After
        // hint — exactly the shape the embedded engine's `engine_error`
        // builder emits (P11a F-P8-13)
        let adapter_error = mirri_transport::adapter::EngineError::Rpc {
            code: mirri_proto::rpc::JSON_RPC_INTERNAL,
            message: "Provider quota exhausted: HTTP 429: top up".to_string(),
            data: Some(json!({
                "domainCode": "provider.quota_exhausted",
                "retryable": false,
                "retryAfterMs": 3000,
            })),
        };

        // When converted through the shared From impl
        let error = RpcError::from(adapter_error);

        // Then the domain code survives (no collapse onto a generic internal
        // error) and the envelope's data carries the hint in milliseconds
        assert_eq!(error.domain_code(), Some("provider.quota_exhausted"));
        let envelope = ApiErrorResponse::error(&error, "req-quota");
        assert_eq!(envelope.code, -32603);
        assert_eq!(
            envelope.domain_code.as_deref(),
            Some("provider.quota_exhausted")
        );
        assert_eq!(
            envelope.data,
            json!({ "retryAfterMs": 3000 }),
            "the Retry-After hint must ride the serve-wire error data"
        );
    }

    #[test]
    fn should_map_adapter_invalid_params_when_rpc_error_carries_32602_without_domain_code() {
        // Given an adapter-level `EngineError::Rpc` carrying the -32602 code
        // and no domain payload (the `invalid_params` builder's shape)
        let adapter_error = mirri_transport::adapter::EngineError::Rpc {
            code: mirri_proto::rpc::JSON_RPC_INVALID_PARAMS,
            message: "input must not be empty".to_string(),
            data: None,
        };

        // When converted through the shared From impl
        let error = RpcError::from(adapter_error);

        // Then the code and message map back to InvalidParams and keep the
        // -32602 wire code (no domain decoration is invented)
        assert!(matches!(error, RpcError::InvalidParams(ref m) if m == "input must not be empty"));
        assert_eq!(error.json_rpc_code(), -32602);
        assert_eq!(error.domain_code(), None);
    }

    #[test]
    fn should_serialize_domain_code_under_the_camelcase_key_when_building_error_envelope() {
        // Given a session-not-found error wrapped via #[from]
        let error = RpcError::from(mirri_core::SessionError::NotFound("sess-404".to_string()));

        // When the HTTP error envelope is serialized
        let envelope = ApiErrorResponse::error(&error, "req-123");
        let serialized = serde_json::to_value(&envelope).expect("envelope serializes");

        // Then the domain code appears under the camelCase `domainCode` key —
        // the D5 shape all three transports now share (WS and IPC carry the
        // same key inside error.data via mirri-proto's domain_error_data).
        assert_eq!(serialized["domainCode"], json!("session.not_found"));
        assert!(
            serialized.get("domain_code").is_none(),
            "no snake_case twin key on the wire"
        );
    }

    /// The M3-c batch-5 alignment: `session.export` answers the v2
    /// `ExportSessionResult` contract — `{zipPath, entries, sessionDir,
    /// manifest}` with a 17-key camelCase manifest (`nodejsVersion`, not
    /// `runtimeVersion`).
    async fn session_manager_with_session() -> (tempfile::TempDir, Arc<SessionManager>, String) {
        let tmp = tempfile::tempdir().expect("tempdir must be creatable");
        let workspace = Arc::new(WorkspaceScope::new(
            WorkspaceId::new("test-ws"),
            tmp.path().to_path_buf(),
            WorkspaceScopeConfig::default(),
        ));
        let manager = Arc::new(SessionManager::new(workspace));
        let now = Utc::now();
        let metadata = SessionMetadata {
            title: "Test Session".to_string(),
            created_at: now,
            updated_at: now,
            tags: Default::default(),
            archived: false,
            is_custom_title: false,
            work_dir: None,
            last_prompt: None,
            forked_from: None,
            cache_root_id: None,
            custom: None,
        };
        let session = manager
            .create_session(metadata, SessionScopeConfig::default())
            .await
            .expect("session must be creatable");
        (tmp, manager, session.id().as_str().to_string())
    }

    /// Given a manager wired with a server-config snapshot whose
    /// `default_model` names a configured alias, and a persisted session
    /// whose wire log carries no model record, when the transport-owned
    /// `session.restore` resumes the session, then the resumed main agent
    /// runs the config snapshot's default model — the R9 no-record fallback
    /// chain equals the fresh-create chain (the same base-config seeding
    /// the embedded resume path performs), never a bare default base whose
    /// empty model would fail the resume loudly.
    #[tokio::test]
    async fn should_seed_the_resume_base_with_the_config_default_model_when_restoring_a_session_without_a_model_record()
     {
        // Given a manager whose wired config snapshot names a default model,
        // and a persisted session whose wire log records a conversation op
        // only — no `config.update` model record anywhere
        let tmp = tempfile::tempdir().expect("tempdir must be creatable");
        let workspace = Arc::new(WorkspaceScope::new(
            WorkspaceId::new("restore-default-model"),
            tmp.path().to_path_buf(),
            WorkspaceScopeConfig::default(),
        ));
        let manager = SessionManager::with_persistence_dir(
            workspace,
            tmp.path().join("sessions"),
            Arc::new(mirri_os::LocalFs::new()),
        )
        .with_server_config(Arc::new(std::sync::RwLock::new(mirri_core::ServerConfig {
            default_model: Some("config-default-model".to_string()),
            ..Default::default()
        })));
        let manager = Arc::new(manager);
        let session_id = SessionId::new("restore-no-model-record");
        manager
            .persistence()
            .save_state(&session_id, &SessionMetadata::default())
            .await
            .expect("the persisted state registers");
        let wire_log_path = manager.persistence().wire_log_path(&session_id);
        let mut log = mirri_persistence::JsonlAppendLog::create(&wire_log_path)
            .await
            .expect("the wire log opens");
        log.append(&WireOp::ContextAppendMessage {
            message: json!({
                "role": "user",
                "content": [{ "type": "text", "text": "hello" }],
                "toolCalls": [],
            }),
        })
        .await
        .expect("the conversation op journals");

        // When the transport-owned session.restore resumes the session
        let channels = Arc::new(SessionEventChannelRegistry::new(256));
        let in_flight: InFlightTurns = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let handler = RestoreSessionHandler::new(
            Arc::clone(&manager),
            in_flight,
            channels,
            "restore-test-server".into(),
            noop_engine(),
        );
        handler
            .handle(json!({ "sessionId": session_id.as_str() }))
            .await
            .expect("restore must resolve the configured default");

        // Then the resumed main agent runs the config snapshot's default
        // model — not the empty model of a bare default base config
        let session = manager
            .get_session(&session_id)
            .await
            .expect("the restored session is live");
        let main = session
            .get_agent(&AgentId::new("main"))
            .await
            .expect("the main agent is recreated");
        assert_eq!(
            main.config()
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .model_id,
            "config-default-model",
            "the restore base must carry the config snapshot's default_model (spec 11 R9)"
        );
    }

    // =======================================================================
    // session.create — main agent provisioning (M3-c batch 6)
    // =======================================================================

    #[tokio::test]
    async fn should_return_ack_with_note_when_watching_config() {
        // Given the server's fixed-at-startup configuration (no config
        // change stream)
        let handler = WatchConfigHandler::new();

        // When watching a config domain
        let value = handler
            .handle(json!({ "domain": "server" }))
            .await
            .expect("config.watch resolves");

        // Then the result is the honest acknowledgement with the
        // explanatory note
        assert_eq!(
            value,
            json!({
                "ok": true,
                "note": "Config watch is not yet supported in mirri-server",
            }),
            "config.watch honest-ack shape must stay ok/note"
        );
    }

    #[tokio::test]
    async fn should_keep_the_tools_wrapper_when_listing_the_tool_catalog() {
        // Given a server with no live sessions
        let tmp = tempfile::tempdir().expect("tempdir must be creatable");
        let workspace = Arc::new(WorkspaceScope::new(
            WorkspaceId::new("test-ws"),
            tmp.path().to_path_buf(),
            WorkspaceScopeConfig::default(),
        ));
        let manager = Arc::new(SessionManager::new(workspace));
        let handler = CatalogToolsHandler::new(Arc::clone(&manager));

        // When listing the tool catalog
        let value = handler
            .handle(json!({}))
            .await
            .expect("tool.catalog resolves");

        // Then the outer `rust:{tools}` wrapper is kept — the TSV authority
        // for this method is RustFirst; the Web client's bare-array
        // expectation was a Web bug, fixed with the batch-⑥ web sync
        let tools = value["tools"].as_array().expect("tools array in wrapper");
        assert!(
            !tools.is_empty(),
            "static builtin rows keep the catalog non-empty"
        );
    }

    #[tokio::test]
    async fn should_list_the_builtin_tools_when_no_session_is_live() {
        // Given a server with no live sessions (a fresh daemon serving the
        // profile editor's config-time tool catalog)
        let tmp = tempfile::tempdir().expect("tempdir must be creatable");
        let workspace = Arc::new(WorkspaceScope::new(
            WorkspaceId::new("test-ws-static-catalog"),
            tmp.path().to_path_buf(),
            WorkspaceScopeConfig::default(),
        ));
        let manager = Arc::new(SessionManager::new(workspace));
        let handler = CatalogToolsHandler::new(Arc::clone(&manager));

        // When listing the tool catalog
        let value = handler
            .handle(json!({}))
            .await
            .expect("tool.catalog resolves");

        // Then the builtin tools are listed without any live session — the
        // catalog is the session-independent directory the profile editor's
        // tool picker reads, so it must not depend on session state
        let tools = value["tools"].as_array().expect("tools array");
        let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
        for expected in ["Read", "Write", "Edit", "Bash", "Grep", "Glob"] {
            assert!(
                names.contains(&expected),
                "builtin tool {expected} missing from catalog: {names:?}"
            );
        }
        let read = tools
            .iter()
            .find(|t| t["name"] == "Read")
            .expect("Read entry");
        assert_eq!(read["source"], "builtin");
        assert!(
            read["description"].as_str().is_some_and(|d| !d.is_empty()),
            "catalog rows carry a description for the config UI"
        );
    }

    // =======================================================================
    // M2 batch ② — typed results and snake_case param aliases
    // =======================================================================

    #[tokio::test]
    async fn should_return_empty_message_page_when_listing_messages_of_agentless_session() {
        // Given a session without agents (no history to page through)
        let (_tmp, manager, session_id) = session_manager_with_session().await;
        let handler = ListMessagesHandler::new(Arc::clone(&manager));

        // When listing the messages with the snake_case `session_id` key
        let value = handler
            .handle(json!({ "session_id": session_id }))
            .await
            .expect("message.list resolves");

        // Then the `rust:{messages,hasMore}` page shape is kept (the TSV
        // authority; the Web client consumes it correctly)
        assert_eq!(value, json!({ "messages": [], "hasMore": false }));
    }

    #[tokio::test]
    async fn should_list_messages_of_a_named_member_agent_when_agent_id_is_given() {
        // Given a session whose first agent (main) holds one message and a
        // second (member) agent holds a different one
        let (_tmp, manager, session_id) = session_manager_with_session().await;
        let session = manager
            .get_session(&SessionId::new(&session_id))
            .await
            .expect("created session is live");
        for (agent_id, text) in [("main", "from main"), ("agent-member", "from member")] {
            session
                .create_agent(AgentId::new(agent_id), AgentScopeConfig::default())
                .await
                .expect("agent must be creatable");
            let agent = session
                .get_agent(&AgentId::new(agent_id))
                .await
                .expect("agent is live");
            let memory = agent.context_memory();
            let mut memory = memory.lock().unwrap_or_else(|e| e.into_inner());
            memory.append_user_message(
                vec![ContentPart::Text {
                    text: text.to_string(),
                }],
                PromptOrigin::User,
            );
        }
        let handler = ListMessagesHandler::new(Arc::clone(&manager));

        // When listing with the member's agent id (snake_case key)
        let value = handler
            .handle(json!({ "session_id": session_id, "agent_id": "agent-member" }))
            .await
            .expect("message.list resolves");

        // Then only the member's history is returned — the member-scoped read
        // face for supervision (US4.1 / architecture §4.2)
        let messages = value["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 1);
        let text = serde_json::to_string(&messages[0]["content"]).expect("content serializes");
        assert!(text.contains("from member"), "unexpected content: {text}");

        // And the camelCase alias routes the same way
        let value = handler
            .handle(json!({ "sessionId": session_id, "agentId": "agent-member" }))
            .await
            .expect("message.list resolves");
        let messages = value["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 1);

        // And an unknown agent id surfaces NotFound, never the main history
        let err = handler
            .handle(json!({ "session_id": session_id, "agent_id": "agent-missing" }))
            .await
            .expect_err("unknown agent must be rejected");
        assert!(matches!(err, RpcError::NotFound(_)));
    }

    #[tokio::test]
    async fn should_carry_origin_metadata_when_listing_messages_of_mixed_history() {
        // Given a session whose first agent holds a real user message and a
        // system-injected reminder (the <tools_added> announcement the
        // injection manager appends between turns)
        let (_tmp, manager, session_id) = session_manager_with_session().await;
        let session = manager
            .get_session(&SessionId::new(&session_id))
            .await
            .expect("created session is live");
        session
            .create_agent(AgentId::new("agent-test"), AgentScopeConfig::default())
            .await
            .expect("agent must be creatable");
        let agent = session
            .get_agent(&AgentId::new("agent-test"))
            .await
            .expect("created agent is live");
        {
            // Two lets: the guard must not borrow the temporary Arc the
            // accessor returns (E0716).
            let memory = agent.context_memory();
            let mut memory = memory.lock().unwrap_or_else(|e| e.into_inner());
            memory.append_user_message(
                vec![ContentPart::Text {
                    text: "hello".to_string(),
                }],
                PromptOrigin::User,
            );
            memory.append_system_reminder(
                "<tools_added>\nBash\n</tools_added>\n\nUse the select_tools tool with exact names."
                    .to_string(),
            );
        }
        let handler = ListMessagesHandler::new(Arc::clone(&manager));

        // When listing the messages
        let value = handler
            .handle(json!({ "session_id": session_id }))
            .await
            .expect("message.list resolves");

        // Then each row carries its provenance under `metadata.origin`
        // (TS toProtocolMessage parity) — the web's transcript filter keys
        // on `metadata.origin.kind` to hide system-injected user turns,
        // so an injection row must never render as real user input
        let messages = value["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["metadata"]["origin"]["kind"], json!("user"));
        assert_eq!(
            messages[1]["metadata"]["origin"]["kind"],
            json!("injection")
        );
    }

    /// A persisted-session fixture whose wire log carries one completed
    /// turn (opener + step.end usage) while the live context memory holds
    /// the matching rows — the shape a live session has after one turn.
    async fn persisted_session_with_one_turn() -> (tempfile::TempDir, Arc<SessionManager>, String) {
        let tmp = tempfile::tempdir().expect("tempdir must be creatable");
        let workspace = Arc::new(WorkspaceScope::new(
            WorkspaceId::new("turn-usage-ws"),
            tmp.path().to_path_buf(),
            WorkspaceScopeConfig::default(),
        ));
        let manager = Arc::new(SessionManager::with_persistence_dir(
            workspace,
            tmp.path().join("sessions"),
            Arc::new(mirri_os::LocalFs::new()),
        ));
        let now = Utc::now();
        let session = manager
            .create_session(
                SessionMetadata {
                    title: "Turn Usage".to_string(),
                    created_at: now,
                    updated_at: now,
                    tags: Default::default(),
                    archived: false,
                    is_custom_title: false,
                    work_dir: None,
                    last_prompt: None,
                    forked_from: None,
                    cache_root_id: None,
                    custom: None,
                },
                SessionScopeConfig::default(),
            )
            .await
            .expect("session must be creatable");
        let session_id = session.id().as_str().to_string();
        session
            .record_op(WireOp::TurnPrompt {
                input: json!("hi"),
                origin: json!({ "kind": "user" }),
            })
            .await;
        session
            .record_op(WireOp::ContextAppendMessage {
                message: json!({
                    "role": "user",
                    "content": "hi",
                    "origin": { "kind": "user" },
                }),
            })
            .await;
        session
            .record_op(WireOp::ContextAppendLoopEvent {
                event: json!({
                    "type": "step.end",
                    "uuid": "step-1",
                    "usage": {
                        "inputTokens": 10,
                        "outputTokens": 5,
                        "cacheReadTokens": 1000,
                        "cacheCreationTokens": 200,
                    },
                }),
            })
            .await;
        session
            .record_op(WireOp::UsageRecord {
                model: "kimi-k2".to_string(),
                usage: json!({
                    "inputTokens": 10,
                    "outputTokens": 5,
                    "cacheReadTokens": 1000,
                    "cacheCreationTokens": 200,
                }),
                usage_scope: None,
            })
            .await;
        session
            .record_op(WireOp::TurnEnded {
                turn_id: 1,
                reason: "completed".to_string(),
                error: None,
                duration_ms: Some(1200),
            })
            .await;
        // The live memory must mirror the wire rows (a live session appends
        // both; `message.list` reads memory while the fold reads the log).
        session
            .create_agent(AgentId::new("main"), AgentScopeConfig::default())
            .await
            .expect("agent must be creatable");
        let agent = session
            .get_agent(&AgentId::new("main"))
            .await
            .expect("created agent is live");
        {
            let memory = agent.context_memory();
            let mut memory = memory.lock().unwrap_or_else(|e| e.into_inner());
            memory.append_user_message(
                vec![ContentPart::Text {
                    text: "hi".to_string(),
                }],
                PromptOrigin::User,
            );
            memory.append_message(ContextMessage::from_message(Message::assistant("hello!")));
        }
        // Mirror the live dual-write for the usage view seed: the turn flow
        // records each step into BOTH the wire log (the `usage.record` op
        // above) and the agent's in-memory usage service, so a snapshot of
        // the same session must find the totals in memory too.
        {
            let usage_service = agent.usage_service();
            let mut service = usage_service.lock().unwrap_or_else(|e| e.into_inner());
            service.record(
                "kimi-k2",
                mirri_core::usage::TokenUsage {
                    input_tokens: 10,
                    output_tokens: 5,
                    cache_read_tokens: 1000,
                    cache_creation_tokens: 200,
                },
                None,
            );
        }
        (tmp, manager, session_id)
    }

    #[tokio::test]
    async fn should_return_turn_usages_and_stamped_rows_when_a_completed_turn_is_listed() {
        // Given a session with one completed, usage-carrying turn
        let (_tmp, manager, session_id) = persisted_session_with_one_turn().await;
        let handler = ListMessagesHandler::new(manager);

        // When listing the messages
        let value = handler
            .handle(json!({ "session_id": session_id }))
            .await
            .expect("message.list resolves");

        // Then the assistant row carries the turn's prompt id
        let messages = value["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], json!("user"));
        assert!(
            messages[0].get("promptId").is_none(),
            "only assistant rows are stamped"
        );
        assert!(
            messages[0].get("durationMs").is_none(),
            "only assistant rows are stamped"
        );
        assert_eq!(messages[1]["role"], json!("assistant"));
        assert_eq!(messages[1]["promptId"], json!("turn-1"));
        assert_eq!(messages[1]["durationMs"], json!(1200));
    }

    #[tokio::test]
    async fn should_stamp_turn_attribution_on_every_page_when_listing_messages_across_pages() {
        // Given a session with one completed, usage-carrying turn whose
        // history spans two pages (the first page holds the newest rows,
        // the second page — requested via beforeId — holds the opener)
        let (_tmp, manager, session_id) = persisted_session_with_one_turn().await;
        let handler = ListMessagesHandler::new(manager);

        // When listing the newest single row
        let first = handler
            .handle(json!({ "session_id": session_id, "pageSize": 1 }))
            .await
            .expect("message.list resolves");
        let rows = first["messages"].as_array().expect("messages array");
        assert_eq!(rows.len(), 1, "the newest page holds one row");
        assert_eq!(rows[0]["role"], json!("assistant"));
        assert_eq!(
            rows[0]["promptId"], json!("turn-1"),
            "the newest page's assistant row carries its turn attribution"
        );
        assert_eq!(first["hasMore"], json!(true));

        // And paging back from the page's oldest anchor re-serves the
        // earlier rows with the same attribution guarantees
        let anchor = rows[0]["id"].as_str().expect("row id").to_string();
        let second = handler
            .handle(json!({ "session_id": session_id, "beforeId": anchor }))
            .await
            .expect("message.list resolves");
        let older = second["messages"].as_array().expect("messages array");
        assert_eq!(older.len(), 1, "the older page holds the user opener");
        assert_eq!(older[0]["role"], json!("user"));
        assert_eq!(
            second["hasMore"], json!(false),
            "the oldest page reports no more rows"
        );
    }

    #[tokio::test]
    async fn should_carry_the_aggregated_usage_and_first_call_when_a_snapshot_is_taken() {
        // Given a session with one completed, usage-carrying turn (the wire
        // log's first `usage.record` is the session's cold-start baseline)
        let (_tmp, manager, session_id) = persisted_session_with_one_turn().await;
        let handler = SnapshotSessionHandler::new(
            manager,
            Arc::new(SessionEventChannelRegistry::new(256)),
            Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            "snapshot-usage-test".into(),
            noop_engine(),
        );

        // When taking the snapshot
        let value = handler
            .handle(json!({ "session_id": session_id }))
            .await
            .expect("session.snapshot resolves");

        // Then the nested session view carries the aggregated four-bucket
        // usage and the first-call baseline (SDR-004) — the same shape the
        // `session.list` rows carry, so a snapshot can seed the usage bar
        // without waiting for the first live status frame
        let usage = &value["session"]["usage"];
        assert_eq!(usage["inputTokens"], json!(10));
        assert_eq!(usage["outputTokens"], json!(5));
        assert_eq!(usage["cacheReadTokens"], json!(1000));
        assert_eq!(usage["cacheCreationTokens"], json!(200));
        let first_call = &usage["firstCall"];
        assert_eq!(first_call["inputTokens"], json!(10));
        assert_eq!(first_call["outputTokens"], json!(5));
        assert_eq!(first_call["cacheReadTokens"], json!(1000));
        assert_eq!(first_call["cacheCreationTokens"], json!(200));
    }

    #[tokio::test]
    async fn should_carry_turn_usages_and_stamped_rows_when_a_snapshot_is_taken_of_a_completed_turn()
     {
        // Given a session with one completed, usage-carrying turn
        let (_tmp, manager, session_id) = persisted_session_with_one_turn().await;
        let handler = SnapshotSessionHandler::new(
            manager,
            Arc::new(SessionEventChannelRegistry::new(256)),
            Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            "turn-usage-test".into(),
            noop_engine(),
        );

        // When taking the snapshot
        let value = handler
            .handle(json!({ "session_id": session_id }))
            .await
            .expect("session.snapshot resolves");

        // Then the envelope carries the folded per-turn usage under the
        // snake_case `turn_usages` key with camelCase inner keys
        let turns = value["turn_usages"].as_array().expect("turn_usages array");
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0]["promptId"], json!("turn-1"));
        assert_eq!(turns[0]["inputTokens"], json!(10));
        assert_eq!(turns[0]["outputTokens"], json!(5));
        assert_eq!(turns[0]["cacheReadTokens"], json!(1000));
        assert_eq!(turns[0]["cacheCreationTokens"], json!(200));
        assert_eq!(turns[0]["models"], json!(["kimi-k2"]));
        assert_eq!(turns[0]["durationMs"], json!(1200));

        // And the assistant row (only) carries the turn's prompt id and the
        // server-measured duration
        let messages = value["messages"]["items"]
            .as_array()
            .expect("snapshot messages items");
        assert_eq!(messages.len(), 2);
        assert!(
            messages[0].get("promptId").is_none(),
            "only assistant rows are stamped"
        );
        assert!(
            messages[0].get("durationMs").is_none(),
            "only assistant rows are stamped"
        );
        assert_eq!(messages[1]["promptId"], json!("turn-1"));
        assert_eq!(messages[1]["durationMs"], json!(1200));
    }

    #[tokio::test]
    async fn should_stamp_the_duration_only_on_the_last_assistant_row_when_a_turn_has_multiple_rows()
    {
        // Given a session whose completed turn holds TWO assistant rows in
        // memory (the wire log still carries one turn with a duration)
        let (_tmp, manager, session_id) = persisted_session_with_one_turn().await;
        let session = manager
            .get_session(&SessionId::new(&session_id))
            .await
            .expect("session is live");
        {
            let agent = session
                .get_agent(&AgentId::new("main"))
                .await
                .expect("main agent is live");
            let memory = agent.context_memory();
            let mut memory = memory.lock().unwrap_or_else(|e| e.into_inner());
            memory.append_message(ContextMessage::from_message(Message::assistant(
                "a second row",
            )));
        }
        let handler = SnapshotSessionHandler::new(
            manager,
            Arc::new(SessionEventChannelRegistry::new(256)),
            Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            "duration-stamp-test".into(),
            noop_engine(),
        );

        // When taking the snapshot
        let value = handler
            .handle(json!({ "session_id": session_id }))
            .await
            .expect("session.snapshot resolves");

        // Then only the turn's LAST assistant row carries the duration — the
        // row whose pill the live transcript shows
        let messages = value["messages"]["items"]
            .as_array()
            .expect("snapshot messages items");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1]["role"], json!("assistant"));
        assert!(
            messages[1].get("durationMs").is_none(),
            "the earlier assistant row stays unstamped: {}",
            messages[1]
        );
        assert_eq!(messages[2]["role"], json!("assistant"));
        assert_eq!(messages[2]["promptId"], json!("turn-1"));
        assert_eq!(messages[2]["durationMs"], json!(1200));
    }

    #[tokio::test]
    async fn should_stamp_current_prompt_id_and_attributed_rows_when_a_snapshot_is_taken_mid_turn()
    {
        // Given a session whose first turn completed and whose second turn
        // is still in flight: the wire log's tail bracket is open (no
        // turn.ended), the live memory already holds the running step's
        // assistant row (the engine commits it before executing the tool),
        // and the in-flight shadow sees the turn on the event bus.
        let (_tmp, manager, session_id) = persisted_session_with_one_turn().await;
        let session = manager
            .get_session(&SessionId::new(&session_id))
            .await
            .expect("session is live");
        session
            .record_op(WireOp::TurnPrompt {
                input: json!("go on"),
                origin: json!({ "kind": "user" }),
            })
            .await;
        session
            .record_op(WireOp::ContextAppendMessage {
                message: json!({
                    "role": "user",
                    "content": "go on",
                    "origin": { "kind": "user" },
                }),
            })
            .await;
        {
            let agent = session
                .get_agent(&AgentId::new("main"))
                .await
                .expect("main agent is live");
            let memory = agent.context_memory();
            let mut memory = memory.lock().unwrap_or_else(|e| e.into_inner());
            memory.append_user_message(
                vec![ContentPart::Text {
                    text: "go on".to_string(),
                }],
                PromptOrigin::User,
            );
            // The running step's row: committed before the tool executes,
            // so the mid-turn snapshot must attribute it to its own turn.
            memory.append_message(ContextMessage::from_message(Message {
                role: Role::Assistant,
                name: None,
                content: Vec::new(),
                tool_calls: vec![ToolCall {
                    id: "call-2".to_string(),
                    call_type: "function".to_string(),
                    function: ToolCallFunction {
                        name: "Task".to_string(),
                        arguments: r#"{"prompt":"Implement F6 closing batch"}"#.to_string(),
                    },
                }],
                tool_call_id: None,
                partial: true,
            }));
        }
        let artifacts = manager
            .session_artifacts(&SessionId::new(&session_id))
            .expect("artifacts installed");
        artifacts.event_bus.publish(SessionEvent::TurnStarted {
            agent_id: "main".to_string(),
            session_id: session_id.clone(),
            turn_id: "turn-<uuid-2>".to_string(),
        });
        artifacts
            .event_bus
            .publish(SessionEvent::AgentMessageChunk {
                agent_id: "main".to_string(),
                text: "F6 收口批进行中".to_string(),
            });
        artifacts.event_bus.publish(SessionEvent::ToolCallStarted {
            agent_id: "main".to_string(),
            session_id: session_id.clone(),
            tool_name: "Task".to_string(),
            tool_call_id: "call-2".to_string(),
            args: json!({ "prompt": "Implement F6 closing batch" }),
        });
        // Let the shadow's fold task consume the published frames.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        let handler = SnapshotSessionHandler::new(
            manager,
            Arc::new(SessionEventChannelRegistry::new(256)),
            Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            "mid-turn-snapshot-test".into(),
            noop_engine(),
        );

        // When taking the snapshot mid-turn
        let value = handler
            .handle(json!({ "session_id": session_id }))
            .await
            .expect("session.snapshot resolves");

        // Then the in-flight turn carries the fold's tail-bracket key as
        // current_prompt_id — the same key the transcript rows below are
        // stamped with, so the client's in-flight seed and the replayed
        // transcript share one turn identity (no duplicate tool card).
        let in_flight = &value["in_flight_turn"];
        assert!(
            in_flight.is_object(),
            "the shadow reported the running turn: {in_flight}"
        );
        assert_eq!(in_flight["current_prompt_id"], json!("turn-2"));
        assert_eq!(
            in_flight["running_tools"][0]["tool_call_id"],
            json!("call-2")
        );

        // And the in-flight turn's assistant row is attributed to its OWN
        // key — not the previous turn's (the mid-turn reload regression).
        let messages = value["messages"]["items"]
            .as_array()
            .expect("snapshot messages items");
        let last = messages.last().expect("assistant row committed pre-tool");
        assert_eq!(last["role"], json!("assistant"));
        assert_eq!(last["promptId"], json!("turn-2"));

        // And no usage record is emitted for the unfinished turn.
        let turns = value["turn_usages"].as_array().expect("turn_usages array");
        assert_eq!(turns.len(), 1, "only the completed turn: {turns:?}");
    }

    /// Serialize a [`Message`] through the shared row builder with a fixed
    /// base timestamp — the read-face shape assertions below pin what
    /// `message.list` and `session.snapshot` rows carry in `content`.
    fn message_row_for(ctx: &ContextMessage, position: usize) -> Value {
        message_list_row(
            ctx,
            "agent-test",
            DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
                .expect("fixed base timestamp")
                .with_timezone(&Utc),
            position,
        )
    }

    #[test]
    fn should_project_with_skills_onto_the_message_row_when_present() {
        // Given a user message carrying a with_skills attachment (the
        // admission stamp a rewritten inline-skill turn receives)
        let mut message = ContextMessage::from_message(Message {
            role: Role::User,
            name: None,
            content: vec![ContentPart::Text {
                text: "run it".to_string(),
            }],
            tool_calls: Vec::new(),
            tool_call_id: None,
            partial: false,
        });
        message.origin = Some(PromptOrigin::User);
        message.with_skills = Some(vec![WithSkillsEntry {
            name: "commit".to_string(),
            target: "git".to_string(),
            trigger: "manual".to_string(),
        }]);

        // When the row is serialized through message_list_row
        let row = message_row_for(&message, 0);

        // Then the row carries a top-level `with_skills` array (snake_case,
        // TS toProtocolMessage parity) alongside `metadata` — the web's
        // MessageRow reads it at the top level, never under metadata
        assert_eq!(
            row["with_skills"],
            json!([{ "name": "commit", "target": "git", "trigger": "manual" }])
        );
        assert!(
            row["metadata"]["with_skills"].is_null(),
            "with_skills must not ride under metadata: {row:?}"
        );
        assert_eq!(row["metadata"]["origin"]["kind"], json!("user"));
    }

    #[test]
    fn should_omit_with_skills_from_the_row_when_the_message_has_none() {
        // Given a plain user message without a with_skills attachment
        let message = ContextMessage::from_message(Message {
            role: Role::User,
            name: None,
            content: vec![ContentPart::Text {
                text: "hello".to_string(),
            }],
            tool_calls: Vec::new(),
            tool_call_id: None,
            partial: false,
        });

        // When the row is serialized through message_list_row
        let row = message_row_for(&message, 0);

        // Then the row carries no `with_skills` key at all
        assert!(
            !row.as_object().expect("row object").contains_key("with_skills"),
            "key must be absent: {row:?}"
        );
    }

    #[test]
    fn should_project_tool_calls_into_tool_use_parts_when_listing_messages() {
        // Given context memory holds an assistant message with one tool call
        // (raw JSON arguments) and the matching tool message with the result
        let assistant = ContextMessage::from_message(Message {
            role: Role::Assistant,
            name: None,
            content: vec![ContentPart::Text {
                text: "\n\n".to_string(),
            }],
            tool_calls: vec![ToolCall {
                id: "call-1".to_string(),
                call_type: "function".to_string(),
                function: ToolCallFunction {
                    name: "Bash".to_string(),
                    arguments: r#"{"command":"pwd"}"#.to_string(),
                },
            }],
            tool_call_id: None,
            partial: false,
        });
        let tool = ContextMessage::from_message(Message {
            role: Role::Tool,
            name: None,
            content: vec![ContentPart::Text {
                text: "out".to_string(),
            }],
            tool_calls: Vec::new(),
            tool_call_id: Some("call-1".to_string()),
            partial: false,
        });

        // When the rows are serialized through message_list_row
        let assistant_row = message_row_for(&assistant, 0);
        let tool_row = message_row_for(&tool, 1);

        // Then the assistant row carries a protocol `tool_use` part (with the
        // parsed JSON input) alongside its text part, and the tool row is a
        // single-element `tool_result` array bound to the same call id
        assert_eq!(
            assistant_row["content"],
            json!([
                { "type": "text", "text": "\n\n" },
                {
                    "type": "tool_use",
                    "tool_call_id": "call-1",
                    "tool_name": "Bash",
                    "input": { "command": "pwd" },
                },
            ])
        );
        assert_eq!(
            tool_row["content"],
            json!([
                { "type": "tool_result", "tool_call_id": "call-1", "output": "out" },
            ])
        );
    }

    #[test]
    fn should_project_think_parts_as_protocol_thinking_when_listing_messages() {
        // Given an assistant message whose content is a single think part
        let message = ContextMessage::from_message(Message {
            role: Role::Assistant,
            name: None,
            content: vec![ContentPart::Think {
                text: "reasoning".to_string(),
            }],
            tool_calls: Vec::new(),
            tool_call_id: None,
            partial: false,
        });

        // When the row is serialized through message_list_row
        let row = message_row_for(&message, 0);

        // Then the row content carries the protocol `thinking` discriminator
        // and field name (not the persisted `think` spelling)
        assert_eq!(
            row["content"],
            json!([{ "type": "thinking", "thinking": "reasoning" }])
        );
    }

    #[test]
    fn should_keep_pure_text_rows_as_plain_strings_when_listing_messages() {
        // Given messages holding only plain text parts (no tool calls, no
        // media, no thinking) for the user and assistant roles
        let user = ContextMessage::from_message(Message::user("hello"));
        let assistant = ContextMessage::from_message(Message::assistant("hi there"));

        // When the rows are serialized through message_list_row
        let user_row = message_row_for(&user, 0);
        let assistant_row = message_row_for(&assistant, 1);

        // Then the row content stays a plain string — the existing read-face
        // guard rail (pure-text rows are never arrays)
        assert_eq!(user_row["content"], json!("hello"));
        assert_eq!(assistant_row["content"], json!("hi there"));
    }

    #[test]
    fn should_map_image_parts_to_protocol_source_shape_when_listing_messages() {
        // Given a user message carrying an image_url part with a remote url
        // and one carrying an inline data: URI
        let remote = ContextMessage::from_message(Message {
            role: Role::User,
            name: None,
            content: vec![ContentPart::ImageUrl {
                image_url: mirri_core::message::ImageUrlData {
                    url: "https://x/a.png".to_string(),
                    detail: None,
                    id: None,
                },
                detail: None,
            }],
            tool_calls: Vec::new(),
            tool_call_id: None,
            partial: false,
        });
        let inline = ContextMessage::from_message(Message {
            role: Role::User,
            name: None,
            content: vec![ContentPart::ImageUrl {
                image_url: mirri_core::message::ImageUrlData {
                    url: "data:image/png;base64,AAAA".to_string(),
                    detail: None,
                    id: None,
                },
                detail: None,
            }],
            tool_calls: Vec::new(),
            tool_call_id: None,
            partial: false,
        });

        // When the rows are serialized through message_list_row
        let remote_row = message_row_for(&remote, 0);
        let inline_row = message_row_for(&inline, 1);

        // Then the image parts map onto the protocol source shapes: a remote
        // url keeps the url kind, a data: URI degrades to the base64 kind
        // with its media type split out of the URI
        assert_eq!(
            remote_row["content"],
            json!([
                { "type": "image", "source": { "kind": "url", "url": "https://x/a.png" } },
            ])
        );
        assert_eq!(
            inline_row["content"],
            json!([
                {
                    "type": "image",
                    "source": { "kind": "base64", "media_type": "image/png", "data": "AAAA" },
                },
            ])
        );
    }

    #[test]
    fn should_project_file_parts_with_a_file_id_into_protocol_file_parts_when_listing_messages() {
        // Given a user message carrying the inbound wire form of a file part:
        // a files-store reference with no resolved media type
        let message = ContextMessage::from_message(Message {
            role: Role::User,
            name: None,
            content: vec![ContentPart::File {
                file_id: Some("file-1".to_string()),
                data: None,
                name: Some("doc.pdf".to_string()),
                media_type: None,
                size: Some(42),
            }],
            tool_calls: Vec::new(),
            tool_call_id: None,
            partial: false,
        });

        // When the row is serialized through message_list_row
        let row = message_row_for(&message, 0);

        // Then the part is a protocol `file` part bound to the store id; the
        // unknown media type falls back to the generic binary type (the
        // protocol `media_type` must be a non-empty string)
        assert_eq!(
            row["content"],
            json!([
                {
                    "type": "file",
                    "file_id": "file-1",
                    "name": "doc.pdf",
                    "media_type": "application/octet-stream",
                    "size": 42,
                },
            ])
        );
    }

    #[test]
    fn should_degrade_inline_image_file_data_into_a_base64_image_source_when_listing_messages() {
        // Given the context-memory inline form of a file part carrying image
        // bytes (a data: URI, no store id)
        let message = ContextMessage::from_message(Message {
            role: Role::User,
            name: None,
            content: vec![ContentPart::File {
                file_id: None,
                data: Some("data:image/png;base64,AAAA".to_string()),
                name: Some("a.png".to_string()),
                media_type: None,
                size: None,
            }],
            tool_calls: Vec::new(),
            tool_call_id: None,
            partial: false,
        });

        // When the row is serialized through message_list_row
        let row = message_row_for(&message, 0);

        // Then the part rides the protocol `image` part's base64 source (the
        // `file` part requires a store id the inline form does not carry)
        assert_eq!(
            row["content"],
            json!([
                {
                    "type": "image",
                    "source": { "kind": "base64", "media_type": "image/png", "data": "AAAA" },
                },
            ])
        );
    }

    #[test]
    fn should_degrade_inline_non_image_file_data_into_a_text_marker_when_listing_messages() {
        // Given the context-memory inline form of a non-image file part
        let message = ContextMessage::from_message(Message {
            role: Role::User,
            name: None,
            content: vec![ContentPart::File {
                file_id: None,
                data: Some("data:application/pdf;base64,BBBB".to_string()),
                name: Some("doc.pdf".to_string()),
                media_type: None,
                size: None,
            }],
            tool_calls: Vec::new(),
            tool_call_id: None,
            partial: false,
        });

        // When the row is serialized through message_list_row
        let row = message_row_for(&message, 0);

        // Then the part degrades to the `[file:…]` text marker (same policy
        // as the audio part): the protocol has no home for inline non-image
        // file bytes, and the part must stay inside the content union
        assert_eq!(
            row["content"],
            json!([{ "type": "text", "text": "[file:doc.pdf]" }])
        );
    }

    #[test]
    fn should_not_emit_a_tool_result_part_when_the_tool_call_id_is_missing_when_listing_messages() {
        // Given a tool-role message with no correlation id (a legacy wire
        // record) holding a plain-text result
        let message = ContextMessage::from_message(Message {
            role: Role::Tool,
            name: None,
            content: vec![ContentPart::Text {
                text: "out".to_string(),
            }],
            tool_calls: Vec::new(),
            tool_call_id: None,
            partial: false,
        });

        // When the row is serialized through message_list_row
        let row = message_row_for(&message, 0);

        // Then the row degrades to its plain-text form — a protocol
        // `tool_result` requires a non-empty tool_call_id, and an empty one
        // would pass the schema while breaking the call association
        assert_eq!(row["content"], json!("out"));
    }

    #[test]
    fn should_carry_the_error_flag_on_tool_result_rows_when_listing_messages() {
        // Given a tool-role message flagged as an error result
        let mut message = ContextMessage::from_message(Message {
            role: Role::Tool,
            name: None,
            content: vec![ContentPart::Text {
                text: "boom".to_string(),
            }],
            tool_calls: Vec::new(),
            tool_call_id: Some("call-9".to_string()),
            partial: false,
        });
        message.is_error = true;

        // When the row is serialized through message_list_row
        let row = message_row_for(&message, 0);

        // Then the tool_result part carries `is_error: true` so the web can
        // render the failure styling (ADR-007)
        assert_eq!(
            row["content"],
            json!([
                {
                    "type": "tool_result",
                    "tool_call_id": "call-9",
                    "output": "boom",
                    "is_error": true,
                },
            ])
        );
    }

    #[test]
    fn should_map_video_and_audio_parts_to_protocol_shapes_when_listing_messages() {
        // Given a user message carrying a video url part and an audio url part
        let message = ContextMessage::from_message(Message {
            role: Role::User,
            name: None,
            content: vec![
                ContentPart::VideoUrl {
                    video_url: "https://x/v.mp4".to_string(),
                },
                ContentPart::AudioUrl {
                    audio_url: "https://x/a.mp3".to_string(),
                },
            ],
            tool_calls: Vec::new(),
            tool_call_id: None,
            partial: false,
        });

        // When the row is serialized through message_list_row
        let row = message_row_for(&message, 0);

        // Then the video part maps onto the protocol video part's url source,
        // and the audio part (no protocol home) degrades to the TS projector's
        // text marker spelling
        assert_eq!(
            row["content"],
            json!([
                { "type": "video", "source": { "kind": "url", "url": "https://x/v.mp4" } },
                { "type": "text", "text": "[audio:https://x/a.mp3]" },
            ])
        );
    }

    #[tokio::test]
    async fn should_return_bare_agent_array_when_listing_agents_of_session_without_agents() {
        // Given a session without agents
        let (_tmp, manager, session_id) = session_manager_with_session().await;
        let handler = ListAgentsHandler::new(Arc::clone(&manager));

        // When listing the session's agents with the snake_case
        // `session_id` key
        let value = handler
            .handle(json!({ "session_id": session_id }))
            .await
            .expect("agent.list resolves");

        // Then the result is the bare empty `rust:AgentInfo[]` array
        assert_eq!(value, json!([]));
    }

    #[tokio::test]
    async fn should_return_not_found_when_getting_agent_of_unknown_session() {
        // Given no session under the requested id
        let (_tmp, manager, _) = session_manager_with_session().await;
        let handler = GetAgentHandler::new(Arc::clone(&manager));

        // When getting an agent of an unknown session (snake_case keys)
        let result = handler
            .handle(json!({ "session_id": "session-missing", "agent_id": "agent-1" }))
            .await;

        // Then the request fails with NotFound
        assert!(matches!(result, Err(RpcError::NotFound(_))));
    }

    #[tokio::test]
    async fn should_dismiss_pending_question_when_dismissing_known_request_id() {
        // Given a session with one pending question (published on its bus)
        let (_tmp, manager, session_id) = session_manager_with_session().await;
        let artifacts = manager
            .session_artifacts(&SessionId::new(&session_id))
            .expect("created session has artifacts");
        let mut rx = artifacts.event_bus.subscribe();
        let gate = Arc::clone(&artifacts.question_gate);
        let asker = tokio::spawn(async move { gate.ask("continue?").await });
        let request_id = match rx.recv().await.expect("question asked event") {
            SessionEvent::QuestionAsked { request_id, .. } => request_id,
            other => panic!("expected QuestionAsked, got {other:?}"),
        };
        let handler = DismissQuestionHandler::new(Arc::clone(&manager));

        // When dismissing it through the handler
        let result = handler
            .handle(json!({ "sessionId": session_id, "requestId": request_id.to_string() }))
            .await;

        // Then the dismissal is acknowledged and the awaiting ask resolves to None
        assert_eq!(
            result.expect("dismiss resolves"),
            json!({ "ok": true, "dismissed": true })
        );
        assert_eq!(asker.await.expect("ask task joins"), None);
    }

    #[tokio::test]
    async fn should_return_not_found_when_dismissing_unknown_request_id() {
        // Given a session with no question under the requested id
        let (_tmp, manager, session_id) = session_manager_with_session().await;
        let handler = DismissQuestionHandler::new(Arc::clone(&manager));

        // When dismissing an unregistered request id
        let result = handler
            .handle(json!({ "sessionId": session_id, "requestId": Uuid::new_v4().to_string() }))
            .await;

        // Then the request fails with NotFound
        assert!(matches!(result, Err(RpcError::NotFound(_))));
    }

    #[tokio::test]
    async fn should_return_not_found_when_dismissing_question_of_unknown_session() {
        // Given no session under the requested id
        let (_tmp, manager, _) = session_manager_with_session().await;
        let handler = DismissQuestionHandler::new(Arc::clone(&manager));

        // When dismissing a question of an unknown session
        let result = handler
            .handle(json!({
                "sessionId": "session-missing",
                "requestId": Uuid::new_v4().to_string(),
            }))
            .await;

        // Then the request fails with NotFound
        assert!(matches!(result, Err(RpcError::NotFound(_))));
    }

    // =======================================================================
    // M2 batch ④ — typed shapes and snake_case param aliases
    // (approval / question / workspace)
    // =======================================================================

    #[tokio::test]
    async fn should_return_typed_ok_ack_when_resolving_a_pending_approval_with_snake_case_keys() {
        // Given a session with one pending approval (published on its bus)
        let (_tmp, manager, session_id) = session_manager_with_session().await;
        let artifacts = manager
            .session_artifacts(&SessionId::new(&session_id))
            .expect("created session has artifacts");
        let mut rx = artifacts.event_bus.subscribe();
        let gate = Arc::clone(&artifacts.approval_gate);
        let asker = tokio::spawn(async move { gate.request("test tool", "summary").await });
        let request_id = match rx.recv().await.expect("approval requested event") {
            SessionEvent::ToolApprovalRequested { request_id, .. } => request_id,
            other => panic!("expected ToolApprovalRequested, got {other:?}"),
        };
        let handler = ResolveApprovalHandler::new(Arc::clone(&manager));

        // When resolving it with the snake_case session_id/request_id keys
        // the Web client's RPC layer sends
        let value = handler
            .handle(json!({
                "session_id": session_id,
                "request_id": request_id.to_string(),
                "approved": true,
            }))
            .await
            .expect("approval.decide resolves");

        // Then the typed `{"ok": true}` ack is returned and the awaiting
        // requester wakes with the approval
        assert_eq!(value, json!({ "ok": true }));
        assert!(matches!(
            asker.await.expect("ask task joins"),
            mirri_core::ApprovalDecision::Approved
        ));
    }

    #[tokio::test]
    async fn should_deliver_session_scoped_variant_when_resolving_with_scope_session() {
        // Given a session with one pending approval (published on its bus)
        let (_tmp, manager, session_id) = session_manager_with_session().await;
        let artifacts = manager
            .session_artifacts(&SessionId::new(&session_id))
            .expect("created session has artifacts");
        let mut rx = artifacts.event_bus.subscribe();
        let gate = Arc::clone(&artifacts.approval_gate);
        let asker = tokio::spawn(async move { gate.request("test tool", "summary").await });
        let request_id = match rx.recv().await.expect("approval requested event") {
            SessionEvent::ToolApprovalRequested { request_id, .. } => request_id,
            other => panic!("expected ToolApprovalRequested, got {other:?}"),
        };
        let handler = ResolveApprovalHandler::new(Arc::clone(&manager));

        // When resolving it as approved with the klient `scope: "session"`
        // field (the always-allow upgrade)
        let value = handler
            .handle(json!({
                "sessionId": session_id,
                "requestId": request_id.to_string(),
                "approved": true,
                "scope": "session",
            }))
            .await
            .expect("approval.decide resolves");

        // Then the typed `{"ok": true}` ack is returned and the gate delivers
        // the session-scoped decision (not the one-off `Approved`) to the
        // awaiting requester — embedded-side parity
        assert_eq!(value, json!({ "ok": true }));
        assert!(matches!(
            asker.await.expect("ask task joins"),
            mirri_core::ApprovalDecision::ApprovedForSession
        ));
    }

    #[tokio::test]
    async fn should_return_not_found_when_resolving_an_unknown_approval_with_camel_case_keys() {
        // Given a session with no pending approval under the requested id
        let (_tmp, manager, session_id) = session_manager_with_session().await;
        let handler = ResolveApprovalHandler::new(Arc::clone(&manager));

        // When resolving an unknown request id with the canonical
        // camelCase keys (both spellings must stay accepted)
        let result = handler
            .handle(json!({
                "sessionId": session_id,
                "requestId": Uuid::new_v4().to_string(),
                "approved": false,
            }))
            .await;

        // Then the request fails with NotFound (the params parsed fine;
        // only the request id is unknown)
        assert!(matches!(result, Err(RpcError::NotFound(_))));
    }

    #[tokio::test]
    async fn should_return_typed_ok_ack_when_answering_a_pending_question() {
        // Given a session with one pending question (published on its bus)
        let (_tmp, manager, session_id) = session_manager_with_session().await;
        let artifacts = manager
            .session_artifacts(&SessionId::new(&session_id))
            .expect("created session has artifacts");
        let mut rx = artifacts.event_bus.subscribe();
        let gate = Arc::clone(&artifacts.question_gate);
        let asker = tokio::spawn(async move { gate.ask("continue?").await });
        let request_id = match rx.recv().await.expect("question asked event") {
            SessionEvent::QuestionAsked { request_id, .. } => request_id,
            other => panic!("expected QuestionAsked, got {other:?}"),
        };
        let handler = AnswerQuestionHandler::new(Arc::clone(&manager));

        // When answering it with the canonical camelCase keys
        let value = handler
            .handle(json!({
                "sessionId": session_id,
                "requestId": request_id.to_string(),
                "answer": "yes",
            }))
            .await
            .expect("question.answer resolves");

        // Then the typed `{"ok": true}` ack is returned and the awaiting
        // ask resolves to the answer
        assert_eq!(value, json!({ "ok": true }));
        assert_eq!(
            asker.await.expect("ask task joins"),
            Some("yes".to_string())
        );
    }

    #[tokio::test]
    async fn should_return_not_found_when_answering_an_unknown_question_with_snake_case_keys() {
        // Given a session with no question under the requested id
        let (_tmp, manager, session_id) = session_manager_with_session().await;
        let handler = AnswerQuestionHandler::new(Arc::clone(&manager));

        // When answering an unknown request id with the snake_case
        // session_id/request_id keys the Web client's RPC layer sends
        let result = handler
            .handle(json!({
                "session_id": session_id,
                "request_id": Uuid::new_v4().to_string(),
                "answer": "yes",
            }))
            .await;

        // Then the request fails with NotFound (the params parsed fine;
        // only the request id is unknown)
        assert!(matches!(result, Err(RpcError::NotFound(_))));
    }

    #[tokio::test]
    async fn should_dismiss_pending_question_when_dismissing_with_snake_case_keys() {
        // Given a session with one pending question (published on its bus)
        let (_tmp, manager, session_id) = session_manager_with_session().await;
        let artifacts = manager
            .session_artifacts(&SessionId::new(&session_id))
            .expect("created session has artifacts");
        let mut rx = artifacts.event_bus.subscribe();
        let gate = Arc::clone(&artifacts.question_gate);
        let asker = tokio::spawn(async move { gate.ask("continue?").await });
        let request_id = match rx.recv().await.expect("question asked event") {
            SessionEvent::QuestionAsked { request_id, .. } => request_id,
            other => panic!("expected QuestionAsked, got {other:?}"),
        };
        let handler = DismissQuestionHandler::new(Arc::clone(&manager));

        // When dismissing it with the snake_case session_id/request_id keys
        // the Web client's RPC layer sends
        let result = handler
            .handle(json!({ "session_id": session_id, "request_id": request_id.to_string() }))
            .await;

        // Then the typed `{ok, dismissed}` ack is returned and the awaiting
        // ask resolves to None
        assert_eq!(
            result.expect("dismiss resolves"),
            json!({ "ok": true, "dismissed": true })
        );
        assert_eq!(asker.await.expect("ask task joins"), None);
    }

    // =======================================================================
    // session.snapshot
    // =======================================================================

    /// Given a session whose activity fold has recorded a completed main
    /// turn, When `session.snapshot` is taken, Then the session sub-view
    /// carries the work-facts tuple (US4.4/SDR-013) — the refreshed
    /// sidebar restores the indicator from the snapshot without waiting
    /// for a live work_changed frame.
    #[tokio::test]
    async fn should_carry_work_facts_on_the_snapshot_session_view() {
        let (_tmp, manager, session_id) = session_manager_with_session().await;
        let session = manager
            .get_session(&SessionId::new(&session_id))
            .await
            .expect("created session is live");
        let artifacts = manager
            .session_artifacts(&SessionId::new(&session_id))
            .expect("live session has artifacts");
        // Drive the fold the way the engine would: a main turn that
        // starts and completes. The shadow publishes its transitions on
        // the same bus; the snapshot reads the fold's state.
        artifacts
            .event_bus
            .publish(mirri_core::SessionEvent::TurnStarted {
                agent_id: "main".to_string(),
                session_id: session_id.clone(),
                turn_id: "turn-1".to_string(),
            });
        artifacts
            .event_bus
            .publish(mirri_core::SessionEvent::TurnCompleted {
                agent_id: "main".to_string(),
                session_id: session_id.clone(),
                turn_id: "turn-1".to_string(),
                stop_reason: "completed".to_string(),
                wire_turn_id: 1,
                duration_ms: None,
            });
        // Let the shadow's fold task consume both frames.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(session);

        let event_channels = Arc::new(SessionEventChannelRegistry::new(256));
        let handler = snapshot_handler(&manager, &event_channels);

        let value = handler
            .handle(json!({ "sessionId": session_id }))
            .await
            .expect("snapshot resolves");

        let session_view = &value["session"];
        assert_eq!(session_view["busy"], json!(false));
        assert_eq!(session_view["mainTurnActive"], json!(false));
        assert_eq!(session_view["pendingInteraction"], json!("none"));
        assert_eq!(session_view["lastTurnReason"], json!("completed"));
    }

    /// Given a session with a pending tool approval, When `session.snapshot`
    /// is taken, Then the session sub-view reports the pending-interaction
    /// kind — the refreshed sidebar can badge the row without opening it.
    #[tokio::test]
    async fn should_carry_pending_interaction_kind_on_the_snapshot_session_view() {
        let (_tmp, manager, session_id) = session_manager_with_session().await;
        let session = manager
            .get_session(&SessionId::new(&session_id))
            .await
            .expect("created session is live");
        session
            .create_agent(AgentId::new("main"), AgentScopeConfig::default())
            .await
            .expect("main agent must be creatable");
        let artifacts = manager
            .session_artifacts(&SessionId::new(&session_id))
            .expect("live session has artifacts");
        artifacts
            .event_bus
            .publish(mirri_core::SessionEvent::ToolApprovalRequested {
                agent_id: uuid::Uuid::new_v4(),
                tool_name: "Bash".to_string(),
                request_id: uuid::Uuid::new_v4(),
            });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(session);

        let event_channels = Arc::new(SessionEventChannelRegistry::new(256));
        let handler = snapshot_handler(&manager, &event_channels);

        let value = handler
            .handle(json!({ "sessionId": session_id }))
            .await
            .expect("snapshot resolves");

        let session_view = &value["session"];
        assert_eq!(
            session_view["pendingInteraction"],
            json!("approval"),
            "a pending tool approval rides the snapshot's session view"
        );
    }

    /// A [`SnapshotSessionHandler`] over `manager` with an empty in-flight
    /// turn map and a fixed epoch, sharing the given event-channel registry.
    fn snapshot_handler(
        manager: &Arc<SessionManager>,
        event_channels: &Arc<SessionEventChannelRegistry>,
    ) -> SnapshotSessionHandler {
        let in_flight_turns: InFlightTurns = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        SnapshotSessionHandler::new(
            Arc::clone(manager),
            Arc::clone(event_channels),
            in_flight_turns,
            "test-epoch".to_string().into(),
            noop_engine(),
        )
    }

    #[tokio::test]
    async fn should_include_member_agents_in_the_snapshot_subagents_roster() {
        // Given a session with a main agent and two member agents, each member
        // registered as an Agent-kind task parented at one swarm tool call
        let (_tmp, manager, session_id) = session_manager_with_session().await;
        let session = manager
            .get_session(&SessionId::new(&session_id))
            .await
            .expect("created session is live");
        session
            .create_agent(AgentId::new("main"), AgentScopeConfig::default())
            .await
            .expect("main agent must be creatable");
        for (agent_id, item, index) in [("agent-a", "scaffold", 0u32), ("agent-b", "tests", 1)] {
            session
                .create_agent(AgentId::new(agent_id), AgentScopeConfig::default())
                .await
                .expect("member agent must be creatable");
            // The identity source the roster joins onto: the same op the
            // subagent host records on spawn (parent linkage included).
            session
                .record_op(WireOp::SubagentRegistered {
                    agent_id: agent_id.to_string(),
                    profile_name: "coder".to_string(),
                    model: Some("fast".to_string()),
                    thinking_effort: None,
                    parent_agent_id: Some("main".to_string()),
                    swarm_item: Some(item.to_string()),
                    parent_tool_call_id: Some("call-swarm".to_string()),
                    swarm_index: Some(index),
                })
                .await;
        }
        let tasks = session.tasks();
        tasks.register(
            TaskKind::Agent {
                agent_id: Some("agent-a".to_string()),
                subagent_type: Some("coder".to_string()),
                parent_tool_call_id: Some("call-swarm".to_string()),
                swarm_index: Some(0),
                model: None,
            },
            "scaffold".to_string(),
            session_id.clone(),
        );
        // agent-b stays registry-only — the swarm-member shape (no task
        // record; the roster falls back to the honest non-running status).
        let event_channels = Arc::new(SessionEventChannelRegistry::new(256));
        let handler = snapshot_handler(&manager, &event_channels);

        // When snapshotting the session
        let value = handler
            .handle(json!({ "sessionId": session_id }))
            .await
            .expect("snapshot resolves");

        // Then the roster carries both members keyed by agent id (matching the
        // WS subagent rows), with honest status/phase and the parent linkage —
        // and never the main agent (supervision US4.1 / architecture §4.2)
        let roster = value["subagents"].as_array().expect("subagents array");
        assert_eq!(
            roster.len(),
            2,
            "roster must carry both members: {roster:?}"
        );
        let ids: Vec<&str> = roster.iter().filter_map(|r| r["id"].as_str()).collect();
        assert!(
            ids.contains(&"agent-a") && ids.contains(&"agent-b"),
            "ids: {ids:?}"
        );
        assert!(!ids.contains(&"main"), "main must never be in the roster");
        for row in roster {
            assert_eq!(row["session_id"], json!(session_id));
            assert_eq!(row["kind"], json!("subagent"));
            assert_eq!(row["parent_tool_call_id"], json!("call-swarm"));
            assert!(row["swarm_index"].is_number());
            assert!(row["description"].is_string());
            // agent-a has a live task record — running/working; agent-b is
            // registry-only (the swarm-member shape) — settled, outcome not
            // derivable, honest non-running status (never a fake "running").
            let expected = if row["id"] == json!("agent-a") {
                ("running", "working")
            } else {
                ("cancelled", "failed")
            };
            assert_eq!(row["status"], json!(expected.0), "row: {row:?}");
            assert_eq!(row["subagent_phase"], json!(expected.1), "row: {row:?}");
        }
    }

    #[tokio::test]
    async fn should_return_snapshot_with_messages_when_snapshotting_session_with_agent() {
        // Given a session whose first agent has a message history
        let (_tmp, manager, session_id) = session_manager_with_session().await;
        let session = manager
            .get_session(&SessionId::new(&session_id))
            .await
            .expect("created session is live");
        session
            .create_agent(AgentId::new("agent-test"), AgentScopeConfig::default())
            .await
            .expect("agent must be creatable");
        let agent = session
            .get_agent(&AgentId::new("agent-test"))
            .await
            .expect("created agent is live");
        agent
            .context_memory()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .append_user_message(
                vec![ContentPart::Text {
                    text: "hello".to_string(),
                }],
                PromptOrigin::User,
            );
        let event_channels = Arc::new(SessionEventChannelRegistry::new(256));
        let handler = snapshot_handler(&manager, &event_channels);

        // When snapshotting the session
        let value = handler
            .handle(json!({ "sessionId": session_id }))
            .await
            .expect("snapshot resolves");

        // Then the snapshot carries the watermark, the session view with one
        // agent and one message, and the full message history
        assert_eq!(value["as_of_seq"], json!(0), "no durable events yet");
        assert_eq!(value["epoch"], json!("test-epoch"));
        let session_view = &value["session"];
        assert_eq!(session_view["id"], json!(session_id));
        assert_eq!(session_view["title"], json!("Test Session"));
        assert_eq!(session_view["busy"], json!(false));
        assert_eq!(session_view["agentCount"], json!(1));
        assert_eq!(
            session_view["model"],
            json!(
                agent
                    .config()
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .model_id
            )
        );
        assert_eq!(session_view["messageCount"], json!(1));
        let items = value["messages"]["items"]
            .as_array()
            .expect("messages items is an array");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["id"], json!("msg-0"));
        assert_eq!(items[0]["role"], json!("user"));
        assert_eq!(items[0]["content"], json!("hello"));
        assert_eq!(items[0]["agentId"], json!("agent-test"));
        // Provenance rides under `metadata.origin` (the same row shape as
        // `message.list`) so clients can filter system-injected turns
        assert_eq!(items[0]["metadata"]["origin"]["kind"], json!("user"));
        assert_eq!(value["messages"]["has_more"], json!(false));
        assert_eq!(value["in_flight_turn"], Value::Null);
        assert_eq!(value["subagents"], json!([]));
        assert_eq!(value["pending_approvals"], json!([]));
        assert_eq!(value["pending_questions"], json!([]));
    }

    /// Given a session with a pending approval that carries the engine
    /// approval-rule facet (and one without), When `session.snapshot` is
    /// taken, Then each `pending_approvals` row carries the facet under
    /// `approvalRule` (only when the snapshot carries one — the klient
    /// camelCase spelling, matching the `approval.listPending` /
    /// interaction-kernel rows) and an honest `expiresAt`: `null` when the
    /// engine derives no expiry (the rust gate never fabricates a value).
    #[tokio::test]
    async fn should_carry_approval_rule_and_honest_expires_at_when_snapshotting_pending_approvals()
    {
        // Given a session with two pending approvals: one carrying the
        // engine approval-rule facet, one without
        let (_tmp, manager, session_id) = session_manager_with_session().await;
        let artifacts = manager
            .session_artifacts(&SessionId::new(&session_id))
            .expect("created session has artifacts");
        let (ruled_id, _ruled_rx) =
            artifacts
                .approval_gate
                .register(mirri_core::session::gates::ApprovalSnapshot {
                    tool_name: "Bash".to_string(),
                    action: "execute Bash".to_string(),
                    display: json!({ "summary": "echo hi" }),
                    agent_id: Uuid::nil(),
                    turn_id: Some(3),
                    tool_call_id: Some("call_ruled".to_string()),
                    created_at: 1_000,
                    approval_rule: Some("Bash".to_string()),
                    grant_dir: None,
                });
        let (bare_id, _bare_rx) =
            artifacts
                .approval_gate
                .register(mirri_core::session::gates::ApprovalSnapshot {
                    tool_name: "Read".to_string(),
                    action: "execute Read".to_string(),
                    display: json!({}),
                    agent_id: Uuid::nil(),
                    turn_id: Some(3),
                    tool_call_id: Some("call_bare".to_string()),
                    created_at: 1_001,
                    approval_rule: None,
                    grant_dir: None,
                });
        let event_channels = Arc::new(SessionEventChannelRegistry::new(256));
        let handler = snapshot_handler(&manager, &event_channels);

        // When snapshotting the session
        let value = handler
            .handle(json!({ "sessionId": session_id }))
            .await
            .expect("snapshot resolves");

        // Then both pending approvals appear, the facet riding the row that
        // registered one (absent ≠ empty: no facet on the snapshot means no
        // approvalRule on the row)
        let pending = value["pending_approvals"]
            .as_array()
            .expect("pending approvals is an array");
        assert_eq!(pending.len(), 2);
        let ruled = pending
            .iter()
            .find(|row| row["approval_id"] == json!(ruled_id.to_string()))
            .expect("the ruled row appears in the snapshot");
        assert_eq!(
            ruled["approvalRule"],
            json!("Bash"),
            "the snapshot row carries the facet it gated on"
        );
        let bare = pending
            .iter()
            .find(|row| row["approval_id"] == json!(bare_id.to_string()))
            .expect("the bare row appears in the snapshot");
        assert!(
            bare.get("approvalRule").is_none(),
            "no facet on the snapshot — no approvalRule on the row"
        );

        // And the expiry is honest: every row carries `expiresAt` as an
        // explicit null (the engine gate derives no expiry — never a
        // fabricated timestamp)
        for row in pending {
            assert_eq!(
                row["expiresAt"],
                Value::Null,
                "expiresAt must be null when the engine derives no expiry"
            );
        }
    }

    /// Given a session with a member agent (`agent-<uuid>`) and pending
    /// approvals — one registered by that member (snapshot `agent_id` = the
    /// member's uuid) and one unattributed (the main agent's nil), When
    /// `session.snapshot` is taken, Then the member's row carries `agent_id`
    /// naming the member (the full agent id — the same key the subagents
    /// roster and the WS `subagent.*` rows use) while the unattributed row
    /// omits the field (single-agent sessions stay unchanged) — and a
    /// snapshot agent uuid that names no registered agent is honestly
    /// dropped rather than fabricated (supervision B1 / US5.3).
    #[tokio::test]
    async fn should_carry_the_requesting_member_identity_on_pending_approval_rows() {
        // Given a session with the main agent and one member agent whose id
        // follows the subagent namespace (`agent-<uuid>`, the form the host
        // mints at spawn)
        let (_tmp, manager, session_id) = session_manager_with_session().await;
        let session = manager
            .get_session(&SessionId::new(&session_id))
            .await
            .expect("created session is live");
        session
            .create_agent(AgentId::new("main"), AgentScopeConfig::default())
            .await
            .expect("main agent must be creatable");
        let member_uuid = Uuid::new_v4();
        let member_id = format!("agent-{member_uuid}");
        session
            .create_agent(AgentId::new(&member_id), AgentScopeConfig::default())
            .await
            .expect("member agent must be creatable");

        let artifacts = manager
            .session_artifacts(&SessionId::new(&session_id))
            .expect("created session has artifacts");
        // The member's approval: the gate snapshot's `agent_id` is the
        // member's uuid (the form the turn-flow seam records).
        let (member_row_id, _member_rx) =
            artifacts
                .approval_gate
                .register(mirri_core::session::gates::ApprovalSnapshot {
                    tool_name: "Bash".to_string(),
                    action: "execute Bash".to_string(),
                    display: json!({ "summary": "cargo test" }),
                    agent_id: member_uuid,
                    turn_id: Some(1),
                    tool_call_id: Some("call_member".to_string()),
                    created_at: 2_000,
                    approval_rule: None,
                    grant_dir: None,
                });
        // The main agent's approval: unattributed (nil), the only form the
        // pre-supervision engine produced.
        let (main_row_id, _main_rx) =
            artifacts
                .approval_gate
                .register(mirri_core::session::gates::ApprovalSnapshot {
                    tool_name: "Bash".to_string(),
                    action: "execute Bash".to_string(),
                    display: json!({ "summary": "ls" }),
                    agent_id: Uuid::nil(),
                    turn_id: Some(1),
                    tool_call_id: Some("call_main".to_string()),
                    created_at: 2_001,
                    approval_rule: None,
                    grant_dir: None,
                });
        // An approval whose agent uuid names no registered agent — must not
        // fabricate an identity on the row.
        let (ghost_row_id, _ghost_rx) =
            artifacts
                .approval_gate
                .register(mirri_core::session::gates::ApprovalSnapshot {
                    tool_name: "Bash".to_string(),
                    action: "execute Bash".to_string(),
                    display: json!({ "summary": "true" }),
                    agent_id: Uuid::new_v4(),
                    turn_id: Some(1),
                    tool_call_id: Some("call_ghost".to_string()),
                    created_at: 2_002,
                    approval_rule: None,
                    grant_dir: None,
                });
        let event_channels = Arc::new(SessionEventChannelRegistry::new(256));
        let handler = snapshot_handler(&manager, &event_channels);

        // When snapshotting the session
        let value = handler
            .handle(json!({ "sessionId": session_id }))
            .await
            .expect("snapshot resolves");

        // Then the member's row names the member by its full agent id, the
        // unattributed and unresolvable rows carry no agent_id at all
        let pending = value["pending_approvals"]
            .as_array()
            .expect("pending approvals is an array");
        assert_eq!(pending.len(), 3);
        let row = |id: &str| {
            pending
                .iter()
                .find(|row| row["approval_id"] == json!(id))
                .unwrap_or_else(|| panic!("the row for {id} appears in the snapshot"))
        };
        assert_eq!(
            row(member_row_id.to_string().as_str())["agent_id"],
            json!(member_id),
            "the member's row carries the full agent id (the roster key)"
        );
        assert!(
            row(main_row_id.to_string().as_str())
                .get("agent_id")
                .is_none(),
            "the unattributed (main-agent) row omits agent_id"
        );
        assert!(
            row(ghost_row_id.to_string().as_str())
                .get("agent_id")
                .is_none(),
            "an unresolvable agent uuid is honestly dropped — never fabricated"
        );
    }

    /// Given a session with a pending question whose registered items carry
    /// no ids (the rust gate transparently forwards the AskUserQuestion tool
    /// input — `{question, options: [{label, description}], multi_select}`),
    /// When `session.snapshot` is taken, Then each `pending_questions` row's
    /// items carry a synthesized item id (`q_<idx>`) and option ids
    /// (`opt_<item>_<option>`) — the same scheme the TS v2 REST edge mints
    /// (`questionRequestSchema` requires ids) — so a client that restores its
    /// question cards from the snapshot can answer unambiguously; without ids
    /// every option's id is undefined and selecting one selects them all.
    #[tokio::test]
    async fn should_mint_item_and_option_ids_when_snapshotting_pending_questions() {
        // Given a session with one pending question registered without ids
        let (_tmp, manager, session_id) = session_manager_with_session().await;
        let artifacts = manager
            .session_artifacts(&SessionId::new(&session_id))
            .expect("created session has artifacts");
        let (question_id, _rx) = artifacts.question_gate.register(
            mirri_core::session::gates::QuestionSnapshot {
                questions: json!([{
                    "question": "Which database?",
                    "multi_select": false,
                    "options": [
                        { "label": "Postgres" },
                        { "label": "SQLite", "description": "embedded" },
                    ],
                }]),
                agent_id: Uuid::nil(),
                turn_id: Some(5),
                tool_call_id: Some("call_q".to_string()),
                created_at: 1_002,
            },
        );
        let event_channels = Arc::new(SessionEventChannelRegistry::new(256));
        let handler = snapshot_handler(&manager, &event_channels);

        // When snapshotting the session
        let value = handler
            .handle(json!({ "sessionId": session_id }))
            .await
            .expect("snapshot resolves");

        // Then the pending question row's items carry synthesized ids and
        // the snake_case flag spelling the v2 REST contract declares
        let pending = value["pending_questions"]
            .as_array()
            .expect("pending questions is an array");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0]["question_id"], json!(question_id.to_string()));
        let items = pending[0]["questions"]
            .as_array()
            .expect("questions items is an array");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["id"], json!("q_0"));
        assert_eq!(
            items[0]["multi_select"],
            json!(false),
            "the snapshot keeps the v2 REST snake_case flag spelling"
        );
        let options = items[0]["options"]
            .as_array()
            .expect("options is an array");
        assert_eq!(options.len(), 2);
        // Option ids mint to the LABEL — the same scheme the live
        // question.ask bridge uses — because the rust question.answer
        // handler feeds the answer string to the model as-is.
        assert_eq!(options[0]["id"], json!("Postgres"));
        assert_eq!(options[0]["label"], json!("Postgres"));
        assert_eq!(options[1]["id"], json!("SQLite"));
        assert_eq!(options[1]["label"], json!("SQLite"));
        assert_eq!(options[1]["description"], json!("embedded"));
    }

    /// Given a session of a NON-default workspace (the registry routing
    /// resolves it onto a second manager, while the handler's bound manager
    /// stays the process default), When `session.snapshot` is taken, Then the
    /// snapshot's session view carries the OWNING workspace's root as `cwd` —
    /// the same field the `session.list` rows report (W4/W5), so the web
    /// client's snapshot merge cannot wipe it and drop the session out of its
    /// sidebar workspace group.
    #[tokio::test]
    async fn should_carry_the_owning_workspace_root_as_cwd_when_snapshotting_a_non_default_workspace_session()
     {
        // The handler's bound default manager (workspace A)
        let default_tmp = tempfile::tempdir().expect("default tempdir must be creatable");
        let default_workspace = Arc::new(WorkspaceScope::new(
            WorkspaceId::new("default-ws"),
            default_tmp.path().to_path_buf(),
            WorkspaceScopeConfig::default(),
        ));
        let default_manager = Arc::new(SessionManager::new(default_workspace));

        // The session's owning manager (workspace B) — a different root
        let owning_tmp = tempfile::tempdir().expect("owning tempdir must be creatable");
        let owning_workspace = Arc::new(WorkspaceScope::new(
            WorkspaceId::new("owning-ws"),
            owning_tmp.path().to_path_buf(),
            WorkspaceScopeConfig::default(),
        ));
        let owning_manager = Arc::new(SessionManager::new(owning_workspace));
        let now = Utc::now();
        let session = owning_manager
            .create_session(
                SessionMetadata {
                    title: "Non-default workspace session".to_string(),
                    is_custom_title: false,
                    created_at: now,
                    updated_at: now,
                    tags: Default::default(),
                    archived: false,
                    // Left None on purpose — create_session backfills it from
                    // the owning manager's workspace cwd, exactly like the
                    // serve path does for a lazily-created workspace session.
                    work_dir: None,
                    last_prompt: None,
                    forked_from: None,
                    cache_root_id: None,
                    custom: None,
                },
                SessionScopeConfig::default(),
            )
            .await
            .expect("session must be creatable");
        let session_id = session.id().as_str().to_string();

        // The handler is constructed over the DEFAULT manager, but routing
        // resolves the session onto the owning workspace's runtime — the
        // composition root's RegistrySessionRouting shape.
        let event_channels = Arc::new(SessionEventChannelRegistry::new(256));
        let handler = SnapshotSessionHandler::new(
            Arc::clone(&default_manager),
            Arc::clone(&event_channels),
            Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            "test-epoch".to_string().into(),
            noop_engine(),
        )
        .with_routing(Arc::new(SingleManagerRouting::live(Arc::clone(
            &owning_manager,
        ))));

        // When snapshotting the session
        let value = handler
            .handle(json!({ "sessionId": session_id }))
            .await
            .expect("snapshot resolves");

        // Then the session view carries the owning workspace's root as cwd
        let cwd = value["session"]["cwd"]
            .as_str()
            .expect("the snapshot session view carries a cwd")
            .to_string();
        assert_eq!(
            cwd,
            owning_tmp.path().to_string_lossy(),
            "the snapshot cwd is the owning workspace's root"
        );
        assert_ne!(
            cwd,
            default_tmp.path().to_string_lossy().into_owned(),
            "the snapshot cwd is never the default workspace's root"
        );
    }

    #[tokio::test]
    async fn should_mark_busy_when_snapshotting_session_with_in_flight_turn() {
        // Given a session with an in-flight engine turn tracked in the
        // shared registry
        let (_tmp, manager, session_id) = session_manager_with_session().await;
        let event_channels = Arc::new(SessionEventChannelRegistry::new(256));
        let in_flight_turns: InFlightTurns = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let handle = tokio::spawn(async {});
        in_flight_turns
            .lock()
            .await
            .insert(format!("{session_id}:agent-test:turn-1"), handle);
        let handler = SnapshotSessionHandler::new(
            Arc::clone(&manager),
            Arc::clone(&event_channels),
            in_flight_turns,
            "test-epoch".to_string().into(),
            noop_engine(),
        );

        // When snapshotting the session
        let value = handler
            .handle(json!({ "sessionId": session_id }))
            .await
            .expect("snapshot resolves");

        // Then the snapshot reports the session as busy
        assert_eq!(value["session"]["busy"], json!(true));
    }

    /// A minimal engine stub whose only interesting answer is the
    /// in-flight session list — the driver-injected turn visibility face.
    struct StaticInFlightEngine {
        sessions: Vec<String>,
    }

    #[async_trait::async_trait]
    impl mirri_transport::adapter::EngineAdapter for StaticInFlightEngine {
        async fn call_method(
            &self,
            _method: &str,
            _params: serde_json::Value,
        ) -> Result<serde_json::Value, mirri_transport::adapter::EngineError> {
            Err(mirri_transport::adapter::EngineError::MethodNotFound(
                "stub".to_string(),
            ))
        }
        async fn subscribe_events(
            &self,
        ) -> Result<
            tokio::sync::mpsc::Receiver<mirri_transport::frame::JsonRpcNotification>,
            mirri_transport::adapter::EngineError,
        > {
            Err(mirri_transport::adapter::EngineError::NotConnected)
        }
        async fn on_reverse_rpc(
            &self,
            _handler: mirri_transport::adapter::ReverseRpcHandler,
        ) -> Result<(), mirri_transport::adapter::EngineError> {
            Ok(())
        }
        async fn in_flight_sessions(&self) -> Vec<String> {
            self.sessions.clone()
        }
        async fn dispose(&self) -> Result<(), mirri_transport::adapter::EngineError> {
            Ok(())
        }
    }

    /// An engine that never reports an engine-side in-flight turn — the
    /// default assumption for hand-built handlers.
    fn noop_engine() -> Arc<dyn mirri_transport::adapter::EngineAdapter> {
        Arc::new(StaticInFlightEngine {
            sessions: Vec::new(),
        })
    }

    #[tokio::test]
    async fn should_mark_busy_when_snapshotting_session_with_only_an_engine_in_flight_turn() {
        // Given a session whose only in-flight turn is engine-side (a
        // driver-injected turn the transport registry never tracked)
        let (_tmp, manager, session_id) = session_manager_with_session().await;
        let event_channels = Arc::new(SessionEventChannelRegistry::new(256));
        let in_flight_turns: InFlightTurns = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let engine: Arc<dyn mirri_transport::adapter::EngineAdapter> =
            Arc::new(StaticInFlightEngine {
                sessions: vec![session_id.clone()],
            });
        let handler = SnapshotSessionHandler::new(
            Arc::clone(&manager),
            Arc::clone(&event_channels),
            in_flight_turns,
            "test-epoch".to_string().into(),
            engine,
        );

        // When snapshotting the session
        let value = handler
            .handle(json!({ "sessionId": session_id }))
            .await
            .expect("snapshot resolves");

        // Then the snapshot reports the session as busy
        assert_eq!(value["session"]["busy"], json!(true));
    }

    #[tokio::test]
    async fn should_report_running_when_statusing_session_with_only_an_engine_in_flight_turn() {
        let (_tmp, manager, session_id) = session_manager_with_session().await;
        let in_flight_turns: InFlightTurns = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let engine: Arc<dyn mirri_transport::adapter::EngineAdapter> =
            Arc::new(StaticInFlightEngine {
                sessions: vec![session_id.clone()],
            });
        let handler = StatusSessionHandler::new(Arc::clone(&manager), in_flight_turns, engine)
            .with_routing(Arc::new(SingleManagerRouting::live(Arc::clone(&manager))));

        let value = handler
            .handle(json!({ "sessionId": session_id }))
            .await
            .expect("status resolves");

        assert_eq!(value["running"], json!(true));
    }

    #[tokio::test]
    async fn should_report_busy_when_getting_session_with_only_an_engine_in_flight_turn() {
        let (_tmp, manager, session_id) = session_manager_with_session().await;
        let event_channels = Arc::new(SessionEventChannelRegistry::new(256));
        let in_flight_turns: InFlightTurns = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let engine: Arc<dyn mirri_transport::adapter::EngineAdapter> =
            Arc::new(StaticInFlightEngine {
                sessions: vec![session_id.clone()],
            });
        let handler = GetSessionHandler::new(
            Arc::clone(&manager),
            in_flight_turns,
            Arc::clone(&event_channels),
            "test-epoch".to_string().into(),
            engine,
        );

        let value = handler
            .handle(json!({ "sessionId": session_id }))
            .await
            .expect("get resolves");

        assert_eq!(value["busy"], json!(true));
    }

    #[tokio::test]
    async fn should_return_empty_lists_when_snapshotting_session_without_agents() {
        // Given a session without any agent
        let (_tmp, manager, session_id) = session_manager_with_session().await;
        let event_channels = Arc::new(SessionEventChannelRegistry::new(256));
        let handler = snapshot_handler(&manager, &event_channels);

        // When snapshotting the session
        let value = handler
            .handle(json!({ "sessionId": session_id }))
            .await
            .expect("snapshot resolves");

        // Then the snapshot has no agents, no messages, and null model/permission
        assert_eq!(value["session"]["agentCount"], json!(0));
        assert_eq!(value["session"]["model"], Value::Null);
        assert_eq!(value["session"]["permission"], Value::Null);
        assert_eq!(value["session"]["messageCount"], json!(0));
        assert_eq!(
            value["messages"]["items"]
                .as_array()
                .expect("items array")
                .len(),
            0
        );
    }

    #[tokio::test]
    async fn should_return_not_found_when_snapshotting_unknown_session() {
        // Given no session under the requested id
        let (_tmp, manager, _) = session_manager_with_session().await;
        let event_channels = Arc::new(SessionEventChannelRegistry::new(256));
        let handler = snapshot_handler(&manager, &event_channels);

        // When snapshotting an unknown session
        let result = handler
            .handle(json!({ "sessionId": "session-missing" }))
            .await;

        // Then the request fails with NotFound
        assert!(matches!(result, Err(RpcError::NotFound(_))));
    }

    // =======================================================================
    // session.warnings
    // =======================================================================

    async fn session_manager_with_file_workspace()
    -> (tempfile::TempDir, Arc<SessionManager>, String) {
        let (tmp, manager, session_id) = session_manager_with_session().await;
        std::fs::create_dir_all(tmp.path().join("beta")).expect("beta dir is creatable");
        std::fs::write(tmp.path().join("alpha.txt"), "alpha content\n").unwrap();
        std::fs::write(tmp.path().join("gamma.md"), "# gamma\n").unwrap();
        (tmp, manager, session_id)
    }

    #[tokio::test]
    async fn should_return_sorted_items_with_exact_element_keys_when_listing_workspace_root() {
        // Given a session whose workspace root holds alpha.txt, beta/, gamma.md
        let (_tmp, manager, session_id) = session_manager_with_file_workspace().await;
        let handler = ListFilesHandler::new(Arc::clone(&manager));

        // When listing the workspace root
        let value = handler
            .handle(json!({ "sessionId": session_id, "path": "." }))
            .await
            .expect("list resolves");

        // Then the result is `{items, truncated: false}` with NO `entries`
        // wrapper (the Web client's `listDirectory` reads items/truncated —
        // the old `entries` wrapper was silently read as an empty list)
        assert!(
            value.get("entries").is_none(),
            "no entries wrapper: {value}"
        );
        assert_eq!(value["truncated"], json!(false));
        let items = value["items"].as_array().expect("items is an array");
        // The session's persistence layer materializes a `.mirri` directory
        // in the workspace root alongside the fixture files.
        assert_eq!(items.len(), 4);

        // And the items are sorted by name with workspace-relative paths
        assert_eq!(items[0]["name"], json!(".mirri"));
        assert_eq!(items[0]["path"], json!(".mirri"));
        assert_eq!(items[0]["kind"], json!("dir"));
        assert_eq!(items[1]["name"], json!("alpha.txt"));
        assert_eq!(items[1]["path"], json!("alpha.txt"));
        assert_eq!(items[1]["kind"], json!("file"));
        assert_eq!(items[2]["name"], json!("beta"));
        assert_eq!(items[2]["path"], json!("beta"));
        assert_eq!(items[2]["kind"], json!("dir"));
        assert_eq!(items[3]["name"], json!("gamma.md"));
        assert_eq!(items[3]["path"], json!("gamma.md"));
        assert_eq!(items[3]["kind"], json!("file"));

        // And each element's keys are exactly the five view fields — no
        // shape drift
        let mut keys: Vec<&str> = items[0]
            .as_object()
            .expect("item is an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["kind", "modifiedAt", "name", "path", "size"]);
        assert!(
            items[0]["modifiedAt"].as_str().is_some(),
            "modifiedAt is an RFC3339 string"
        );
        assert!(
            items[0]["size"].as_u64().is_some(),
            "size is a non-null number for a regular file"
        );
    }

    #[tokio::test]
    async fn should_return_not_found_when_listing_a_path_that_is_not_a_directory() {
        // Given a session whose workspace root holds the file `alpha.txt`
        let (_tmp, manager, session_id) = session_manager_with_file_workspace().await;
        let handler = ListFilesHandler::new(Arc::clone(&manager));

        // When listing a file instead of a directory
        let result = handler
            .handle(json!({ "sessionId": session_id, "path": "alpha.txt" }))
            .await;

        // Then the request fails with NotFound
        assert!(matches!(result, Err(RpcError::NotFound(_))));
    }

    #[tokio::test]
    async fn should_return_content_and_encoding_when_reading_a_utf8_file() {
        // Given a session whose workspace root holds `alpha.txt`
        let (_tmp, manager, session_id) = session_manager_with_file_workspace().await;
        let handler = ReadFileHandler::new(Arc::clone(&manager));

        // When reading the file
        let value = handler
            .handle(json!({ "sessionId": session_id, "path": "alpha.txt" }))
            .await
            .expect("read resolves");

        // Then the content is returned with utf-8 as the encoding
        assert_eq!(value["content"], json!("alpha content\n"));
        assert_eq!(value["encoding"], json!("utf-8"));
    }

    #[tokio::test]
    async fn should_return_full_file_metadata_when_reading_a_utf8_file() {
        // Given a session whose workspace root holds `gamma.md` (13 bytes)
        let (_tmp, manager, session_id) = session_manager_with_file_workspace().await;
        let handler = ReadFileHandler::new(Arc::clone(&manager));

        // When reading the file
        let value = handler
            .handle(json!({ "sessionId": session_id, "path": "gamma.md" }))
            .await
            .expect("read resolves");

        // Then the result carries the Web file-preview contract fields the
        // old shape omitted (path/size/mime/languageId/lineCount/isBinary) —
        // without them the preview panel falls into the binary branch and
        // renders "unknown type · NaN MB"
        assert_eq!(value["path"], json!("gamma.md"));
        assert_eq!(value["size"], json!("# gamma\n".len() as u64));
        assert_eq!(value["mime"], json!("text/markdown"));
        assert_eq!(value["languageId"], json!("markdown"));
        assert_eq!(value["lineCount"], json!(1));
        assert_eq!(value["isBinary"], json!(false));
        assert_eq!(value["truncated"], json!(false));
        assert!(value["etag"].as_str().is_some(), "etag is a string");

        // And every key the Web client reads is present, camelCased
        let mut keys: Vec<&str> = value
            .as_object()
            .expect("result is an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "content",
                "encoding",
                "etag",
                "isBinary",
                "languageId",
                "lineCount",
                "mime",
                "path",
                "size",
                "truncated"
            ]
        );
    }

    #[tokio::test]
    async fn should_guess_plain_text_mime_when_extension_is_unknown() {
        // Given a session whose workspace root holds `alpha.txt` (an extension
        // with no explicit mime mapping — falls through to text/plain)
        let (_tmp, manager, session_id) = session_manager_with_file_workspace().await;
        let handler = ReadFileHandler::new(Arc::clone(&manager));

        // When reading the file
        let value = handler
            .handle(json!({ "sessionId": session_id, "path": "alpha.txt" }))
            .await
            .expect("read resolves");

        // Then the mime is the text/plain fallback with no languageId
        assert_eq!(value["mime"], json!("text/plain"));
        assert_eq!(value["languageId"], json!(null));
        // And a trailing newline does not count as an extra line
        assert_eq!(value["lineCount"], json!(1));
    }

    #[tokio::test]
    async fn should_return_base64_content_when_reading_a_binary_file() {
        // Given a session whose workspace root holds a PNG-headered file
        let (tmp, manager, session_id) = session_manager_with_session().await;
        let png: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0xFF];
        std::fs::write(tmp.path().join("logo.png"), png).unwrap();
        let handler = ReadFileHandler::new(Arc::clone(&manager));

        // When reading the file
        let value = handler
            .handle(json!({ "sessionId": session_id, "path": "logo.png" }))
            .await
            .expect("read resolves");

        // Then the binary content is returned base64-encoded (the Web client
        // builds a data: URL from it) with the binary marker fields set
        assert_eq!(value["encoding"], json!("base64"));
        let expected_b64 = base64::engine::general_purpose::STANDARD.encode(png);
        let actual = value["content"].as_str().expect("content is a string");
        // Compare after stripping padding so the assertion holds regardless
        // of the encoder's padding policy
        assert_eq!(
            actual.replace('=', ""),
            expected_b64.replace('=', ""),
            "content is the base64 of the file bytes"
        );
        assert_eq!(value["mime"], json!("image/png"));
        assert_eq!(value["isBinary"], json!(true));
        assert_eq!(value["size"], json!(png.len() as u64));
        assert_eq!(value["languageId"], json!(null));
    }

    // =======================================================================
    // file.search
    // =======================================================================

    /// A session manager with one session whose workspace root holds a
    /// nested file layout: `src/main.rs`, `src/lib.rs`, `README.md`.
    async fn session_manager_with_search_workspace()
    -> (tempfile::TempDir, Arc<SessionManager>, String) {
        let (tmp, manager, session_id) = session_manager_with_session().await;
        std::fs::create_dir_all(tmp.path().join("src")).expect("src dir is creatable");
        std::fs::write(tmp.path().join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(tmp.path().join("src/lib.rs"), "pub fn lib() {}\n").unwrap();
        std::fs::write(tmp.path().join("README.md"), "# readme\n").unwrap();
        (tmp, manager, session_id)
    }

    #[tokio::test]
    async fn should_rank_prefix_matches_first_when_searching_by_filename_substring() {
        // Given a session whose workspace root holds a nested file layout
        let (_tmp, manager, session_id) = session_manager_with_search_workspace().await;
        let handler = SearchFilesHandler::new(Arc::clone(&manager));

        // When searching for the substring "main"
        let value = handler
            .handle(json!({ "sessionId": session_id, "query": "main" }))
            .await
            .expect("search resolves");

        // Then the prefix match is returned first with a score of 1.0 and a
        // workspace-relative path
        let items = value["items"].as_array().expect("items is an array");
        assert_eq!(value["truncated"], json!(false));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["path"], json!("src/main.rs"));
        assert_eq!(items[0]["name"], json!("main.rs"));
        assert_eq!(items[0]["kind"], json!("file"));
        assert_eq!(items[0]["score"], json!(1.0));
        assert_eq!(items[0]["matchPositions"], json!([]));
    }

    #[tokio::test]
    async fn should_score_containment_matches_lower_when_query_is_not_a_prefix() {
        // Given a session whose workspace root holds `.rs` files
        let (_tmp, manager, session_id) = session_manager_with_search_workspace().await;
        let handler = SearchFilesHandler::new(Arc::clone(&manager));

        // When searching for "rs" (contained in, but not a prefix of, the names)
        let value = handler
            .handle(json!({ "sessionId": session_id, "query": "rs" }))
            .await
            .expect("search resolves");

        // Then every match scores 0.5 and both `.rs` files are found
        let items = value["items"].as_array().expect("items is an array");
        assert!(items.iter().all(|item| item["score"] == json!(0.5)));
        assert!(items.iter().any(|item| item["name"] == "main.rs"));
        assert!(items.iter().any(|item| item["name"] == "lib.rs"));
    }

    #[tokio::test]
    async fn should_truncate_when_search_results_exceed_limit() {
        // Given a session whose workspace root holds three files
        let (_tmp, manager, session_id) = session_manager_with_search_workspace().await;
        let handler = SearchFilesHandler::new(Arc::clone(&manager));

        // When searching with a limit below the match count
        let value = handler
            .handle(json!({ "sessionId": session_id, "query": "rs", "limit": 1 }))
            .await
            .expect("search resolves");

        // Then only one item is returned and the response is marked truncated
        let items = value["items"].as_array().expect("items is an array");
        assert_eq!(items.len(), 1);
        assert_eq!(value["truncated"], json!(true));
    }

    #[tokio::test]
    async fn should_return_empty_items_when_query_matches_nothing() {
        // Given a session whose workspace root holds a nested file layout
        let (_tmp, manager, session_id) = session_manager_with_search_workspace().await;
        let handler = SearchFilesHandler::new(Arc::clone(&manager));

        // When searching for a substring that matches no file name
        let value = handler
            .handle(json!({ "sessionId": session_id, "query": "zzz-not-there" }))
            .await
            .expect("search resolves");

        // Then the item list is empty and not truncated
        assert_eq!(value["items"], json!([]));
        assert_eq!(value["truncated"], json!(false));
    }

    #[tokio::test]
    async fn should_return_empty_items_when_query_is_blank() {
        // Given a session whose workspace root holds files
        let (_tmp, manager, session_id) = session_manager_with_search_workspace().await;
        let handler = SearchFilesHandler::new(Arc::clone(&manager));

        // When searching with a blank query
        let value = handler
            .handle(json!({ "sessionId": session_id, "query": "  " }))
            .await
            .expect("search resolves");

        // Then no files are reported
        assert_eq!(value["items"], json!([]));
    }

    #[tokio::test]
    async fn should_respect_exclude_globs_when_searching() {
        // Given a session whose workspace root holds `README.md` next to `.rs` files
        let (_tmp, manager, session_id) = session_manager_with_search_workspace().await;
        let handler = SearchFilesHandler::new(Arc::clone(&manager));

        // When searching with an exclude glob for markdown
        let value = handler
            .handle(json!({
                "sessionId": session_id,
                "query": "r",
                "excludeGlobs": ["*.md"],
            }))
            .await
            .expect("search resolves");

        // Then README.md is filtered out while the `.rs` files remain
        let items = value["items"].as_array().expect("items is an array");
        assert!(items.iter().all(|item| item["name"] != "README.md"));
        assert!(items.iter().any(|item| item["name"] == "main.rs"));
    }

    #[tokio::test]
    async fn should_return_not_found_when_searching_unknown_session() {
        // Given no session under the requested id
        let (_tmp, manager, _) = session_manager_with_session().await;
        let handler = SearchFilesHandler::new(Arc::clone(&manager));

        // When searching an unknown session
        let result = handler
            .handle(json!({ "sessionId": "session-missing", "query": "main" }))
            .await;

        // Then the request fails with NotFound
        assert!(matches!(result, Err(RpcError::NotFound(_))));
    }

    // =======================================================================
    // file.grep
    // =======================================================================

    /// Return `true` if the `rg` binary is available on PATH.
    fn rg_available() -> bool {
        std::process::Command::new("rg")
            .arg("--version")
            .output()
            .is_ok()
    }

    #[tokio::test]
    async fn should_return_matching_lines_when_grepping_workspace_with_ripgrep() {
        if !rg_available() {
            eprintln!("skipping: rg not available");
            return;
        }

        // Given a session whose workspace root holds a source file
        let (tmp, manager, session_id) = session_manager_with_session().await;
        std::fs::create_dir_all(tmp.path().join("src")).unwrap();
        std::fs::write(
            tmp.path().join("src/app.rs"),
            "fn main() {\n    println!(\"hi\");\n}\n",
        )
        .unwrap();
        let handler = GrepFilesHandler::new(Arc::clone(&manager));

        // When grepping for a fixed string
        let value = handler
            .handle(json!({
                "sessionId": session_id,
                "pattern": "println",
                "regex": false,
            }))
            .await
            .expect("grep resolves");

        // Then the match is reported with 1-based line and column
        assert_eq!(value["truncated"], json!(false));
        let files = value["files"].as_array().expect("files is an array");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0]["path"], json!("src/app.rs"));
        assert_eq!(files[0]["matches"][0]["line"], json!(2));
        assert_eq!(files[0]["matches"][0]["col"], json!(5));
        assert_eq!(
            files[0]["matches"][0]["text"],
            json!("    println!(\"hi\");")
        );
        assert!(value["filesScanned"].as_u64().is_some_and(|n| n >= 1));
        assert!(value["elapsedMs"].as_u64().is_some());
    }

    #[tokio::test]
    async fn should_return_before_and_after_context_when_grepping_with_context_lines() {
        if !rg_available() {
            eprintln!("skipping: rg not available");
            return;
        }

        // Given a session whose workspace root holds a file where the match
        // is surrounded by lines
        let (tmp, manager, session_id) = session_manager_with_session().await;
        std::fs::write(tmp.path().join("a.txt"), "one\ntwo\nthree\nfour\n").unwrap();
        let handler = GrepFilesHandler::new(Arc::clone(&manager));

        // When grepping with one context line on each side
        let value = handler
            .handle(json!({
                "sessionId": session_id,
                "pattern": "three",
                "contextLines": 1,
            }))
            .await
            .expect("grep resolves");

        // Then the surrounding lines are attached as before/after windows
        let files = value["files"].as_array().expect("files is an array");
        assert_eq!(files.len(), 1);
        assert_eq!(files[0]["matches"][0]["line"], json!(3));
        assert_eq!(files[0]["matches"][0]["before"], json!(["two"]));
        assert_eq!(files[0]["matches"][0]["after"], json!(["four"]));
    }

    #[tokio::test]
    async fn should_return_empty_files_when_grep_matches_nothing() {
        if !rg_available() {
            eprintln!("skipping: rg not available");
            return;
        }

        // Given a session whose workspace root holds a file without the pattern
        let (tmp, manager, session_id) = session_manager_with_session().await;
        std::fs::write(tmp.path().join("a.txt"), "hello world\n").unwrap();
        let handler = GrepFilesHandler::new(Arc::clone(&manager));

        // When grepping for a pattern that is absent
        let value = handler
            .handle(json!({ "sessionId": session_id, "pattern": "zzz" }))
            .await
            .expect("grep resolves");

        // Then no files match, the scan still happened, and ripgrep's exit
        // code 1 (no matches) is not an error
        assert_eq!(value["files"], json!([]));
        assert_eq!(value["truncated"], json!(false));
        assert!(value["filesScanned"].as_u64().is_some_and(|n| n >= 1));
    }

    #[tokio::test]
    async fn should_return_not_found_when_grepping_unknown_session() {
        // Given no session under the requested id
        let (_tmp, manager, _) = session_manager_with_session().await;
        let handler = GrepFilesHandler::new(Arc::clone(&manager));

        // When grepping an unknown session
        let result = handler
            .handle(json!({ "sessionId": "session-missing", "pattern": "hello" }))
            .await;

        // Then the request fails with NotFound
        assert!(matches!(result, Err(RpcError::NotFound(_))));
    }

    // =======================================================================
    // file.git_status
    // =======================================================================

    /// Return `true` if the `git` binary is available on PATH.
    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok()
    }

    /// Run `git <args>` in `dir`. Test helper.
    fn run_git(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .expect("git must be runnable")
    }

    /// Initialize a git repository in `dir` with one committed file
    /// `a.txt` (content `"v1\n"`). Test helper.
    fn init_git_repo_with_commit(dir: &std::path::Path) {
        run_git(dir, &["init", "-q"]);
        std::fs::write(dir.join("a.txt"), "v1\n").unwrap();
        run_git(dir, &["add", "a.txt"]);
        let commit = run_git(
            dir,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-q",
                "-m",
                "init",
            ],
        );
        assert!(
            commit.status.success(),
            "seed commit must succeed: {}",
            String::from_utf8_lossy(&commit.stderr)
        );
    }

    #[tokio::test]
    async fn should_return_empty_status_when_workspace_is_not_a_git_repository() {
        // Given a session whose workspace root is a plain directory
        let (_tmp, manager, session_id) = session_manager_with_session().await;
        let handler = GitStatusFilesHandler::new(Arc::clone(&manager));

        // When reading the git status
        let value = handler
            .handle(json!({ "sessionId": session_id }))
            .await
            .expect("git status resolves");

        // Then the empty status object is returned instead of an error
        assert_eq!(
            value,
            json!({
                "branch": "",
                "ahead": 0,
                "behind": 0,
                "entries": {},
                "additions": 0,
                "deletions": 0,
            })
        );
    }

    #[tokio::test]
    async fn should_report_modified_and_untracked_files_when_git_workspace_has_changes() {
        if !git_available() {
            eprintln!("skipping: git not available");
            return;
        }

        // Given a session whose workspace root is a git repo with one commit
        let (tmp, manager, session_id) = session_manager_with_session().await;
        init_git_repo_with_commit(tmp.path());
        std::fs::write(tmp.path().join("a.txt"), "v1\nv2\n").unwrap();
        std::fs::write(tmp.path().join("new.txt"), "untracked\n").unwrap();
        let handler = GitStatusFilesHandler::new(Arc::clone(&manager));

        // When reading the git status
        let value = handler
            .handle(json!({ "sessionId": session_id }))
            .await
            .expect("git status resolves");

        // Then the modified and untracked files are reported with a branch
        assert!(!value["branch"].as_str().unwrap_or("").is_empty());
        assert_eq!(value["ahead"], json!(0));
        assert_eq!(value["behind"], json!(0));
        let entries = value["entries"].as_object().expect("entries is an object");
        assert_eq!(entries["a.txt"], json!("modified"));
        assert_eq!(entries["new.txt"], json!("untracked"));
        assert_eq!(value["additions"], json!(1));
        assert_eq!(value["deletions"], json!(0));
    }

    #[tokio::test]
    async fn should_return_not_found_when_status_of_unknown_session() {
        // Given no session under the requested id
        let (_tmp, manager, _) = session_manager_with_session().await;
        let handler = GitStatusFilesHandler::new(Arc::clone(&manager));

        // When reading the git status of an unknown session
        let result = handler
            .handle(json!({ "sessionId": "session-missing" }))
            .await;

        // Then the request fails with NotFound
        assert!(matches!(result, Err(RpcError::NotFound(_))));
    }

    // =======================================================================
    // file.diff
    // =======================================================================

    #[tokio::test]
    async fn should_return_diff_when_git_workspace_has_uncommitted_changes() {
        if !git_available() {
            eprintln!("skipping: git not available");
            return;
        }

        // Given a session whose workspace root is a git repo with a modified file
        let (tmp, manager, session_id) = session_manager_with_session().await;
        init_git_repo_with_commit(tmp.path());
        std::fs::write(tmp.path().join("a.txt"), "v1\nchanged\n").unwrap();
        let handler = DiffFilesHandler::new(Arc::clone(&manager));

        // When diffing the changed file against HEAD
        let value = handler
            .handle(json!({ "sessionId": session_id, "path": "a.txt" }))
            .await
            .expect("diff resolves");

        // Then the unified diff is returned with the requested path echoed
        assert_eq!(value["path"], json!("a.txt"));
        assert_eq!(value["truncated"], json!(false));
        let diff = value["diff"].as_str().expect("diff is a string");
        assert!(
            diff.contains("diff --git a/a.txt b/a.txt"),
            "diff has a file header: {diff}"
        );
        assert!(
            diff.contains("+changed"),
            "diff carries the added line: {diff}"
        );
    }

    #[tokio::test]
    async fn should_truncate_when_diff_exceeds_size_cap() {
        if !git_available() {
            eprintln!("skipping: git not available");
            return;
        }

        // Given a repo whose working copy rewrites a file with 100k lines
        let (tmp, manager, session_id) = session_manager_with_session().await;
        init_git_repo_with_commit(tmp.path());
        let huge = "lorem ipsum dolor sit amet\n".repeat(60_000);
        std::fs::write(tmp.path().join("a.txt"), huge).unwrap();
        let handler = DiffFilesHandler::new(Arc::clone(&manager));

        // When diffing the rewritten file
        let value = handler
            .handle(json!({ "sessionId": session_id, "path": "a.txt" }))
            .await
            .expect("diff resolves");

        // Then the diff is capped at MAX_DIFF_BYTES and marked truncated
        assert_eq!(value["truncated"], json!(true));
        let diff = value["diff"].as_str().expect("diff is a string");
        assert!(diff.len() <= MAX_DIFF_BYTES);
    }

    #[tokio::test]
    async fn should_return_internal_error_when_diffing_in_non_git_workspace() {
        // Given a session whose workspace root is a plain directory
        let (_tmp, manager, session_id) = session_manager_with_session().await;
        let handler = DiffFilesHandler::new(Arc::clone(&manager));

        // When diffing a file outside any git work tree
        let result = handler
            .handle(json!({ "sessionId": session_id, "path": "a.txt" }))
            .await;

        // Then the request fails with an internal error naming the cause
        match result {
            Err(RpcError::Internal { message, .. }) => {
                assert!(
                    message.contains("not a git repository"),
                    "message explains the cause: {message}"
                );
            }
            other => panic!("expected an internal error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn should_reject_path_outside_workspace_when_diffing() {
        // Given a session whose workspace root is a plain directory
        let (_tmp, manager, session_id) = session_manager_with_session().await;
        let handler = DiffFilesHandler::new(Arc::clone(&manager));

        // When diffing a path that escapes the workspace root
        let result = handler
            .handle(json!({ "sessionId": session_id, "path": "../../etc/passwd" }))
            .await;

        // Then the request fails with InvalidParams on the traversal
        assert!(matches!(result, Err(RpcError::InvalidParams(_))));
    }

    #[tokio::test]
    async fn should_return_not_found_when_diff_of_unknown_session() {
        // Given no session under the requested id
        let (_tmp, manager, _) = session_manager_with_session().await;
        let handler = DiffFilesHandler::new(Arc::clone(&manager));

        // When diffing a file of an unknown session
        let result = handler
            .handle(json!({ "sessionId": "session-missing", "path": "a.txt" }))
            .await;

        // Then the request fails with NotFound
        assert!(matches!(result, Err(RpcError::NotFound(_))));
    }

    // =======================================================================
    // Transport registry surface (M5-B3a)
    // =======================================================================

    /// The M5-B3a transport registry must register exactly the 21
    /// transport-owned methods: the server-only fact rows of
    /// [`mirri_proto::rpc`] plus `btw.start` — a `both`-registered method
    /// whose serve-side fork handler stays here while the embedded engine
    /// implements the same fork semantics. The four `workspace.*` handlers
    /// were deleted in the W2 batch — the serve wire rides the engine
    /// fall-through for them.
    #[test]
    fn should_register_exactly_the_21_transport_owned_methods_when_building_the_transport_registry()
    {
        // Given a session manager and the shared server-side pieces
        let tmp = tempfile::tempdir().expect("tempdir must be creatable");
        let workspace = Arc::new(WorkspaceScope::new(
            WorkspaceId::new("registry-surface"),
            tmp.path().to_path_buf(),
            WorkspaceScopeConfig::default(),
        ));
        let manager = Arc::new(SessionManager::new(workspace));
        let channels = Arc::new(SessionEventChannelRegistry::new(256));
        let in_flight: InFlightTurns = Arc::new(tokio::sync::Mutex::new(HashMap::new()));

        // When the transport registry is built
        let registry = build_transport_registry(
            noop_engine(),
            Arc::clone(&manager),
            "registry-surface-server".to_string(),
            channels,
            in_flight,
            Arc::new(SingleManagerRouting::live(Arc::clone(&manager))),
            Arc::new(RwLock::new(mirri_core::ServerConfig::default())),
        );

        // Then it registers exactly the 21 transport-owned methods
        let mut actual: Vec<&str> = registry.list_methods();
        actual.sort_unstable();
        assert_eq!(
            actual,
            vec![
                "agent.get",
                "agent.list",
                "approval.decide",
                "btw.start",
                "config.watch",
                "file.diff",
                "file.git_status",
                "file.grep",
                "file.list",
                "file.read",
                "file.search",
                "mcp.subscribe",
                "message.list",
                "question.answer",
                "question.dismiss",
                "session.children",
                "session.get",
                "session.restore",
                "session.snapshot",
                "session.status",
                "tool.catalog",
            ]
        );
    }

    /// The transport registry must stay exactly the serve-server fact rows
    /// plus the explicit overrides (`btw.start`, `session.get`, and
    /// `session.restore` — the two session rows keep their transport-owned
    /// handlers after the B1 flips to both — same wire shapes as the
    /// embedded routes): no engine-domain row may creep
    /// back, and every registered method must be a `serves_server` fact.
    #[test]
    fn should_pin_the_transport_registry_to_the_server_only_fact_rows() {
        // Given the transport registry on a throwaway workspace
        let tmp = tempfile::tempdir().expect("tempdir must be creatable");
        let workspace = Arc::new(WorkspaceScope::new(
            WorkspaceId::new("registry-facts"),
            tmp.path().to_path_buf(),
            WorkspaceScopeConfig::default(),
        ));
        let manager = Arc::new(SessionManager::new(workspace));
        let channels = Arc::new(SessionEventChannelRegistry::new(256));
        let in_flight: InFlightTurns = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
        let registry = build_transport_registry(
            noop_engine(),
            Arc::clone(&manager),
            "registry-facts-server".to_string(),
            channels,
            in_flight,
            Arc::new(SingleManagerRouting::live(Arc::clone(&manager))),
            Arc::new(RwLock::new(mirri_core::ServerConfig::default())),
        );
        let methods: HashSet<&str> = registry.list_methods().into_iter().collect();

        // Then every registered method is a serve-surface fact...
        for method in &methods {
            let entry = mirri_proto::rpc::get(method)
                .unwrap_or_else(|| panic!("{method}: not a mirri_proto::rpc fact row"));
            assert!(
                entry.availability.serves_server(),
                "{method}: a transport-registered method must be a serves_server fact"
            );
        }

        // ...the count is exactly the server-only rows plus the six
        // explicit overrides (`btw.start`, `session.get`, `session.restore`
        // — B1 kept their transport-owned handlers; the B4-a EMB backfills
        // of approval.decide, question.answer, and question.dismiss kept
        // theirs too, same wire shapes — and the B4-b workspace handlers
        // were DELETED in the W2 batch, so the serve wire rides the engine
        // fall-through for `workspace.*`; the B2 flips ride the engine
        // fall-through too, so they never join the transport registry — and
        // the M7 mcp.subscribe row is registered: its transport-owned
        // handler answers the non-push transports while the WebSocket
        // endpoint handles it connection-locally)...
        let server_only: Vec<&'static str> = mirri_proto::rpc::methods_for(RpcBackend::Server)
            .filter(|entry| entry.availability == BackendAvailability::Server)
            .map(|entry| entry.name)
            .collect();
        assert_eq!(
            methods.len(),
            server_only.len() + 6,
            "the transport registry holds the server-only rows plus the btw.start, session.get, session.restore, approval.decide, question.answer, and question.dismiss overrides"
        );
        assert!(
            methods.contains("mcp.subscribe"),
            "mcp.subscribe registered (M7: the app-level event subscription surface)"
        );
        assert!(
            methods.contains("btw.start"),
            "btw.start override registered"
        );
        assert!(
            methods.contains("session.get"),
            "session.get override registered (B1: transport-owned handler kept, \
             same wire shape as the embedded route)"
        );
        assert!(
            methods.contains("session.restore"),
            "session.restore override registered (B1: transport-owned handler kept, \
             same wire shape as the embedded route)"
        );
        assert!(
            methods.contains("approval.decide"),
            "approval.decide override registered (B4-a: the EMB backfill mirrors \
             this transport-owned handler wire-for-wire)"
        );
        assert!(
            methods.contains("question.answer"),
            "question.answer override registered (B4-a: the EMB backfill mirrors \
             this transport-owned handler wire-for-wire)"
        );
        assert!(
            methods.contains("question.dismiss"),
            "question.dismiss override registered (B4-a: the EMB backfill mirrors \
             this transport-owned handler wire-for-wire)"
        );
        // The W2 deletion is pinned the other way: NO workspace method may
        // join the transport registry — the serve wire answers them through
        // the engine fall-through only.
        for method in [
            "workspace.createOrTouch",
            "workspace.update",
            "workspace.delete",
            "workspace.list",
            "workspace.get",
            "workspace.handlerFor",
        ] {
            assert!(
                !methods.contains(method),
                "{method}: a workspace method must NOT be transport-registered after the W2 unseal"
            );
        }

        // ...and every server-only fact row is registered.
        for name in &server_only {
            assert!(
                methods.contains(name),
                "{name}: a server-only fact row must be transport-registered"
            );
        }
    }

    // =======================================================================
    // session.get — restore-on-demand (persisted-only sessions)
    // =======================================================================

    /// A restore hook that makes the addressed session live by creating it
    /// under the given id — the transport-level stand-in for the engine's
    /// full `session.restore` assembly (the wiring under test is the
    /// miss → hook → retry sequence, not the resume itself).
    fn restoring_hook(
        manager: &Arc<SessionManager>,
        calls: Arc<std::sync::Mutex<Vec<String>>>,
    ) -> crate::rpc::EngineRestoreHook {
        let manager = Arc::clone(manager);
        Arc::new(move |session_id| {
            let manager = Arc::clone(&manager);
            let calls = Arc::clone(&calls);
            let session_id_clone = session_id.clone();
            Box::pin(async move {
                calls
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(session_id_clone);
                let now = Utc::now();
                manager
                    .create_session_with_id(
                        SessionId::new(&session_id),
                        SessionMetadata {
                            title: "Restored".to_string(),
                            is_custom_title: false,
                            created_at: now,
                            updated_at: now,
                            tags: Default::default(),
                            archived: false,
                            work_dir: None,
                            last_prompt: None,
                            forked_from: None,
                            cache_root_id: None,
                            custom: None,
                        },
                        SessionScopeConfig::default(),
                    )
                    .await
                    .map(|_| ())
                    .map_err(|_| {
                        mirri_transport::adapter::EngineError::Internal(format!(
                            "restore failed for {session_id}"
                        ))
                    })
            })
        })
    }

    /// A session manager bound to a throwaway workspace (no session created).
    fn session_manager_with_workspace() -> (tempfile::TempDir, Arc<SessionManager>) {
        let tmp = tempfile::tempdir().expect("tempdir must be creatable");
        let workspace = Arc::new(WorkspaceScope::new(
            WorkspaceId::new("test-ws"),
            tmp.path().to_path_buf(),
            WorkspaceScopeConfig::default(),
        ));
        (tmp, Arc::new(SessionManager::new(workspace)))
    }

    #[tokio::test]
    async fn should_resume_a_persisted_session_through_the_restore_hook_when_session_get_misses() {
        // Given a manager with no live session, and a get handler whose
        // restore hook makes the addressed session live
        let (_tmp, manager) = session_manager_with_workspace();
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let handler = GetSessionHandler::new(
            Arc::clone(&manager),
            Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            Arc::new(SessionEventChannelRegistry::new(16)),
            "test-epoch".to_string().into(),
            noop_engine(),
        )
        .with_restore_hook(Some(restoring_hook(&manager, Arc::clone(&calls))));

        // When the persisted-only session is fetched
        let value = handler
            .handle(json!({ "sessionId": "session-restored-on-demand" }))
            .await
            .expect("get resolves after restore");

        // Then the hook ran for that id and the session view is served
        assert_eq!(
            *calls.lock().unwrap_or_else(|e| e.into_inner()),
            vec!["session-restored-on-demand".to_string()]
        );
        assert_eq!(value["id"], json!("session-restored-on-demand"));
    }

    // =======================================================================
    // session.get — routing injection (W3 session-routing seam)
    // =======================================================================

    /// Given a handler bound to one manager but routed to another (the
    /// composition root's registry-backed routing), when `session.get`
    /// addresses a session that lives only under the routed manager, then
    /// the resolution follows the injected routing — never the handler's
    /// bound default manager.
    #[tokio::test]
    async fn should_resolve_session_get_through_the_injected_routing() {
        // Given a session under the default manager and another under a
        // second (routed) manager
        let (_tmp_a, manager_a, session_a) = session_manager_with_session().await;
        let (_tmp_b, manager_b, session_b) = session_manager_with_session().await;

        // And a get handler BOUND to manager A but ROUTED to manager B (the
        // registry-backed injection the composition root installs)
        let handler = GetSessionHandler::new(
            Arc::clone(&manager_a),
            Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            Arc::new(SessionEventChannelRegistry::new(16)),
            "test-epoch".to_string().into(),
            noop_engine(),
        )
        .with_routing(Arc::new(SingleManagerRouting::live(Arc::clone(&manager_b))));

        // When the routed manager's session is fetched, then the view is
        // served from manager B's session
        let value = handler
            .handle(json!({ "sessionId": session_b }))
            .await
            .expect("the routed session resolves");
        assert_eq!(value["id"], json!(session_b));

        // And the bound (default) manager's session is no longer reachable —
        // the routing replaced the single-manager resolution, proving the
        // injection is real (not a silent default fallback)
        let result = handler.handle(json!({ "sessionId": session_a })).await;
        assert!(
            matches!(result, Err(RpcError::NotFound(_))),
            "the bound default manager's session must not resolve through the injected routing"
        );
    }

    #[tokio::test]
    async fn should_return_not_found_when_the_restore_hook_cannot_make_the_session_live() {
        // Given a manager with no live session and a get handler whose
        // restore hook does nothing (an unknown or corrupt session)
        let (_tmp, manager) = session_manager_with_workspace();
        let noop: crate::rpc::EngineRestoreHook = Arc::new(|_| Box::pin(async { Ok(()) }));
        let handler = GetSessionHandler::new(
            Arc::clone(&manager),
            Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            Arc::new(SessionEventChannelRegistry::new(16)),
            "test-epoch".to_string().into(),
            noop_engine(),
        )
        .with_restore_hook(Some(noop));

        // When the unknown session is fetched
        let err = handler
            .handle(json!({ "sessionId": "session-never-existed" }))
            .await
            .expect_err("get still misses after the hook");

        // Then the NotFound error names the session
        match err {
            RpcError::NotFound(id) => assert_eq!(id, "session-never-existed"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn should_resume_a_persisted_session_through_the_restore_hook_when_snapshotting() {
        // Given a manager with no live session, and a snapshot handler whose
        // restore hook makes the addressed session live
        let (_tmp, manager) = session_manager_with_workspace();
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let handler = SnapshotSessionHandler::new(
            Arc::clone(&manager),
            Arc::new(SessionEventChannelRegistry::new(16)),
            Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            "test-epoch".to_string().into(),
            noop_engine(),
        )
        .with_restore_hook(Some(restoring_hook(&manager, Arc::clone(&calls))));

        // When the persisted-only session is snapshotted
        let value = handler
            .handle(json!({ "sessionId": "session-snapshotted-after-restore" }))
            .await
            .expect("snapshot resolves after restore");

        // Then the hook ran for that id and the snapshot view is served
        assert_eq!(
            *calls.lock().unwrap_or_else(|e| e.into_inner()),
            vec!["session-snapshotted-after-restore".to_string()]
        );
        assert_eq!(
            value["session"]["id"],
            json!("session-snapshotted-after-restore")
        );
    }

    #[tokio::test]
    async fn should_stay_not_found_when_a_session_closed_in_this_process_is_fetched() {
        // Given a live session that this process then closes
        let (_tmp, manager, session_id) = session_manager_with_session().await;
        manager
            .close_session(
                &SessionId::new(&session_id),
                mirri_core::session::CloseReason::UserInitiated,
            )
            .await
            .expect("close resolves");
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let handler = GetSessionHandler::new(
            Arc::clone(&manager),
            Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            Arc::new(SessionEventChannelRegistry::new(16)),
            "test-epoch".to_string().into(),
            noop_engine(),
        )
        .with_restore_hook(Some(restoring_hook(&manager, Arc::clone(&calls))));

        // When the closed session is fetched
        let err = handler
            .handle(json!({ "sessionId": session_id }))
            .await
            .expect_err("a closed session stays not found");

        // Then the NotFound error is served and the restore hook never ran
        match err {
            RpcError::NotFound(_) => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
        assert!(
            calls.lock().unwrap_or_else(|e| e.into_inner()).is_empty(),
            "the restore hook must not run for a closed session"
        );
    }
}
