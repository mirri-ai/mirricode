//! Axum HTTP server exposing the RPC method table as REST-style routes
//! under `/api/v1/`, plus the multiplexed WebSocket endpoint (`/api/v1/ws`,
//! see [`crate::ws`]).
//!
//! Every route answers with the `{code, msg, data, request_id}` envelope
//! (matching the TS server): `code: 0` means success, non-zero codes follow
//! JSON-RPC 2.0 conventions (`-326xx` protocol errors, `-32xxx` application
//! errors). The HTTP status code mirrors the RPC error class (404/400/401/500).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{ConnectInfo, DefaultBodyLimit, Path, State},
    http::{HeaderMap, StatusCode},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};
use uuid::Uuid;

use crate::auth;
use crate::error::ServerError;
use crate::rpc::{ApiErrorResponse, ApiResponse, RpcError};
use crate::state::ServerState;

/// Build the axum router for the HTTP API.
///
/// The [`auth::require_auth`] middleware protects every route except the
/// public `healthz`/`meta` probes; `POST /api/v1/shutdown` additionally
/// rejects non-loopback peers when no token is configured (see
/// [`auth::shutdown_request_authorized`]). CORS is same-origin only until an
/// explicit origin allow-list is configured.
pub fn router(state: ServerState, web_dir: Option<PathBuf>) -> Router {
    let api_routes = Router::new()
        .route("/api/v1/healthz", get(healthz))
        .route("/api/v1/meta", get(meta))
        .route("/api/v1/auth", get(auth_summary))
        .route("/api/v1/sessions", post(create_session).get(list_sessions))
        .route(
            "/api/v1/sessions/{id}",
            get(get_session).delete(close_session),
        )
        .route("/api/v1/sessions/{id}/messages", post(send_message))
        .route(
            "/api/v1/sessions/{id}/approvals/{rid}",
            post(resolve_approval),
        )
        .route(
            "/api/v1/sessions/{id}/questions/{rid}",
            post(answer_question),
        )
        .route("/api/v1/sessions/{id}/tools", get(list_tools))
        // Web terminal panel (protocol `rest/terminal.ts`): the terminal
        // pool lives on the session scope, so these are local handlers —
        // they do not go through the shared RPC registry. The v1 REST
        // contract spells close as a `{tid}:close` suffix, but axum's path
        // syntax cannot express a suffix after a capture — the rust
        // backend serves it as a `/close` subpath instead, and the rust
        // web client (WsRpcApi, the only consumer) uses that spelling.
        .route(
            "/api/v1/sessions/{id}/terminals",
            get(list_terminals).post(create_terminal),
        )
        .route("/api/v1/sessions/{id}/terminals/{tid}", get(get_terminal))
        .route(
            "/api/v1/sessions/{id}/terminals/{tid}/close",
            post(close_terminal),
        )
        .route("/api/v1/ws", get(crate::ws::handle_ws))
        // Unified JSON-RPC 2.0 endpoint (design 01-protocol-unification.md):
        // forward RPC, event stream, and reverse RPC over one connection.
        .route("/api/v1/rpc", get(crate::ws_rpc::handle_ws_rpc))
        // Files REST surface (P4, the v1 TS `/files` routes): multipart
        // upload and binary download/delete over the shared files store.
        // Like the terminal routes these are local handlers — multipart
        // bodies and binary responses have no JSON-RPC wire form. The
        // upload route disables the default body limit to match the TS
        // server's uncapped uploads (`Number.MAX_SAFE_INTEGER`) and
        // consumes the body as a stream, so the memory profile stays
        // bounded by the chunk cadence, not the file size; the auth
        // middleware above still gates every request.
        .route(
            "/api/v1/files",
            post(crate::files::upload_file).layer(DefaultBodyLimit::disable()),
        )
        .route(
            "/api/v1/files/{id}",
            get(crate::files::get_file).delete(crate::files::delete_file),
        )
        .route("/api/v1/workspaces", get(list_workspaces))
        .route("/api/v1/config", get(get_config))
        // Agent profiles REST surface (P6, the web `/agents` contract —
        // `packages/kap-server/src/routes/agents.ts`): the five CRUD
        // routes dispatch through the engine's `agent.*Profile` methods,
        // answering the TS `ProfileEntry` wire shape (snake_case) in the
        // standard envelope.
        .route(
            "/api/v1/agents",
            get(list_agent_profiles).post(create_agent_profile),
        )
        .route(
            "/api/v1/agents/{name}",
            get(get_agent_profile)
                .put(update_agent_profile)
                .delete(delete_agent_profile),
        )
        .route("/api/v1/shutdown", post(shutdown));

    let router = Router::new()
        .merge(api_routes)
        // Same-origin only: no cross-origin browser clients are allowed until
        // an explicit origin allow-list is configured (was permissive).
        .layer(CorsLayer::new())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_auth,
        ))
        .with_state(state);

    // When a web directory is provided, serve the SPA build alongside the
    // API: non-API routes fall through to the static file server, with an
    // SPA history-mode fallback that serves `index.html` for unknown paths.
    // Without a directory, a full build (SPA embedded at compile time)
    // serves the embedded assets; a lite build serves the API only.
    if let Some(dir) = web_dir {
        router
            .fallback_service(ServeDir::new(&dir).fallback(ServeFile::new(dir.join("index.html"))))
    } else {
        crate::web_embed::attach_embedded_fallback(router)
    }
}

