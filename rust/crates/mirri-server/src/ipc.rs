//! Unix-socket IPC transport: line-delimited JSON-RPC 2.0.
//!
//! Clients write one JSON-RPC 2.0 request per line and read one response per
//! line:
//!
//! ```text
//! -> {"jsonrpc":"2.0","method":"session.create","params":{"title":"t"},"id":1}
//! <- {"jsonrpc":"2.0","result":{...},"id":1}
//! ```
//!
//! Requests without an `id` are JSON-RPC notifications and never receive a
//! response. Event streaming is served by the WebSocket transport
//! (`/api/v1/ws`): because event buses are per-session and an IPC connection
//! carries no session context, there is no bus to push from here.

use std::path::PathBuf;

use crate::error::ServerError;
use crate::state::ServerState;

#[cfg(unix)]
use mirri_transport::{JsonRpcError, JsonRpcFrame, JsonRpcResponse, protocol_error_frame};
#[cfg(unix)]
use serde_json::json;
#[cfg(unix)]
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
#[cfg(unix)]
use tokio::net::UnixListener;

/// Serve the IPC endpoint on a Unix socket, accepting connections until
/// a graceful shutdown is requested or the process exits.
#[cfg(unix)]
pub async fn serve(path: PathBuf, state: ServerState) -> Result<(), ServerError> {
    // Remove a stale socket file left by an unclean shutdown.
    if path.exists() {
        std::fs::remove_file(&path)?;
        tracing::warn!(path = %path.display(), "removed stale IPC socket");
    }
    let listener = UnixListener::bind(&path)?;
    tracing::info!(path = %path.display(), "IPC server listening");

    let mut shutdown_rx = state.shutdown_rx();
    loop {
        tokio::select! {
            // Accept new connections.
            accept_result = listener.accept() => {
                match accept_result {
                    Ok((stream, _peer_addr)) => {
                        let state = state.clone();
                        tokio::spawn(async move {
                            if let Err(error) = handle_connection(stream, state).await {
                                tracing::warn!(%error, "IPC connection handler failed");
                            }
                        });
                    }
                    Err(error) => {
                        tracing::error!(%error, "IPC accept failed");
                        // Brief backoff on persistent errors (e.g. EMFILE).
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                }
            }
            // Graceful shutdown signal.
            _ = shutdown_rx.changed() => {
                tracing::info!("IPC server shutting down");
                break;
            }
        }
    }

    // Clean up the socket file on exit.
    let _ = std::fs::remove_file(&path);
    tracing::info!("IPC server stopped");
    Ok(())
}

/// Named pipes on Windows are not implemented yet; refuse the socket path
/// explicitly instead of silently ignoring it.
#[cfg(not(unix))]
pub async fn serve(path: PathBuf, _state: ServerState) -> Result<(), ServerError> {
    Err(ServerError::BindFailed(format!(
        "IPC is not supported on this platform (socket path: {})",
        path.display()
    )))
}

/// Serve one IPC connection: answer every line-delimited JSON-RPC request
/// until the peer disconnects.
#[cfg(unix)]
async fn handle_connection(
    stream: tokio::net::UnixStream,
    state: ServerState,
) -> Result<(), ServerError> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    while let Some(line) = lines.next_line().await? {
        if let Some(response) = handle_request(&state, &line).await {
            writer.write_all(response.as_bytes()).await?;
            writer.write_all(b"\n").await?;
            writer.flush().await?;
        }
    }

    Ok(())
}

/// Handle one line-delimited JSON-RPC request. Returns the response line to
/// write, or `None` for notifications (requests without an `id`).
#[cfg(unix)]
async fn handle_request(state: &ServerState, line: &str) -> Option<String> {
    let request = match JsonRpcFrame::parse(line) {
        Ok(JsonRpcFrame::Request(request)) => request,
        // Notifications (no id) are never answered.
        Ok(JsonRpcFrame::Notification(_)) => return None,
        // The IPC server issues no reverse RPC, so a client response frame
        // is protocol noise.
        Ok(JsonRpcFrame::Response(_)) => {
            tracing::debug!("unexpected response frame over IPC; dropping");
            return None;
        }
        Err(error) => {
            // Parse errors are always answered (id null); structurally
            // invalid frames are answered only when they carried an id.
            if error.respond {
                return Some(protocol_error_frame(error.code, error.message, error.id).to_string());
            }
            tracing::debug!(message = %error.message, "dropping invalid id-less IPC frame");
            return None;
        }
    };

    // JSON-RPC notifications never receive a response.
    let id = request.id?;

    let result = state.dispatch(&request.method, request.params).await;

    match result {
        Ok(data) => Some(JsonRpcFrame::Response(JsonRpcResponse::ok(id, data)).to_string()),
        Err(error) => {
            // RPC errors are returned in the JSON-RPC error object; log them
            // server-side so failures are observable.
            tracing::error!(?id, method = %request.method, error = %error, "rpc error");
            let mut rpc_error =
                JsonRpcError::new(i64::from(error.json_rpc_code()), error.to_string());
            if let Some(error_id) = error.error_id() {
                rpc_error.data = Some(json!({ "error_id": error_id }));
            } else if let Some(domain_code) = error.domain_code() {
                // The D5 domain-code payload (`{"domainCode": "..."}`) — the
                // same shape the WS transport attaches, and the HTTP
                // envelope exposes as its top-level `domainCode` field.
                rpc_error.data = Some(mirri_proto::rpc::domain_error_data(domain_code));
            }
            Some(JsonRpcFrame::Response(JsonRpcResponse::err(id, rpc_error)).to_string())
        }
    }
}
