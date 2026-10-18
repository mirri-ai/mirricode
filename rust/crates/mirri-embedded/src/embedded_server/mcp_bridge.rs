//! MCP bridge tool — adapts an MCP server tool to the engine's [`Tool`] trait.
//!
//! Each connected MCP server exposes a set of tools via `list_tools`. This
//! module wraps each discovered tool in an [`McpBridgeTool`] struct so the
//! engine can register it in the [`ToolManager`] and the LLM can call it
//! like any builtin tool.
//!
//! Execution is forwarded to the [`McpConnectionManager::call_tool`] method,
//! which routes the call to the correct MCP server over its transport
//! (stdio/SSE). The result is mapped to [`ToolResult`] and returned to the
//! engine. A connection-class failure (server killed, transport reset)
//! additionally marks the connection lost and schedules the automatic
//! reconnection (plans/07 T8) — the failing call still fails, the next one
//! finds the recovered connection.
//!
//! [`McpAuthBridgeTool`] (same module) is the OAuth counterpart: the
//! synthetic `mcp__<server>__authenticate` tool a remote server in the
//! `needs-auth` state exposes so the LLM can drive the authorization flow
//! (the TS `agent/mcp/tools/auth.ts` design, non-blocking).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use mirri_core::error::EngineResult;
use mirri_core::tool::{
    ExecutionPlan, Tool, ToolExecution, ToolInputDisplay, ToolOutput, ToolResult,
};
use mirri_mcp::McpConnectionManager;

use mirri_mcp::oauth::{AuthorizationCodeFlow, OAuthConfig, TokenStore};

/// A single MCP tool exposed to the LLM as a callable [`Tool`].
///
/// Created during session setup from `McpConnectionManager::list_all_tools()`
/// and registered via `ToolManager::register_mcp_tool`. The qualified name
/// (`mcp__<server>__<tool>`) is used as the tool name so it matches the
/// permission-rule glob pattern `mcp__*`.
pub struct McpBridgeTool {
    name: String,
    description: String,
    parameters: serde_json::Value,
    server_name: String,
    /// The original tool name on the MCP server (without the `mcp__server__`
    /// prefix). Used when forwarding the call to the manager.
    local_name: String,
    manager: Arc<McpConnectionManager>,
}

impl McpBridgeTool {
    /// Create a new bridge tool.
    ///
    /// - `qualified_name`: the engine-visible name (`mcp__server__tool`).
    /// - `local_name`: the original tool name on the server.
    /// - `server_name`: the MCP server identifier.
    /// - `definition`: the tool definition from `list_tools`.
    /// - `manager`: the shared connection manager for routing calls.
    pub fn new(
        qualified_name: impl Into<String>,
        local_name: impl Into<String>,
        server_name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
        manager: Arc<McpConnectionManager>,
    ) -> Self {
        Self {
            name: qualified_name.into(),
            description: description.into(),
            parameters,
            server_name: server_name.into(),
            local_name: local_name.into(),
            manager,
        }
    }
}

impl Tool for McpBridgeTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> &serde_json::Value {
        &self.parameters
    }

    fn resolve_execution(&self, input: serde_json::Value) -> EngineResult<ToolExecution> {
        let manager = Arc::clone(&self.manager);
        let server = self.server_name.clone();
        let tool = self.local_name.clone();
        let display_name = self.name.clone();

        let plan = ExecutionPlan {
            accesses: vec![],
            display: ToolInputDisplay::Generic {
                summary: format!("MCP tool: {display_name}"),
            },
            approval_rule: display_name.clone(),
            stop_batch_after_this: false,
        };

        Ok(ToolExecution::Runnable {
            plan,
            execute: Box::new(move |_ctx| {
                Box::pin(async move {
                    match manager.call_tool(&server, &tool, input).await {
                        Ok(result) => {
                            // Render content blocks to text. MCP tools may
                            // return text, image, or audio blocks; for the
                            // engine we concatenate all text blocks into a
                            // single string. Non-text blocks are noted.
                            let mut parts: Vec<String> = Vec::new();
                            for block in &result.content {
                                match block {
                                    mirri_mcp::types::McpContentBlock::Text { text } => {
                                        parts.push(text.clone());
                                    }
                                    mirri_mcp::types::McpContentBlock::Image {
                                        mime_type, ..
                                    } => {
                                        parts.push(format!("[image: {mime_type}]"));
                                    }
                                    mirri_mcp::types::McpContentBlock::Audio {
                                        mime_type, ..
                                    } => {
                                        parts.push(format!("[audio: {mime_type}]"));
                                    }
                                    mirri_mcp::types::McpContentBlock::Resource { resource } => {
                                        if let Some(text) = &resource.text {
                                            parts.push(text.clone());
                                        } else if let Some(blob) = &resource.blob {
                                            parts.push(format!(
                                                "[resource: {} ({} bytes)]",
                                                resource.uri,
                                                blob.len()
                                            ));
                                        } else {
                                            parts.push(format!("[resource: {}]", resource.uri));
                                        }
                                    }
                                }
                            }
                            let output = if parts.is_empty() {
                                String::new()
                            } else {
                                parts.join("\n")
                            };

                            ToolResult {
                                output: ToolOutput::Text(output),
                                is_error: result.is_error,
                                note: None,
                                stop_turn: false,
                                truncated: false,
                                injection: None,
                            }
                        }
                        Err(e) => {
                            // A connection-class failure means the server was
                            // lost (killed, transport reset): record the loss
                            // so the stale bridge tools are dropped, and
                            // schedule the automatic reconnection with
                            // backoff — the failing call still fails, the
                            // next one finds the recovered connection
                            // (plans/07 T8; the TS reconnectForToolCall
                            // counterpart, backgrounded).
                            if is_connection_loss(&e) {
                                let _ = manager.mark_unexpectedly_disconnected(&server).await;
                                McpConnectionManager::spawn_auto_reconnect(
                                    Arc::clone(&manager),
                                    &server,
                                );
                            }
                            ToolResult {
                                output: ToolOutput::Text(format!(
                                    "MCP tool '{display_name}' error: {e}"
                                )),
                                is_error: true,
                                note: None,
                                stop_turn: false,
                                truncated: false,
                                injection: None,
                            }
                        }
                    }
                })
            }),
        })
    }
}

