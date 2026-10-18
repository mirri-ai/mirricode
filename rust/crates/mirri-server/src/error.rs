//! Error types for the server transport layer.

use crate::rpc::RpcError;

/// Errors produced by the transport servers (HTTP, WebSocket, IPC).
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    /// Binding the listener or serving on it failed.
    #[error("bind failed: {0}")]
    BindFailed(String),
    /// An underlying I/O operation failed.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// An RPC dispatch failed.
    #[error("rpc error: {0}")]
    Rpc(#[from] RpcError),
    /// A JSON serialization or deserialization failure.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}