/// Serve the HTTP API on the given listener until a graceful shutdown is
/// requested through [`ServerState::request_shutdown`]. When `web_dir` is
/// provided, non-API routes serve the SPA build with history-mode fallback.
pub async fn serve(
    listener: TcpListener,
    state: ServerState,
    web_dir: Option<PathBuf>,
) -> Result<(), ServerError> {
    let mut shutdown_rx = state.shutdown_rx();
    axum::serve(
        listener,
        router(state, web_dir).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        let _ = shutdown_rx.changed().await;
    })
    .await
    .map_err(|e| ServerError::BindFailed(e.to_string()))
}

// ---------------------------------------------------------------------------
// Infra routes (no RPC dispatch)
// ---------------------------------------------------------------------------

/// Wire shape of the health response.
#[derive(Debug, Clone, Serialize)]
struct HealthzData {
    ok: bool,
    ready: bool,
}

/// `GET /api/v1/healthz` — liveness/readiness probe.
async fn healthz(headers: HeaderMap) -> Json<ApiResponse<HealthzData>> {
    let request_id = request_id_from(&headers);
    Json(ApiResponse::ok(
        HealthzData {
            ok: true,
            ready: true,
        },
        &request_id,
    ))
}

/// Wire shape of the auth readiness summary, byte-compatible with the
/// TypeScript server's v1 `AuthSummary` contract (`GET /api/v1/auth`): the
/// web UI's first-paint probe decides onboarding vs. chat UI from it.
///
/// The route is NOT a public probe: without a valid bearer credential the
/// auth middleware rejects with 401, which is exactly the signal the web
/// client uses to show its server-token dialog.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub(crate) struct AuthSummaryData {
    ready: bool,
    providers_count: usize,
    default_model: Option<String>,
}

/// Project the v1 auth summary from the config.
///
/// `ready` mirrors the TS semantics: at least one provider and a
/// resolvable default model.
pub(crate) fn project_auth_summary(config: &mirri_core::ServerConfig) -> AuthSummaryData {
    let mut providers_count = config.providers.as_ref().map_or(0, |m| m.len());
    // Legacy single-provider section counts once when it carries a key or
    // endpoint.
    if let Some(provider) = &config.provider
        && (provider.api_key.as_deref().is_some_and(|k| !k.is_empty())
            || provider.base_url.as_deref().is_some_and(|u| !u.is_empty()))
    {
        providers_count += 1;
    }

    let default_model = config
        .default_model
        .as_deref()
        .or(config.model.as_ref().map(|m| m.id.as_str()))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);

    let ready = providers_count >= 1 && default_model.is_some();

    AuthSummaryData {
        ready,
        providers_count,
        default_model,
    }
}

