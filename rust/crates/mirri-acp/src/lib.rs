//! mirri-acp: Agent Client Protocol (ACP) host — protocol handler and stdio transport.
//!
//! The [`AcpHandler`] implements the ACP JSON-RPC methods and the
//! [`StdioTransport`] carries newline-delimited JSON-RPC 2.0 frames over
//! stdin/stdout. [`run`] glues them together (logging, engine selection, the
//! serve loop) — the entry point behind both the `mirri acp` subcommand and
//! the standalone `mirri-acp` compatibility binary.

pub mod acp_handler;
pub mod desktop_probe;
pub mod mcp_servers;
pub mod prompt_content;
pub mod question;
pub mod run;
pub mod stdio_transport;

pub use acp_handler::{AcpHandler, EngineFactory, PendingClientRequest};
pub use run::run;
pub use stdio_transport::StdioTransport;