/// Whether an error means the server connection was lost (as opposed to a
/// tool-level failure): the classes a reconnection can plausibly fix.
/// Authorization failures are deliberately excluded — retrying cannot fix
/// them (the authenticate tool owns that path).
fn is_connection_loss(error: &mirri_mcp::McpError) -> bool {
    matches!(
        error,
        mirri_mcp::McpError::Disconnected
            | mirri_mcp::McpError::Transport(_)
            | mirri_mcp::McpError::ConnectionFailed(_)
    )
}

/// The synthetic `mcp__<server>__authenticate` tool a remote OAuth server
/// exposes in the `needs-auth` state (the TS `agent/mcp/tools/auth.ts`
/// design, mirrored):
///
/// 1. When cached tokens are already valid, the call just reconnects the
///    server (the `AlreadyAuthorized` fast path).
/// 2. Otherwise it runs the discovery half of the authorization-code flow
///    (RFC 9728 + RFC 8414 + RFC 7591 + PKCE), **returns the authorization
///    URL for the model to hand to the user verbatim**, and completes the
///    flow in the background: the local callback listener waits (bounded by
///    the OAuth module's 300s timeout), the exchanged token set is
///    persisted, and the pool reconnects — the engine's status listener
///    then swaps this synthetic tool out for the server's real tools.
///
/// Unlike the TS counterpart (which blocks the tool call for up to 15
/// minutes), the rust variant returns as soon as the URL is known: the
/// engine's turn model should not be held hostage by a human's browser
/// round-trip.
pub struct McpAuthBridgeTool {
    name: String,
    server_name: String,
    server_url: String,
    oauth_config: OAuthConfig,
    token_dir: PathBuf,
    manager: Arc<McpConnectionManager>,
}

impl McpAuthBridgeTool {
    /// Create the authenticate tool for one `needs-auth` server.
    ///
    /// - `qualified_name`: `mcp__<server>__authenticate`.
    /// - `server_url`: the remote endpoint (HTTP/SSE transport URL).
    /// - `client_id` / `scopes`: the config's OAuth settings (a `None`
    ///   client id triggers dynamic client registration).
    pub fn new(
        qualified_name: impl Into<String>,
        server_name: impl Into<String>,
        server_url: impl Into<String>,
        client_id: Option<String>,
        scopes: Vec<String>,
        token_dir: PathBuf,
        manager: Arc<McpConnectionManager>,
    ) -> Self {
        Self {
            name: qualified_name.into(),
            server_name: server_name.into(),
            server_url: server_url.into(),
            oauth_config: OAuthConfig {
                client_id,
                scopes,
                redirect_port: None,
            },
            token_dir,
            manager,
        }
    }
}

const AUTH_TOOL_DESCRIPTION: &str = "Authenticate with an OAuth-protected MCP server.

This server requires an OAuth login that has not yet been completed. Calling this tool returns an authorization URL.

  1. **You must show that URL to the user verbatim** and ask them to open it
     in a browser, sign in, and approve the mirri-code client.
  2. The approval completes in the background (no further action needed):
     once the browser redirects back, the server reconnects and its real
     tools replace this one.

Takes no arguments. Treat the URL as sensitive — do not modify it or strip query parameters.";

