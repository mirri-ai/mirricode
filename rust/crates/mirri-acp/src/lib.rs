//! mirri-acp: Agent Client Protocol (ACP) host — protocol handler and stdio transport.
//!
//! The [`AcpHandler`] implements the ACP JSON-RPC methods and the
//! [`StdioTransport`] carries newline-delimited JSON-RPC 2.0 frames over
//! stdin/stdout. The `mirri-acp` binary (see `main.rs`) glues them together.

pub mod acp_handler;
pub mod mcp_servers;
pub mod prompt_content;
pub mod question;
pub mod stdio_transport;

pub use acp_handler::{AcpHandler, EngineFactory, PendingClientRequest};
pub use stdio_transport::StdioTransport;
