//! # mirri-mcp — MCP (Model Context Protocol) client for Rust.
//!
//! This crate provides a minimal, dependency-light MCP client that can
//! connect to MCP servers over three transports:
//!
//! - **stdio** — spawn a child process and communicate over stdin/stdout
//! - **HTTP** (Streamable HTTP) — POST JSON-RPC requests to an HTTP endpoint
//! - **SSE** (deprecated) — Server-Sent Events for older servers
//!
//! The client implements the JSON-RPC 2.0 protocol directly, without
//! depending on the `rmcp` SDK (which requires a newer Rust toolchain).
//!
//! ## Quick start
//!
//! ```no_run
//! use mirri_mcp::{McpClient, McpTransportConfig};
//! use std::time::Duration;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let config = McpTransportConfig::Stdio {
//!     command: "npx".to_string(),
//!     args: vec!["-y".to_string(), "@modelcontextprotocol/server-filesystem".to_string()],
//!     env: None,
//!     cwd: None,
//! };
//!
//! let client = McpClient::connect(&config, Duration::from_secs(30)).await?;
//! let tools = client.list_tools().await?;
//! for tool in &tools {
//!     println!("  {} — {}", tool.name, tool.description);
//! }
//! client.close().await?;
//! # Ok(())
//! # }
//! ```

pub mod client;
pub mod config;
pub mod connection;
pub mod content;
pub mod error;
pub mod naming;
pub mod oauth;
pub mod reconcile;
pub mod transport;
pub mod types;

// Re-export the primary public API.
pub use client::McpClient;
pub use config::{
    McpAuthConfig, McpConfigFile, McpServerConfig, expand_server_config_values_with_env,
    load_mcp_config,
};
pub use connection::{
    McpConnection, McpConnectionManager, McpConnectionState, ReconnectBackoff, StatusListener,
};
pub use content::{ConvertedContent, convert_content_block, convert_content_blocks};
pub use error::{McpError, McpResult};
pub use naming::qualify_tool_name;
pub use oauth::{
    AuthorizationCodeFlow, OAuthClient, OAuthConfig, OAuthError, OAuthResult, PendingAuthorization,
    ServerTokenProvider, TokenSet, TokenStore,
};
pub use reconcile::{PoolOp, diff_pool_ops};
pub use transport::{McpTransportConfig, TokenProvider};
pub use types::{
    McpCallToolResult, McpContentBlock, McpEmbeddedResourceContents, McpServerInfo,
    McpToolDefinition, McpToolListResult,
};
