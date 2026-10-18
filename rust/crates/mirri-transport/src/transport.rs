//! Transport trait — abstracts the byte-level frame transport.
//!
//! Implementations:
//! - `WsTransport` — WebSocket (in `mirri-server`)
//! - `StdioTransport` — stdin/stdout (in `mirri-acp`)
//! - `NapiTransport` — FFI (in `mirri-node`)

use super::frame::JsonRpcFrame;
use async_trait::async_trait;

/// A bidirectional transport for JSON-RPC 2.0 frames.
///
/// Each implementation maps this to a different byte transport
/// (WebSocket, stdio, FFI), but all carry the same frame format.
#[async_trait]
pub trait Transport: Send + Sync {
    /// Send a frame to the peer.
    async fn send(&self, frame: JsonRpcFrame) -> Result<(), TransportError>;

    /// Receive the next frame from the peer.
    async fn recv(&self) -> Result<JsonRpcFrame, TransportError>;

    /// Close the transport cleanly.
    async fn close(&self) -> Result<(), TransportError>;

    /// Whether the transport is currently connected.
    fn is_connected(&self) -> bool;
}

/// Transport-level errors.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("connection closed")]
    Closed,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame parse error: {0}")]
    Parse(String),
    #[error("send error: {0}")]
    Send(String),
    #[error("not connected")]
    NotConnected,
}
