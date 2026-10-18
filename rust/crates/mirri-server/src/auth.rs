//! Bearer-token authentication for the HTTP API.
//!
//! [`require_auth`] runs as router middleware on every route:
//!
//! - the public probes `/api/v1/healthz` and `/api/v1/meta` never require a
//!   token, so liveness/readiness checks and server identity stay reachable;
//! - with a shared token configured on [`ServerState`], every other route
//!   requires `Authorization: Bearer <token>`; a missing or wrong token gets
//!   the standard 401 error envelope ([`unauthorized_response`]);
//! - without a configured token the server runs in open (development) mode
//!   and every route is reachable — except `POST /api/v1/shutdown`, which
//!   additionally demands a valid token or a loopback peer (see
//!   [`shutdown_request_authorized`]).
//!
//! The startup warning for open mode is emitted by the binary (mirri-cli),
//! not by this library.

use std::net::SocketAddr;

use axum::Json;
use axum::extract::Request;
use axum::extract::State;
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use subtle::ConstantTimeEq;

use crate::http::request_id_from;
use crate::rpc::{ApiErrorResponse, RpcError};
use crate::state::ServerState;

/// Routes that stay unauthenticated: they expose no session data and must
/// remain reachable by infrastructure regardless of auth configuration.
const PUBLIC_ROUTES: &[&str] = &["/api/v1/healthz", "/api/v1/meta"];

/// The unified JSON-RPC WebSocket endpoint authenticates itself through the
/// `Sec-WebSocket-Protocol` bearer subprotocol (the browser WebSocket API
/// cannot set the `Authorization` header; design §13.1), so the HTTP
/// middleware must not pre-empt it — [`crate::ws_rpc::handle_ws_rpc`] rejects
/// the upgrade itself with the same 401 when the token is missing or wrong.
const WS_RPC_ROUTE: &str = "/api/v1/rpc";

/// Scheme prefix of the `Authorization` header this server accepts
/// (case-sensitive, per the HTTP scheme syntax).
const BEARER_SCHEME: &str = "Bearer ";

/// Auth middleware — skips public routes and the self-authenticating WS RPC
/// route, validates Bearer token on the rest.
pub async fn require_auth(
    State(state): State<ServerState>,
    request: Request,
    next: Next,
) -> Response {
    // Public probes are always reachable, even when a token is configured.
    if is_public_path(request.uri().path()) || request.uri().path() == WS_RPC_ROUTE {
        return next.run(request).await;
    }

    // No token configured → open (development) mode: every route is open.
    let Some(expected_token) = &state.auth_token else {
        return next.run(request).await;
    };

    match extract_bearer_token(request.headers()) {
        Some(supplied_token) if token_matches(supplied_token, expected_token) => {
            next.run(request).await
        }
        _ => {
            let request_id = request_id_from(request.headers());
            unauthorized_response(&request_id)
        }
    }
}

/// Whether the request path is one of the unauthenticated public routes.
pub fn is_public_path(path: &str) -> bool {
    PUBLIC_ROUTES.contains(&path)
}

/// Extract the token from `Authorization: Bearer <token>`.
///
/// Returns `None` for a missing header, a non-UTF-8 header value, a scheme
/// other than `Bearer`, or an empty token — all of which fail authentication.
pub fn extract_bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let token = value.strip_prefix(BEARER_SCHEME)?;
    (!token.is_empty()).then_some(token)
}

/// Compare two tokens in constant time, so a timing side channel cannot
/// reveal how much of the secret prefix matched. Only the length comparison
/// is non-constant, and length is not secret (both sides are visible to the
/// caller). Shared with the WebSocket RPC endpoint's bearer-subprotocol
/// check ([`crate::ws_rpc`]).
pub(crate) fn token_matches(supplied: &str, expected: &str) -> bool {
    let supplied = supplied.as_bytes();
    let expected = expected.as_bytes();
    supplied.len() == expected.len() && supplied.ct_eq(expected).into()
}

/// The standard 401 error envelope for missing or invalid credentials:
/// HTTP status 401 with `{code: -32002, msg: "unauthorized", data: null,
/// request_id}` (see [`ApiErrorResponse`]).
pub fn unauthorized_response(request_id: &str) -> Response {
    let envelope = ApiErrorResponse::error(&RpcError::Unauthorized, request_id);
    (StatusCode::UNAUTHORIZED, Json(envelope)).into_response()
}

/// Whether a `POST /api/v1/shutdown` request may shut the server down.
///
/// `shutdown` is the one route that is never public:
/// - with a token configured, the caller must present a valid Bearer token
///   (the middleware enforces this too; the handler re-checks so the
///   invariant holds even if the route stops being wrapped by middleware);
/// - without a configured token (open mode), only loopback peers may shut
///   the server down, so a remote caller cannot kill the process.
pub fn shutdown_request_authorized(
    state: &ServerState,
    headers: &HeaderMap,
    peer_addr: Option<SocketAddr>,
) -> bool {
    if let Some(expected_token) = &state.auth_token {
        return extract_bearer_token(headers)
            .is_some_and(|supplied_token| token_matches(supplied_token, expected_token));
    }
    peer_addr.is_some_and(|addr| addr.ip().is_loopback())
}
