//! `mirri-server` — the Phase 4 transport layer.
//!
//! Exposes the [`mirri_core`] engine to clients over three transports, all
//! sharing one RPC method table ([`rpc`]) and one [`ServerState`]:
//!
//! - [`http`] — axum HTTP server with REST-style routes under `/api/v1/`
//!   (session lifecycle, messages, approvals, questions, tools, workspaces,
//!   config, health, meta, shutdown).
//! - [`ws`] — multiplexed WebSocket endpoint at `/api/v1/ws` for
//!   real-time [`SessionEvent`](mirri_core::SessionEvent) streaming (one
//!   connection subscribes to many sessions).
//! - [`ws_rpc`] — unified JSON-RPC 2.0 WebSocket endpoint at `/api/v1/rpc`
//!   (design 01-protocol-unification.md): forward RPC, event stream with
//!   `{seq, epoch}` cursor replay, reverse RPC plumbing, and bearer
//!   subprotocol authentication. The legacy [`ws`] endpoint stays during the
//!   migration.
//! - [`ipc`] — line-delimited JSON-RPC 2.0 over a Unix socket.
//!
//! HTTP responses use the `{code, msg, data, request_id}` envelope, matching
//! the TypeScript server (`code: 0` means success; non-zero codes follow
//! JSON-RPC 2.0 conventions). The [`auth`] module guards the HTTP API with a
//! Bearer token when one is configured (the `healthz`/`meta` probes stay
//! public; `POST /api/v1/shutdown` additionally requires a loopback peer in
//! open mode). [`server_token`] persists that token in
//! `<mirri-home>/server.token` (shared format with the TypeScript
//! kap-server) so `mirri serve` boots authenticated by default.

pub mod auth;
pub mod error;
pub mod files;
pub mod http;
pub mod ipc;
pub mod rpc;
pub mod server_token;
pub mod state;
pub mod web_embed;
pub mod ws;
pub mod ws_rpc;

use std::time::Duration;

pub use error::ServerError;
pub use http::{router as http_router, serve as serve_http};
pub use ipc::serve as serve_ipc;
pub use rpc::{ApiErrorResponse, ApiResponse, RpcError, RpcHandler, RpcRegistry, RpcResult};
pub use state::ServerState;

/// Serve the full transport stack: HTTP (and its WebSocket endpoint) on
/// the already-bound `http_listener`, plus the Unix-socket IPC server
/// when `ipc_path` is given.
///
/// The listener is bound by the caller so that anything printed before
/// this call (e.g. the CLI startup banner) or spawned against the URL
/// (e.g. the `mirri web` browser opener) targets an address that is
/// already reachable.
///
/// The HTTP server runs until a graceful shutdown is requested through
/// [`ServerState::request_shutdown`] (e.g. via `POST /api/v1/shutdown`); the
/// IPC server runs concurrently as a spawned task.
pub async fn serve(
    http_listener: tokio::net::TcpListener,
    ipc_path: Option<std::path::PathBuf>,
    state: ServerState,
    web_dir: Option<std::path::PathBuf>,
) -> Result<(), ServerError> {
    // Start the IPC server if a socket path was provided.
    if let Some(path) = ipc_path {
        let ipc_state = state.clone();
        tokio::spawn(async move {
            if let Err(error) = ipc::serve(path, ipc_state).await {
                tracing::error!(%error, "IPC server error");
            }
        });
    }

    // Serve HTTP until a graceful shutdown is requested.
    let shutdown_rx = state.shutdown_tx.subscribe();
    // `into_make_service_with_connect_info` exposes the client's socket
    // address as a `ConnectInfo` extension, which the shutdown route needs
    // for its loopback guard.
    axum::serve(
        http_listener,
        http::router(state.clone(), web_dir).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        let mut shutdown_rx = shutdown_rx;
        // `changed()` resolves when a shutdown is requested or when the
        // sender is dropped — both mean "stop serving".
        let _ = shutdown_rx.changed().await;
        tracing::info!("HTTP server shutting down");
    })
    .await
    .map_err(|e| ServerError::BindFailed(e.to_string()))?;

    // Drain in-flight engine calls before returning: the caller (the CLI's
    // main) drops the runtime as soon as `serve` returns, which aborts any
    // still-running engine-call task mid-flight — every in-flight RPC
    // would fail with a dropped oneshot instead of completing. The drain
    // is bounded (a wedged engine call cannot hang the shutdown forever),
    // and connections stay alive throughout so responses reach clients.
    state.drain_engine_calls(Duration::from_secs(10)).await;

    Ok(())
}