impl Tool for McpAuthBridgeTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        AUTH_TOOL_DESCRIPTION
    }

    fn parameters(&self) -> &serde_json::Value {
        // A constant empty-object schema.
        static EMPTY: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        EMPTY.get_or_init(|| serde_json::json!({"type": "object", "properties": {}}))
    }

    fn resolve_execution(&self, _input: serde_json::Value) -> EngineResult<ToolExecution> {
        let server_name = self.server_name.clone();
        let server_url = self.server_url.clone();
        let oauth_config = self.oauth_config.clone();
        let token_dir = self.token_dir.clone();
        let manager = Arc::clone(&self.manager);
        let display_name = self.name.clone();

        let plan = ExecutionPlan {
            accesses: vec![],
            display: ToolInputDisplay::Generic {
                summary: format!("MCP OAuth authentication: {display_name}"),
            },
            approval_rule: display_name.clone(),
            stop_batch_after_this: false,
        };

        Ok(ToolExecution::Runnable {
            plan,
            execute: Box::new(move |_ctx| {
                Box::pin(async move {
                    // Already-authorized fast path: valid cached tokens mean
                    // only the reconnection is missing.
                    let store = TokenStore::new(token_dir.clone());
                    if let Some(cached) = store.load(&server_name)
                        && !cached.is_expired(SystemTime::now())
                    {
                        return match manager.reconnect(&server_name).await {
                            Ok(()) => ToolResult {
                                output: ToolOutput::Text(format!(
                                    "MCP server \"{server_name}\" already had valid OAuth credentials. Reconnected; real tools are available now."
                                )),
                                is_error: false,
                                note: None,
                                stop_turn: false,
                                truncated: false,
                                injection: None,
                            },
                            Err(error) => auth_error_result(&server_name, &error),
                        };
                    }

                    // Run the whole flow on a background task and hand the
                    // authorization URL back through a oneshot: the flow's
                    // borrow of the HTTP client must stay inside the task
                    // (the pending authorization is not `'static`).
                    let (url_tx, url_rx) =
                        tokio::sync::oneshot::channel::<Result<String, String>>();
                    let bg_server_url = server_url.clone();
                    let bg_oauth_config = oauth_config.clone();
                    let bg_server = server_name.clone();
                    let bg_token_dir = token_dir.clone();
                    let bg_manager = Arc::clone(&manager);
                    tokio::spawn(async move {
                        let http = reqwest::Client::new();
                        let flow =
                            AuthorizationCodeFlow::new(&http, &bg_server_url, &bg_oauth_config);
                        let pending = match flow.begin().await {
                            Ok(pending) => pending,
                            Err(error) => {
                                let _ = url_tx.send(Err(error.to_string()));
                                return;
                            }
                        };
                        let _ = url_tx.send(Ok(pending.authorization_url().to_string()));
                        // Complete in the same task (the callback listener,
                        // the state check, and the token exchange), persist,
                        // and reconnect — the engine's status listener swaps
                        // this tool out for the server's real tools.
                        match pending.complete().await {
                            Ok(tokens) => {
                                if let Err(error) =
                                    TokenStore::new(bg_token_dir).save(&bg_server, &tokens)
                                {
                                    tracing::warn!(
                                        server = %bg_server, %error,
                                        "persisting the MCP OAuth token set failed"
                                    );
                                }
                                if let Err(error) = bg_manager.reconnect(&bg_server).await {
                                    tracing::warn!(
                                        server = %bg_server, %error,
                                        "reconnecting the authorized MCP server failed"
                                    );
                                }
                            }
                            Err(error) => {
                                tracing::warn!(
                                    server = %bg_server, %error,
                                    "the MCP OAuth authorization flow did not complete"
                                );
                            }
                        }
                    });

                    let authorization_url = match url_rx.await {
                        Ok(Ok(url)) => url,
                        Ok(Err(error)) => {
                            return ToolResult {
                                output: ToolOutput::Text(format!(
                                    "OAuth authorization for MCP server \"{server_name}\" failed during discovery: {error}"
                                )),
                                is_error: true,
                                note: None,
                                stop_turn: false,
                                truncated: false,
                                injection: None,
                            };
                        }
                        Err(_) => {
                            return auth_error_result(
                                &server_name,
                                &"the flow task ended unexpectedly",
                            );
                        }
                    };

                    ToolResult {
                        output: ToolOutput::Text(format!(
                            "Open this URL in a browser to authorize MCP server \"{server_name}\":\n\n{authorization_url}\n\nOnce the user approves, the server reconnects automatically and its real tools become available."
                        )),
                        is_error: false,
                        note: None,
                        stop_turn: false,
                        truncated: false,
                        injection: None,
                    }
                })
            }),
        })
    }
}

/// The error-shaped result of a failed authenticate-tool invocation.
fn auth_error_result(server_name: &str, error: &dyn std::fmt::Display) -> ToolResult {
    ToolResult {
        output: ToolOutput::Text(format!(
            "OAuth authorization for MCP server \"{server_name}\" failed: {error}"
        )),
        is_error: true,
        note: None,
        stop_turn: false,
        truncated: false,
        injection: None,
    }
}