/// `GET /api/v1/auth` — auth readiness summary (v1 `AuthSummary` shape).
async fn auth_summary(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> Json<ApiResponse<AuthSummaryData>> {
    let request_id = request_id_from(&headers);
    let config = state
        .config
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    Json(ApiResponse::ok(project_auth_summary(&config), &request_id))
}

/// Wire shape of the metadata response — the v1 REST contract the protocol
/// schema (`metaResponseSchema`) and the live e2e suites pin is snake_case
/// (`server_version` / `server_id` / `started_at`), matching the TypeScript
/// kap-server's `/meta` byte-for-byte.
#[derive(Debug, Clone, Serialize)]
struct MetaData {
    server_version: String,
    server_id: String,
    started_at: String,
    /// Identifies the server implementation so the web client can pick the
    /// right transport ("v3" (the Rust core) → single WS JSON-RPC;
    /// "v1"/"v2" → REST+WS).
    backend: String,
    /// Feature capability flags advertised to the client. The web UI uses
    /// these to show/hide features that are not yet implemented on the Rust
    /// server (e.g. terminal, mcp, file_upload).
    capabilities: Capabilities,
    /// Entries the web "open in" affordance offers; the Rust server does not
    /// enumerate apps yet, so the array is empty (the web hides the buttons).
    open_in_apps: Vec<String>,
    /// True when the server runs in open (development) mode — no bearer token
    /// is enforced, so the web UI can skip the token prompt.
    dangerous_bypass_auth: bool,
}

/// Feature flags advertised via `/meta.capabilities`.
#[derive(Debug, Clone, Serialize)]
struct Capabilities {
    websocket: bool,
    fs_query: bool,
    mcp: bool,
    background_tasks: bool,
    tasks: bool,
    #[serde(rename = "file_upload")]
    file_upload: bool,
    terminal: bool,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            websocket: true,
            fs_query: true,
            mcp: false,
            background_tasks: true,
            tasks: true,
            // The files REST surface is served by this server (the
            // `POST/GET /api/v1/files` routes plus the `message.send`
            // structured parts entry resolution), so the web attachment
            // bar is enabled.
            file_upload: true,
            // The web terminal panel is served by this server (the
            // `/terminals` REST routes plus the `terminal_*` WS frames on
            // `/api/v1/rpc`).
            terminal: true,
        }
    }
}

/// `GET /api/v1/meta` — server identity and start time.
async fn meta(State(state): State<ServerState>, headers: HeaderMap) -> Json<ApiResponse<MetaData>> {
    let request_id = request_id_from(&headers);
    let mcp_enabled = match &state.mcp_manager {
        Some(mgr) => mgr.has_connected().await,
        None => false,
    };
    let caps = Capabilities {
        mcp: mcp_enabled,
        ..Default::default()
    };
    Json(ApiResponse::ok(
        MetaData {
            server_version: state.version,
            server_id: state.server_id,
            started_at: state.started_at.to_rfc3339(),
            backend: "v3".to_string(),
            capabilities: caps,
            open_in_apps: Vec::new(),
            dangerous_bypass_auth: state.auth_token.is_none(),
        },
        &request_id,
    ))
}

