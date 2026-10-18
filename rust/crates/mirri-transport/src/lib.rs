//! Transport abstraction for the Mirri Code engine.
//!
//! This crate provides the [`Transport`] trait that abstracts the wire-level
//! frame transport (WebSocket, stdio, FFI), and the [`EngineAdapter`] trait
//! that provides a unified engine interface regardless of transport.
//!
//! Three implementations are planned:
//! - [`WsTransport`] — WebSocket (for Web frontend via `mirri-server`)
//! - [`StdioTransport`] — stdin/stdout (for ACP host binary)
//! - [`NapiTransport`] — FFI (for TUI NAPI binding)
//!
//! All three carry the same JSON-RPC 2.0 frames; only the byte transport differs.

pub mod adapter;
pub mod frame;
pub mod transport;

pub use adapter::{EngineAdapter, ReverseRpcHandler};
pub use frame::{
    FrameError, FrameParseError, INTERNAL_ERROR, INVALID_PARAMS, INVALID_REQUEST, JsonRpcError,
    JsonRpcFrame, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse, METHOD_NOT_FOUND,
    PARSE_ERROR, RequestId, protocol_error_frame,
};
pub use transport::Transport;