/// `POST /api/v1/shutdown` — request a graceful shutdown of every transport.
///
/// This is the one route that is never public (see
/// [`auth::shutdown_request_authorized`]): with a token configured the
/// caller must present it; in open (no-token) mode only loopback peers may
/// shut the server down.
async fn shutdown(
    State(state): State<ServerState>,
    headers: HeaderMap,
    ConnectInfo(peer_addr): ConnectInfo<SocketAddr>,
) -> Response {
    let request_id = request_id_from(&headers);
    if !auth::shutdown_request_authorized(&state, &headers, Some(peer_addr)) {
        tracing::warn!(%peer_addr, "rejected shutdown request without a valid token");
        return auth::unauthorized_response(&request_id);
    }
    state.request_shutdown();
    (
        StatusCode::OK,
        Json(ApiResponse::ok(json!({ "ok": true }), &request_id)),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// RPC dispatch helpers
// ---------------------------------------------------------------------------

/// Dispatch one RPC call through the shared dispatch (transport registry
/// first, then the engine fall-through gated on the `mirri_proto::rpc`
/// availability facts).
async fn rpc_call(
    state: &ServerState,
    method: &'static str,
    params: Value,
) -> Result<Value, RpcError> {
    state.dispatch(method, params).await
}

/// Wrap an RPC result in the `{code, msg, data, request_id}` envelope.
/// The HTTP status code is always 200 (matching the TS server convention —
/// the business outcome lives in `envelope.code`).
fn rpc_response(
    method: &'static str,
    result: Result<Value, RpcError>,
    request_id: &str,
) -> Response {
    match result {
        Ok(data) => {
            let envelope =
                serde_json::to_value(ApiResponse::ok(data, request_id)).unwrap_or_default();
            (StatusCode::OK, Json(envelope)).into_response()
        }
        Err(error) => {
            // RPC errors are returned to the client in the envelope; log them
            // server-side too, correlated by request id, so failures are
            // observable without client cooperation.
            tracing::error!(request_id = %request_id, method = %method, error = %error, "rpc error");
            let envelope = serde_json::to_value(ApiErrorResponse::error(&error, request_id))
                .unwrap_or_default();
            // TS server always returns HTTP 200 — the error code is in the envelope.
            (StatusCode::OK, Json(envelope)).into_response()
        }
    }
}

/// Extract the client-supplied request id from `x-request-id` (echoed back in
/// the envelope), or mint a fresh UUID when absent.
pub(crate) fn request_id_from(headers: &HeaderMap) -> String {
    headers
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .filter(|request_id| !request_id.is_empty())
        .unwrap_or_else(|| Uuid::new_v4().to_string())
}

/// Merge a `session_id` from the URL path into the JSON body.
fn session_scoped_params(session_id: &str, body: Value) -> Value {
    match body {
        Value::Object(mut fields) => {
            fields.insert(
                "sessionId".to_string(),
                Value::String(session_id.to_string()),
            );
            Value::Object(fields)
        }
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Session routes
// ---------------------------------------------------------------------------

/// `POST /api/v1/sessions` → `session.create`
async fn create_session(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(params): Json<Value>,
) -> Response {
    let request_id = request_id_from(&headers);
    let result = rpc_call(&state, "session.create", params).await;
    rpc_response("session.create", result, &request_id)
}

/// `GET /api/v1/sessions` → `session.list`
async fn list_sessions(State(state): State<ServerState>, headers: HeaderMap) -> Response {
    let request_id = request_id_from(&headers);
    let result = rpc_call(&state, "session.list", Value::Null).await;
    rpc_response("session.list", result, &request_id)
}

/// `GET /api/v1/sessions/{id}` → `session.get`
async fn get_session(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> Response {
    let request_id = request_id_from(&headers);
    let result = rpc_call(&state, "session.get", json!({ "sessionId": session_id })).await;
    rpc_response("session.get", result, &request_id)
}

/// `DELETE /api/v1/sessions/{id}` → `session.close`
async fn close_session(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> Response {
    let request_id = request_id_from(&headers);
    let result = rpc_call(&state, "session.close", json!({ "sessionId": session_id })).await;
    rpc_response("session.close", result, &request_id)
}

/// `POST /api/v1/sessions/{id}/messages` → `message.send`
async fn send_message(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let request_id = request_id_from(&headers);
    let params = session_scoped_params(&session_id, body);
    let result = rpc_call(&state, "message.send", params).await;
    rpc_response("message.send", result, &request_id)
}

/// `POST /api/v1/sessions/{id}/approvals/{rid}` → `approval.decide`
async fn resolve_approval(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path((session_id, request_id)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Response {
    let request_id_header = request_id_from(&headers);
    let mut params = session_scoped_params(&session_id, body);
    if let Value::Object(fields) = &mut params {
        fields.insert("requestId".to_string(), Value::String(request_id));
    }
    let result = rpc_call(&state, "approval.decide", params).await;
    rpc_response("approval.decide", result, &request_id_header)
}

/// `POST /api/v1/sessions/{id}/questions/{rid}` → `question.answer`
async fn answer_question(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path((session_id, request_id)): Path<(String, String)>,
    Json(body): Json<Value>,
) -> Response {
    let request_id_header = request_id_from(&headers);
    let mut params = session_scoped_params(&session_id, body);
    if let Value::Object(fields) = &mut params {
        fields.insert("requestId".to_string(), Value::String(request_id));
    }
    let result = rpc_call(&state, "question.answer", params).await;
    rpc_response("question.answer", result, &request_id_header)
}

/// `GET /api/v1/sessions/{id}/tools` → `tool.list`
async fn list_tools(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> Response {
    let request_id = request_id_from(&headers);
    let result = rpc_call(&state, "tool.list", json!({ "sessionId": session_id })).await;
    rpc_response("tool.list", result, &request_id)
}

// ---------------------------------------------------------------------------
// Terminal routes (web terminal panel — local handlers over the session's
// TerminalPool, not the shared RPC registry)
// ---------------------------------------------------------------------------

/// Resolve a live session's scope for the terminal routes, answering the
/// same not-found shape the RPC-backed routes use. Routing follows the W3
/// seam (`session_routing`) with restore-on-demand — a session of a
/// non-default workspace resolves on its own manager, and a persisted-only
/// session (a restart left it on disk) is resumed before answering, exactly
/// like `session.subscribe` and the fire-mode watch do.
async fn terminal_session(
    state: &ServerState,
    session_id: &str,
) -> Result<Arc<mirri_core::SessionScope>, RpcError> {
    crate::rpc::live_or_restored_session(state.session_routing.as_ref(), session_id)
        .await
        .ok_or_else(|| RpcError::NotFound(format!("session not found: {session_id}")))
}

/// Project one pool snapshot onto the protocol's terminal wire shape
/// (`rest/terminal.ts`, snake_case). `cwd` is reported relative to the
/// session workspace (the schema rejects absolute paths); the pool stores
/// the absolute spawn directory, so the workspace root is stripped back
/// off — the root itself reads as `"."`.
fn terminal_wire(
    session_id: &str,
    info: &mirri_core::TerminalInfo,
    workspace_root: Option<&std::path::Path>,
) -> Value {
    let cwd = match workspace_root
        .and_then(|root| std::path::Path::new(&info.cwd).strip_prefix(root).ok())
    {
        Some(relative) if !relative.as_os_str().is_empty() => {
            relative.to_string_lossy().to_string()
        }
        Some(_) => ".".to_string(),
        None => info.cwd.clone(),
    };
    let mut fields = serde_json::Map::new();
    fields.insert("id".to_string(), json!(info.id));
    fields.insert("session_id".to_string(), json!(session_id));
    fields.insert("cwd".to_string(), json!(cwd));
    fields.insert("shell".to_string(), json!(info.shell));
    fields.insert("cols".to_string(), json!(info.cols));
    fields.insert("rows".to_string(), json!(info.rows));
    match info.status {
        mirri_core::terminal_pool::TerminalStatus::Running => {
            fields.insert("status".to_string(), json!("running"));
        }
        mirri_core::terminal_pool::TerminalStatus::Exited(code) => {
            fields.insert("status".to_string(), json!("exited"));
            // The protocol's `exit_code` is `int | null` and optional; the
            // exited spelling always carries it (null for an unknown code).
            fields.insert("exit_code".to_string(), json!(code));
        }
    }
    fields.insert(
        "created_at".to_string(),
        json!(info.created_at.to_rfc3339()),
    );
    if let Some(closed_at) = info.closed_at {
        fields.insert("exited_at".to_string(), json!(closed_at.to_rfc3339()));
    }
    Value::Object(fields)
}

/// Find one terminal's snapshot in the pool's list (the pool exposes no
/// single-terminal lookup; the list is bounded by `DEFAULT_MAX_TERMINALS`).
async fn find_terminal(
    pool: &Arc<mirri_core::terminal_pool::TerminalPool>,
    terminal_id: &str,
) -> Result<mirri_core::TerminalInfo, RpcError> {
    pool.list_terminals()
        .await
        .into_iter()
        .find(|info| info.id == terminal_id)
        .ok_or_else(|| RpcError::NotFound(format!("terminal not found: {terminal_id}")))
}

/// `GET /api/v1/sessions/{id}/terminals` → `{items: [Terminal]}`
async fn list_terminals(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> Response {
    let request_id = request_id_from(&headers);
    let result = async {
        let session = terminal_session(&state, &session_id).await?;
        let root = session.workspace_root().map(std::path::Path::to_path_buf);
        let items: Vec<Value> = session
            .terminal_pool()
            .list_terminals()
            .await
            .iter()
            .map(|info| terminal_wire(&session_id, info, root.as_deref()))
            .collect();
        Ok(json!({ "items": items }))
    }
    .await;
    rpc_response("terminal.list", result, &request_id)
}

/// `POST /api/v1/sessions/{id}/terminals` body `{cwd?, shell?, cols?, rows?}` → `Terminal`
async fn create_terminal(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let request_id = request_id_from(&headers);
    let result = async {
        let session = terminal_session(&state, &session_id).await?;
        // The protocol's `createTerminalRequestSchema`: a relative `cwd`
        // (absolute paths are rejected), an optional `shell`, and positive
        // integer dimensions.
        let cwd = body
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap_or(".")
            .to_string();
        let cwd_path = std::path::PathBuf::from(&cwd);
        if cwd_path.is_absolute() {
            return Err(RpcError::InvalidParams(
                "cwd must be relative to the session workspace".to_string(),
            ));
        }
        let shell = body.get("shell").and_then(Value::as_str).map(String::from);
        let dimension = |key: &str, default: u16| -> Result<u16, RpcError> {
            match body.get(key) {
                None | Some(Value::Null) => Ok(default),
                Some(value) => value
                    .as_u64()
                    .filter(|n| (1..=u64::from(u16::MAX)).contains(n))
                    .map(|n| n as u16)
                    .ok_or_else(|| {
                        RpcError::InvalidParams(format!("'{key}' must be a positive integer"))
                    }),
            }
        };
        let cols = dimension("cols", 80)?;
        let rows = dimension("rows", 24)?;
        let workspace_root = session
            .workspace_root()
            .map(std::path::Path::to_path_buf)
            .ok_or_else(|| {
                RpcError::internal("the session has no workspace root; terminals cannot be created")
            })?;
        let pool = session.terminal_pool();
        let spawn_cwd = workspace_root.join(&cwd_path);
        let terminal_id = pool
            .create_terminal(spawn_cwd, shell, cols, rows)
            .await
            .map_err(|e| RpcError::internal(e.to_string()))?;
        let info = find_terminal(&pool, &terminal_id).await?;
        Ok(terminal_wire(
            &session_id,
            &info,
            Some(workspace_root.as_path()),
        ))
    }
    .await;
    rpc_response("terminal.create", result, &request_id)
}

/// `GET /api/v1/sessions/{id}/terminals/{tid}` → `Terminal`
async fn get_terminal(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path((session_id, terminal_id)): Path<(String, String)>,
) -> Response {
    let request_id = request_id_from(&headers);
    let result = async {
        let session = terminal_session(&state, &session_id).await?;
        let pool = session.terminal_pool();
        let info = find_terminal(&pool, &terminal_id).await?;
        Ok(terminal_wire(&session_id, &info, session.workspace_root()))
    }
    .await;
    rpc_response("terminal.get", result, &request_id)
}

/// `POST /api/v1/sessions/{id}/terminals/{tid}/close` → `{closed: true}`
async fn close_terminal(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path((session_id, terminal_id)): Path<(String, String)>,
) -> Response {
    let request_id = request_id_from(&headers);
    let result = async {
        let session = terminal_session(&state, &session_id).await?;
        session
            .terminal_pool()
            .kill(&terminal_id)
            .await
            .map_err(|e| RpcError::NotFound(e.to_string()))?;
        Ok(json!({ "closed": true }))
    }
    .await;
    rpc_response("terminal.close", result, &request_id)
}

// ---------------------------------------------------------------------------
// Global routes
// ---------------------------------------------------------------------------

/// `GET /api/v1/workspaces` → `workspace.list`
async fn list_workspaces(State(state): State<ServerState>, headers: HeaderMap) -> Response {
    let request_id = request_id_from(&headers);
    let result = rpc_call(&state, "workspace.list", Value::Null).await;
    rpc_response("workspace.list", result, &request_id)
}

/// `GET /api/v1/config` → `config.get`
async fn get_config(State(state): State<ServerState>, headers: HeaderMap) -> Response {
    let request_id = request_id_from(&headers);
    let result = rpc_call(&state, "config.get", Value::Null).await;
    rpc_response("config.get", result, &request_id)
}

// ---------------------------------------------------------------------------
// Agent profile routes (the web `/agents` REST contract, P6)
// ---------------------------------------------------------------------------

/// `GET /api/v1/agents` → `agent.listProfiles`, which answers the v1
/// `{items: [...], skipped: [{path, reason}]}` envelope the web client
/// reads (R3-6) — passed through verbatim.
async fn list_agent_profiles(State(state): State<ServerState>, headers: HeaderMap) -> Response {
    let request_id = request_id_from(&headers);
    let result = match rpc_call(&state, "agent.listProfiles", Value::Null).await {
        Ok(response @ Value::Object(_)) => Ok(response),
        Ok(other) => Err(RpcError::internal(format!(
            "agent.listProfiles answered a non-object: {other}"
        ))),
        Err(error) => Err(error),
    };
    rpc_response("agent.listProfiles", result, &request_id)
}

/// `GET /api/v1/agents/{name}` → `agent.getProfile`
async fn get_agent_profile(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    let request_id = request_id_from(&headers);
    let result = rpc_call(&state, "agent.getProfile", json!({ "name": name })).await;
    rpc_response("agent.getProfile", result, &request_id)
}

/// `POST /api/v1/agents` → `agent.createProfile` (body `{name, ...fields}`)
async fn create_agent_profile(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let request_id = request_id_from(&headers);
    let result = rpc_call(&state, "agent.createProfile", body).await;
    rpc_response("agent.createProfile", result, &request_id)
}

/// `PUT /api/v1/agents/{name}` → `agent.updateProfile` (the path name is
/// merged into the partial body)
async fn update_agent_profile(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(name): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let request_id = request_id_from(&headers);
    let params = match body {
        Value::Object(mut fields) => {
            fields.insert("name".to_string(), Value::String(name));
            Value::Object(fields)
        }
        Value::Null => json!({ "name": name }),
        other => other,
    };
    let result = rpc_call(&state, "agent.updateProfile", params).await;
    rpc_response("agent.updateProfile", result, &request_id)
}

/// `DELETE /api/v1/agents/{name}` → `agent.deleteProfile`
async fn delete_agent_profile(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Path(name): Path<String>,
) -> Response {
    let request_id = request_id_from(&headers);
    let result = rpc_call(&state, "agent.deleteProfile", json!({ "name": name })).await;
    rpc_response("agent.deleteProfile", result, &request_id)
}
