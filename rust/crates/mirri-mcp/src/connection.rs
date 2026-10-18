//! MCP connection manager.
//!
//! The connection manager owns the lifecycle of all MCP server connections
//! in a workspace. It is designed to be shared across async tasks (it is
//! `Send + Sync`) and provides:
//!
//! - **Parallel connect** — all servers connect concurrently, with per-server
//!   failure isolation.
//! - **Tool discovery** — after connecting, the server's tools are cached
//!   and can be listed without additional round-trips.
//! - **Tool invocation** — `call_tool` routes to the correct server.
//! - **Status listeners** — register callbacks that fire on state changes.
//! - **Reconnection** — tear down and re-establish a single server's
//!   connection.
//!
//! ## Stale-connection prevention
//!
//! Each connection attempt is assigned a monotonically increasing `attempt_id`.
//! When an async connect completes, it checks whether its `attempt_id` still
//! matches the current one on the connection handle. If a newer attempt has
//! started (e.g. a `reconnect` was called while the first was still in
//! flight), the stale result is discarded.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Mutex, RwLock};
use tracing::{debug, error, info, instrument, warn};

use crate::client::McpClient;
use crate::config::{McpAuthConfig, McpServerConfig};
use crate::error::{McpError, McpResult};
use crate::oauth::{ServerTokenProvider, TokenStore};
use crate::types::{McpCallToolResult, McpServerInfo, McpToolDefinition};

// ---------------------------------------------------------------------------
// Connection state
// ---------------------------------------------------------------------------

/// The lifecycle state of a single MCP server connection.
#[derive(Debug, Clone)]
pub enum McpConnectionState {
    /// Not yet connected.
    Pending,
    /// Successfully connected; server info and discovered tools are cached.
    Connected {
        /// Identity of the remote server.
        server_info: McpServerInfo,
        /// Tools advertised by the server.
        tools: Vec<McpToolDefinition>,
    },
    /// The connection attempt failed.
    Failed {
        /// Human-readable error message.
        error: String,
    },
    /// The server is disabled in configuration.
    Disabled,
    /// The server requires authentication.
    NeedsAuth,
}

/// Manual `PartialEq` implementation — the contained types (`McpServerInfo`,
/// `McpToolDefinition`) do not derive `PartialEq` because they come from
/// Task 24's types module, which we cannot modify. We compare by server
/// name + tool count for `Connected`, and by error message for `Failed`.
impl PartialEq for McpConnectionState {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Pending, Self::Pending) => true,
            (Self::Disabled, Self::Disabled) => true,
            (Self::NeedsAuth, Self::NeedsAuth) => true,
            (
                Self::Connected {
                    server_info: a_info,
                    tools: a_tools,
                },
                Self::Connected {
                    server_info: b_info,
                    tools: b_tools,
                },
            ) => {
                a_info.name == b_info.name
                    && a_info.version == b_info.version
                    && a_tools.len() == b_tools.len()
                    && a_tools
                        .iter()
                        .zip(b_tools.iter())
                        .all(|(a, b)| a.name == b.name && a.description == b.description)
            }
            (Self::Failed { error: a_err }, Self::Failed { error: b_err }) => a_err == b_err,
            _ => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

/// A single MCP server connection handle.
///
/// Holds the config, current state, and the underlying client (if connected).
/// The `attempt_id` field prevents stale async results from overwriting
/// a newer connection state.
pub struct McpConnection {
    /// Server name (key in the config map).
    pub server_name: String,
    /// Server configuration.
    pub config: McpServerConfig,
    /// Current lifecycle state.
    pub state: McpConnectionState,
    /// The MCP client, present when connected.
    client: Option<Arc<McpClient>>,
    /// Monotonic attempt counter — prevents stale connect results.
    attempt_id: u64,
    /// Tool calls currently executing against this connection's client.
    /// The reconcile path waits for this to drain before tearing a
    /// connection down (spec §4.3 / R2).
    inflight_calls: usize,
}

impl McpConnection {
    /// Create a new connection in `Pending` state.
    fn new(server_name: String, config: McpServerConfig) -> Self {
        let state = if config.is_enabled() {
            McpConnectionState::Pending
        } else {
            McpConnectionState::Disabled
        };
        McpConnection {
            server_name,
            config,
            state,
            client: None,
            attempt_id: 0,
            inflight_calls: 0,
        }
    }

    /// Replace the client and state, but only if the `attempt_id` matches.
    /// Returns `true` if the update was applied, `false` if it was stale.
    fn try_apply_result(
        &mut self,
        attempt_id: u64,
        client: Option<Arc<McpClient>>,
        state: McpConnectionState,
    ) -> bool {
        if attempt_id != self.attempt_id {
            debug!(
                server = %self.server_name,
                "discarding stale connection result (attempt {} != current {})",
                attempt_id,
                self.attempt_id
            );
            return false;
        }
        self.client = client;
        self.state = state;
        true
    }
}

impl std::fmt::Debug for McpConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpConnection")
            .field("server_name", &self.server_name)
            .field("state", &self.state)
            .field("attempt_id", &self.attempt_id)
            .field("has_client", &self.client.is_some())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Connection manager
// ---------------------------------------------------------------------------

/// Type alias for a status-change listener callback.
pub type StatusListener = Box<dyn Fn(String, McpConnectionState) + Send + Sync>;

/// MCP connection manager — workspace-level shared state.
///
/// Owns all MCP server connections and coordinates their lifecycle. Designed
/// to be wrapped in `Arc` and shared across the agent runtime.
pub struct McpConnectionManager {
    /// Server name → connection handle (behind a mutex for state mutations).
    connections: RwLock<HashMap<String, Arc<Mutex<McpConnection>>>>,
    /// Registered status-change listeners.
    ///
    /// A plain std lock (not tokio's): registration and notification are
    /// both synchronous, and `add_status_listener` must never silently
    /// drop a listener the way a `try_write` does — the hot-reconcile path
    /// attaches listeners while connections may already be notifying.
    status_listeners: std::sync::RwLock<Vec<StatusListener>>,
    /// The OAuth token store directory, when OAuth is wired (the TS
    /// parity stance: a token provider is attached to a remote transport
    /// only when a cached token set exists under this directory).
    oauth_token_dir: Option<PathBuf>,
    /// Server name → generation of the running auto-reconnect loop (the
    /// single-flight guard: a newer trigger supersedes the older loop).
    auto_reconnect_gens: std::sync::Mutex<HashMap<String, u32>>,
}

impl McpConnectionManager {
    /// Create an empty connection manager.
    pub fn new() -> Self {
        McpConnectionManager {
            connections: RwLock::new(HashMap::new()),
            status_listeners: std::sync::RwLock::new(Vec::new()),
            oauth_token_dir: None,
            auto_reconnect_gens: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Create an empty connection manager whose remote-transports may use
    /// OAuth tokens persisted under `oauth_token_dir` (one
    /// `<server-name>.json` per server, the [`crate::oauth::TokenStore`]
    /// layout — the persistence calls stay in this crate, never in
    /// mirri-core).
    pub fn with_oauth_token_dir(oauth_token_dir: PathBuf) -> Self {
        McpConnectionManager {
            oauth_token_dir: Some(oauth_token_dir),
            ..Self::new()
        }
    }

    /// The OAuth token store directory this manager was wired with.
    pub fn oauth_token_dir(&self) -> Option<&Path> {
        self.oauth_token_dir.as_deref()
    }

    /// Connect to all configured MCP servers in parallel.
    ///
    /// Each server is connected independently; a failure in one server does
    /// not affect the others. The returned map contains the result for each
    /// server name.
    ///
    /// Disabled servers are skipped (their state is set to `Disabled`).
    #[instrument(skip(self, configs), fields(server_count = configs.len()))]
    pub async fn connect_all(
        &self,
        configs: HashMap<String, McpServerConfig>,
    ) -> HashMap<String, McpResult<()>> {
        // Register all connections in Pending/Disabled state.
        {
            let mut connections = self.connections.write().await;
            for (name, config) in &configs {
                let conn = McpConnection::new(name.clone(), config.clone());
                connections.insert(name.clone(), Arc::new(Mutex::new(conn)));
            }
        }

        // Fire initial status events.
        {
            let connections = self.connections.read().await;
            for (name, conn) in connections.iter() {
                let state = conn.lock().await.state.clone();
                self.notify_listeners(name.clone(), state);
            }
        }

        // Connect all servers in parallel.
        let oauth_token_dir = self.oauth_token_dir.clone();
        let mut tasks = Vec::new();
        {
            let connections = self.connections.read().await;
            for (name, conn_arc) in connections.iter() {
                let name = name.clone();
                let conn_arc = Arc::clone(conn_arc);
                let oauth_token_dir = oauth_token_dir.clone();
                tasks.push(tokio::spawn(async move {
                    let result = Self::do_connect(&conn_arc, oauth_token_dir.as_deref()).await;
                    (name, result)
                }));
            }
        }

        let mut results = HashMap::new();
        for task in tasks {
            match task.await {
                Ok((name, result)) => {
                    // Fire status event after connect.
                    let state = {
                        let connections = self.connections.read().await;
                        if let Some(conn) = connections.get(&name) {
                            conn.lock().await.state.clone()
                        } else {
                            McpConnectionState::Pending
                        }
                    };
                    self.notify_listeners(name.clone(), state);
                    results.insert(name, result);
                }
                Err(e) => {
                    // Task panicked — should not happen but handle gracefully.
                    error!("connect_all task panicked: {e}");
                }
            }
        }

        results
    }

    /// Connect to a single MCP server.
    ///
    /// If a connection with the same name already exists, it is replaced.
    #[instrument(skip(self, config), fields(server = %name))]
    pub async fn connect_one(&self, name: &str, config: McpServerConfig) -> McpResult<()> {
        let conn_arc = {
            let mut connections = self.connections.write().await;
            let conn = McpConnection::new(name.to_string(), config);
            let arc = Arc::new(Mutex::new(conn));
            connections.insert(name.to_string(), Arc::clone(&arc));
            arc
        };

        // Fire Pending status.
        {
            let state = conn_arc.lock().await.state.clone();
            self.notify_listeners(name.to_string(), state);
        }

        let result = Self::do_connect(&conn_arc, self.oauth_token_dir.as_deref()).await;

        // Fire final status.
        let state = conn_arc.lock().await.state.clone();
        self.notify_listeners(name.to_string(), state);

        result
    }

    /// Internal: perform the actual connect + discover sequence.
    ///
    /// This increments `attempt_id`, creates the transport, connects the
    /// client, lists tools, and updates the connection state.
    async fn do_connect(
        conn_arc: &Arc<Mutex<McpConnection>>,
        oauth_token_dir: Option<&Path>,
    ) -> McpResult<()> {
        let (config, attempt_id, server_name) = {
            let mut conn = conn_arc.lock().await;
            // Increment attempt_id for this attempt.
            conn.attempt_id += 1;
            let attempt_id = conn.attempt_id;

            if !conn.config.is_enabled() {
                conn.state = McpConnectionState::Disabled;
                info!(server = %conn.server_name, "MCP server disabled in config, skipping connection");
                return Ok(());
            }

            (conn.config.clone(), attempt_id, conn.server_name.clone())
        };

        // Value-level env expansion at connect time (adjudication A5): the
        // stored config keeps `${VAR}` references raw; they are resolved
        // here — exactly once per connect attempt — so startup connects,
        // watcher reloads, plugin-manifest servers and connection tests all
        // expand with the environment current at connect. Undefined
        // variables expand to the empty string; their names are collected
        // so failures can point at the missing variables.
        let (config, undefined_vars) = crate::config::expand_server_config_values(&config);
        if !undefined_vars.is_empty() {
            warn!(
                server = %server_name,
                undefined_vars = %undefined_vars.join(", "),
                "MCP config references undefined environment variables"
            );
        }

        info!(server = %server_name, "connecting to MCP server");

        let startup_timeout = config.startup_timeout();

        // OAuth wiring (adjudication A4 — remote transports only): when the
        // config declares `auth: {type: "oauth"}` and a cached token set
        // already exists, the transport gets a token provider so the bearer
        // token is attached (and refreshed) per request. Without cached
        // tokens the plain client connects and the server's real 401 lands
        // the connection in `needs-auth` (the authenticate tool's trigger).
        let token_provider: Option<Arc<dyn crate::transport::TokenProvider>> =
            if uses_remote_oauth(&config) {
                oauth_token_dir.and_then(|dir| {
                    TokenStore::new(dir.to_path_buf())
                        .load(&server_name)
                        .map(|_| {
                            Arc::new(ServerTokenProvider::new(
                                reqwest::Client::new(),
                                dir.to_path_buf(),
                                &server_name,
                            ))
                                as Arc<dyn crate::transport::TokenProvider>
                        })
                })
            } else {
                None
            };

        // Connect with timeout.
        let connect_result = tokio::time::timeout(
            startup_timeout,
            McpClient::connect_with_token_provider(
                &config.transport,
                startup_timeout,
                token_provider,
            ),
        )
        .await;

        // A5 diagnostics: when the config referenced undefined variables,
        // append their names to the failure so a user can tell an empty
        // command/url came from an unset `${VAR}` at a glance.
        let undefined_note = |error: &str| {
            if undefined_vars.is_empty() {
                error.to_string()
            } else {
                format!(
                    "{error} (undefined env vars: {})",
                    undefined_vars.join(", ")
                )
            }
        };

        let client = match connect_result {
            Ok(Ok(client)) => Arc::new(client),
            Ok(Err(e)) => {
                warn!(server = %server_name, error = %e, "MCP connection failed");
                let mut conn = conn_arc.lock().await;
                let failed = undefined_note(&e.to_string());
                conn.try_apply_result(
                    attempt_id,
                    None,
                    connect_failure_state(&config, &e, &failed),
                );
                return Err(e);
            }
            Err(_) => {
                warn!(
                    server = %server_name,
                    timeout_ms = startup_timeout.as_millis(),
                    "MCP connection timed out"
                );
                let mut conn = conn_arc.lock().await;
                conn.try_apply_result(
                    attempt_id,
                    None,
                    McpConnectionState::Failed {
                        error: undefined_note(&format!(
                            "connection timed out after {}ms",
                            startup_timeout.as_millis()
                        )),
                    },
                );
                return Err(McpError::Timeout);
            }
        };

        // Discover tools.
        let tools = match client.list_tools().await {
            Ok(tools) => tools,
            Err(e) => {
                warn!(server = %server_name, error = %e, "tool discovery failed");
                let _ = client.close().await;
                let mut conn = conn_arc.lock().await;
                let failed = format!("tool discovery failed: {e}");
                conn.try_apply_result(
                    attempt_id,
                    None,
                    connect_failure_state(&config, &e, &failed),
                );
                return Err(e);
            }
        };

        // Get server info.
        let server_info = match client.server_info().await {
            Ok(info) => info,
            Err(e) => {
                warn!(server = %server_name, error = %e, "failed to get server info");
                let _ = client.close().await;
                let mut conn = conn_arc.lock().await;
                let failed = format!("failed to get server info: {e}");
                conn.try_apply_result(
                    attempt_id,
                    None,
                    connect_failure_state(&config, &e, &failed),
                );
                return Err(e);
            }
        };

        info!(
            server = %server_name,
            server_info_name = %server_info.name,
            tool_count = tools.len(),
            "MCP server connected"
        );

        let mut conn = conn_arc.lock().await;
        conn.try_apply_result(
            attempt_id,
            Some(client),
            McpConnectionState::Connected { server_info, tools },
        );

        Ok(())
    }

    /// Disconnect from a single MCP server.
    ///
    /// Closes the underlying client and sets the state to `Pending`.
    #[instrument(skip(self), fields(server = %name))]
    pub async fn disconnect(&self, name: &str) -> McpResult<()> {
        let conn_arc = {
            let connections = self.connections.read().await;
            connections.get(name).cloned()
        };

        let conn_arc = match conn_arc {
            Some(arc) => arc,
            None => return Ok(()), // Not connected — nothing to do.
        };

        let client = {
            let mut conn = conn_arc.lock().await;
            let client = conn.client.take();
            conn.state = McpConnectionState::Pending;
            client
        };

        if let Some(client) = client
            && let Err(e) = client.close().await
        {
            warn!(server = %name, error = %e, "error closing MCP client");
        }

        let state = conn_arc.lock().await.state.clone();
        self.notify_listeners(name.to_string(), state);

        debug!(server = %name, "disconnected");
        Ok(())
    }

    /// Disconnect a single MCP server and remove its registration entirely.
    ///
    /// Unlike [`disconnect`](Self::disconnect), which leaves the connection
    /// registered in the `Pending` state, `remove` deletes the entry so the
    /// server no longer appears in
    /// [`list_connections`](Self::list_connections). Removing a server that
    /// is not registered is a no-op. Used by the config hot-reload watcher to
    /// clean up servers deleted from `mcp.json`.
    ///
    /// The takedown is atomic with respect to a concurrent
    /// [`connect_one`](Self::connect_one): the entry is claimed and deleted
    /// under a single write lock, and the connection's `attempt_id` is bumped
    /// so that any in-flight connect result is discarded as stale instead of
    /// re-applying `Connected` after the entry is gone.
    #[instrument(skip(self), fields(server = %name))]
    pub async fn remove(&self, name: &str) -> McpResult<()> {
        let client = {
            let mut connections = self.connections.write().await;
            let Some(conn_arc) = connections.get(name).cloned() else {
                return Ok(()); // Not registered — nothing to do.
            };
            let mut conn = conn_arc.lock().await;
            // Invalidate any in-flight connect so its result cannot
            // resurrect the entry after we delete it.
            conn.attempt_id += 1;
            conn.state = McpConnectionState::Pending;
            let client = conn.client.take();
            connections.remove(name);
            client
        };

        if let Some(client) = client
            && let Err(e) = client.close().await
        {
            warn!(server = %name, error = %e, "error closing MCP client");
        }

        self.notify_listeners(name.to_string(), McpConnectionState::Pending);
        debug!(server = %name, "disconnected and unregistered");
        Ok(())
    }

    /// Disconnect from all MCP servers in parallel.
    pub async fn disconnect_all(&self) {
        let names: Vec<String> = {
            let connections = self.connections.read().await;
            connections.keys().cloned().collect()
        };

        let mut tasks = Vec::new();
        for name in &names {
            let name = name.clone();
            let this = self;
            tasks.push(async move {
                let _ = this.disconnect(&name).await;
            });
        }

        futures::future::join_all(tasks).await;
    }

    /// Call a tool on a specific MCP server.
    ///
    /// Returns an error if the server is not connected or the tool call fails.
    #[instrument(skip(self, args), fields(server = %server_name, tool = %tool_name))]
    pub async fn call_tool(
        &self,
        server_name: &str,
        tool_name: &str,
        args: serde_json::Value,
    ) -> McpResult<McpCallToolResult> {
        let conn_arc = {
            let connections = self.connections.read().await;
            connections.get(server_name).cloned()
        };

        let conn_arc = match conn_arc {
            Some(arc) => arc,
            None => {
                return Err(McpError::ConnectionFailed(format!(
                    "no MCP server named '{server_name}' is registered"
                )));
            }
        };

        // One lock section gathers the client, the tool-filter check and
        // the call timeout, and marks the call as in flight so the
        // reconcile path can drain before tearing the connection down.
        let (client, tool_timeout) = {
            let mut conn = conn_arc.lock().await;
            let client = match &conn.client {
                Some(c) => Arc::clone(c),
                None => return Err(McpError::Disconnected),
            };
            if !conn.config.is_tool_allowed(tool_name) {
                return Err(McpError::ToolNotFound(format!(
                    "tool '{tool_name}' is not allowed for server '{server_name}'"
                )));
            }
            conn.inflight_calls += 1;
            (client, conn.config.tool_timeout())
        };

        // call_tool inside the client uses a fixed default timeout, so we
        // wrap the whole call in our configured timeout.
        let result = tokio::time::timeout(tool_timeout, client.call_tool(tool_name, args)).await;

        // Leaving the in-flight window and detecting teardown: if the
        // client was taken off the connection while the call ran (a
        // disconnect/disable/remove that did not wait for it), an error
        // result is re-attributed to the reconcile teardown with the
        // server named (spec R2's force-fail semantics).
        let client_still_live = {
            let mut conn = conn_arc.lock().await;
            conn.inflight_calls = conn.inflight_calls.saturating_sub(1);
            conn.client
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &client))
        };

        match result {
            Ok(Ok(result)) => Ok(result),
            // The error and timeout arms share the teardown re-attribution
            // (T1 review F-T1-2): a call whose connection was torn down
            // while it ran must fail naming the server, whether it failed
            // outright or hung until its own tool timeout.
            Ok(Err(e)) => {
                if client_still_live {
                    Err(e)
                } else {
                    Err(McpError::ConnectionFailed(format!(
                        "MCP server '{server_name}' was disconnected while the tool call was in flight (server restarting)"
                    )))
                }
            }
            Err(_) => {
                if client_still_live {
                    Err(McpError::Timeout)
                } else {
                    Err(McpError::ConnectionFailed(format!(
                        "MCP server '{server_name}' was disconnected while the tool call was in flight (server restarting)"
                    )))
                }
            }
        }
    }

    /// List all tools from all connected servers.
    ///
    /// Returns a list of `(server_name, tool_definition)` pairs. Only
    /// servers in the `Connected` state are included, and tool filtering
    /// (`enabled_tools` / `disabled_tools`) is applied.
    pub async fn list_all_tools(&self) -> Vec<(String, McpToolDefinition)> {
        let connections = self.connections.read().await;
        let mut all_tools = Vec::new();

        for (name, conn_arc) in connections.iter() {
            let conn = conn_arc.lock().await;
            if let McpConnectionState::Connected { tools, .. } = &conn.state {
                for tool in tools {
                    if conn.config.is_tool_allowed(&tool.name) {
                        all_tools.push((name.clone(), tool.clone()));
                    }
                }
            }
        }

        all_tools
    }

    /// Return a snapshot of all connection states.
    ///
    /// This is a synchronous (non-async) method that takes a read lock.
    /// It returns `(server_name, state)` pairs.
    pub async fn list_connections(&self) -> Vec<(String, McpConnectionState)> {
        let connections = self.connections.read().await;
        let mut result = Vec::new();

        for (name, conn_arc) in connections.iter() {
            let conn = conn_arc.lock().await;
            result.push((name.clone(), conn.state.clone()));
        }

        result
    }

    /// Return the config of a registered server, or `None` when no server of
    /// that name is registered.
    ///
    /// Used by the config hot-reload watcher to detect whether a server's
    /// on-disk config changed relative to the config it was connected with.
    pub async fn get_server_config(&self, name: &str) -> Option<McpServerConfig> {
        let connections = self.connections.read().await;
        let conn = connections.get(name)?.lock().await;
        Some(conn.config.clone())
    }

    /// Return a snapshot of every registered entry's config, keyed by
    /// server name — the runtime-state input of the reconcile diff
    /// ([`crate::reconcile::diff_pool_ops`]).
    pub async fn registered_configs(&self) -> HashMap<String, McpServerConfig> {
        let connections = self.connections.read().await;
        let mut snapshot = HashMap::with_capacity(connections.len());
        for (name, conn_arc) in connections.iter() {
            let conn = conn_arc.lock().await;
            snapshot.insert(name.clone(), conn.config.clone());
        }
        snapshot
    }

    /// Swap a registered server's stored config in place, leaving the
    /// connection (client and state) untouched.
    ///
    /// The refresh-without-reconnect path of the reconcile chain: tool
    /// filters and call timeouts are read from the stored config at use
    /// time ([`McpServerConfig::is_tool_allowed`],
    /// [`McpServerConfig::tool_timeout`]), so a filter-only change takes
    /// effect through this swap with no connection churn. Returns an error
    /// when the server is not registered.
    pub async fn update_server_config(&self, name: &str, config: McpServerConfig) -> McpResult<()> {
        let conn_arc = {
            let connections = self.connections.read().await;
            connections.get(name).cloned()
        };

        let conn_arc = match conn_arc {
            Some(arc) => arc,
            None => {
                return Err(McpError::ConnectionFailed(format!(
                    "no MCP server named '{name}' is registered"
                )));
            }
        };

        let mut conn = conn_arc.lock().await;
        conn.config = config;
        Ok(())
    }

    /// Wait until no tool call is in flight against the named server, or
    /// the timeout elapses.
    ///
    /// The reconcile path calls this before tearing a connection down
    /// (spec §4.3 / R2): in-flight calls get to complete; on timeout the
    /// caller proceeds anyway and the in-flight call fails with an error
    /// naming the server. An unregistered name reports idle immediately
    /// (nothing can be in flight under it). Returns `true` when the server
    /// went idle, `false` when the timeout elapsed first.
    pub async fn wait_for_idle(&self, name: &str, timeout: std::time::Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let idle = {
                let connections = self.connections.read().await;
                match connections.get(name) {
                    Some(conn_arc) => conn_arc.lock().await.inflight_calls == 0,
                    None => true,
                }
            };
            if idle {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    /// Register a status-change listener.
    ///
    /// The listener is called whenever a connection's state changes.
    /// Registration blocks briefly on the listener list's write lock —
    /// it can no longer be silently dropped by a concurrent notification
    /// (the historical `try_write` best-effort push).
    pub fn add_status_listener(&self, listener: StatusListener) {
        let mut listeners = self
            .status_listeners
            .write()
            .unwrap_or_else(|e| e.into_inner());
        listeners.push(listener);
    }

    /// Returns `true` if at least one MCP server is in the `Connected` state.
    ///
    /// Used by the `/meta` endpoint to report the `mcp` capability.
    pub async fn has_connected(&self) -> bool {
        let snapshots: Vec<_> = {
            let connections = self.connections.read().await;
            connections.values().map(Arc::clone).collect()
        };
        for conn_arc in snapshots {
            let conn = conn_arc.lock().await;
            if matches!(&conn.state, McpConnectionState::Connected { .. }) {
                return true;
            }
        }
        false
    }

    /// Reconnect to a single MCP server.
    ///
    /// Closes the existing client (if any) and performs a fresh connect +
    /// discover cycle.
    #[instrument(skip(self), fields(server = %name))]
    pub async fn reconnect(&self, name: &str) -> McpResult<()> {
        let conn_arc = {
            let connections = self.connections.read().await;
            connections.get(name).cloned()
        };

        let conn_arc = match conn_arc {
            Some(arc) => arc,
            None => {
                return Err(McpError::ConnectionFailed(format!(
                    "no MCP server named '{name}' is registered"
                )));
            }
        };

        // Close existing client.
        let old_client = {
            let mut conn = conn_arc.lock().await;
            let old = conn.client.take();
            conn.state = McpConnectionState::Pending;
            // Increment attempt_id so the in-flight connect will be accepted.
            conn.attempt_id += 1;
            old
        };

        if let Some(client) = old_client
            && let Err(e) = client.close().await
        {
            warn!(server = %name, error = %e, "error closing old client during reconnect");
        }

        self.notify_listeners(name.to_string(), McpConnectionState::Pending);

        // Perform fresh connect.
        let result = Self::do_connect(&conn_arc, self.oauth_token_dir.as_deref()).await;

        let state = conn_arc.lock().await.state.clone();
        self.notify_listeners(name.to_string(), state);

        result
    }

    /// Disable a server at runtime: disconnect and mark as `Disabled`.
    ///
    /// The connection's config `enabled` flag is cleared so the disabled
    /// state is reflected in [`get_server_config`](Self::get_server_config),
    /// and any in-flight connect result is invalidated so it cannot
    /// re-apply `Connected` after the server is disabled. Disabling a
    /// server that is not registered is a no-op.
    #[instrument(skip(self), fields(server = %name))]
    pub async fn disable_server(&self, name: &str) -> McpResult<()> {
        let conn_arc = {
            let connections = self.connections.read().await;
            connections.get(name).cloned()
        };

        let conn_arc = match conn_arc {
            Some(arc) => arc,
            None => return Ok(()), // Not registered — nothing to do.
        };

        let client = {
            let mut conn = conn_arc.lock().await;
            // Invalidate any in-flight connect so its result cannot
            // resurrect the connection after we mark it Disabled.
            conn.attempt_id += 1;
            conn.config.enabled = Some(false);
            conn.state = McpConnectionState::Disabled;
            conn.client.take()
        };

        if let Some(client) = client
            && let Err(e) = client.close().await
        {
            warn!(server = %name, error = %e, "error closing MCP client during disable");
        }

        self.notify_listeners(name.to_string(), McpConnectionState::Disabled);
        debug!(server = %name, "disabled");
        Ok(())
    }

    /// Enable a previously disabled server: reconnect.
    ///
    /// Sets `config.enabled` to `true`, closes any existing client (if the
    /// server is still connected), and performs a fresh connect + discover
    /// cycle via [`do_connect`](Self::do_connect).
    #[instrument(skip(self), fields(server = %name))]
    pub async fn enable_server(&self, name: &str) -> McpResult<()> {
        let conn_arc = {
            let connections = self.connections.read().await;
            connections.get(name).cloned()
        };

        let conn_arc = match conn_arc {
            Some(arc) => arc,
            None => {
                return Err(McpError::ConnectionFailed(format!(
                    "no MCP server named '{name}' is registered"
                )));
            }
        };

        // Re-enable and close any existing client.
        let old_client = {
            let mut conn = conn_arc.lock().await;
            conn.config.enabled = Some(true);
            let old = conn.client.take();
            conn.state = McpConnectionState::Pending;
            // Invalidate older in-flight connects; the fresh attempt takes
            // the new attempt_id.
            conn.attempt_id += 1;
            old
        };

        if let Some(client) = old_client
            && let Err(e) = client.close().await
        {
            warn!(server = %name, error = %e, "error closing old client during enable");
        }

        self.notify_listeners(name.to_string(), McpConnectionState::Pending);

        // Perform fresh connect.
        let result = Self::do_connect(&conn_arc, self.oauth_token_dir.as_deref()).await;

        let state = conn_arc.lock().await.state.clone();
        self.notify_listeners(name.to_string(), state);

        result
    }

    /// Enable a tool at runtime: remove it from the config's
    /// `disabled_tools` denylist so
    /// [`is_tool_allowed`](crate::config::McpServerConfig::is_tool_allowed)
    /// passes for it.
    ///
    /// Tool filtering is applied per call against the stored config, so no
    /// reconnect is needed. Returns an error when the server is not
    /// registered.
    #[instrument(skip(self), fields(server = %server_name, tool = %tool_name))]
    pub async fn enable_tool(&self, server_name: &str, tool_name: &str) -> McpResult<()> {
        let conn_arc = {
            let connections = self.connections.read().await;
            connections.get(server_name).cloned()
        };

        let conn_arc = match conn_arc {
            Some(arc) => arc,
            None => {
                return Err(McpError::ConnectionFailed(format!(
                    "no MCP server named '{server_name}' is registered"
                )));
            }
        };

        let mut conn = conn_arc.lock().await;
        if let Some(disabled) = &mut conn.config.disabled_tools {
            disabled.retain(|t| t != tool_name);
        }
        Ok(())
    }

    /// Disable a tool at runtime: add it to the config's `disabled_tools`
    /// denylist (unless already present) so
    /// [`is_tool_allowed`](crate::config::McpServerConfig::is_tool_allowed)
    /// blocks it.
    ///
    /// Tool filtering is applied per call against the stored config, so no
    /// reconnect is needed. Returns an error when the server is not
    /// registered.
    #[instrument(skip(self), fields(server = %server_name, tool = %tool_name))]
    pub async fn disable_tool(&self, server_name: &str, tool_name: &str) -> McpResult<()> {
        let conn_arc = {
            let connections = self.connections.read().await;
            connections.get(server_name).cloned()
        };

        let conn_arc = match conn_arc {
            Some(arc) => arc,
            None => {
                return Err(McpError::ConnectionFailed(format!(
                    "no MCP server named '{server_name}' is registered"
                )));
            }
        };

        let mut conn = conn_arc.lock().await;
        let disabled = conn.config.disabled_tools.get_or_insert_with(Vec::new);
        if !disabled.iter().any(|t| t == tool_name) {
            disabled.push(tool_name.to_string());
        }
        Ok(())
    }

    /// Record an observed unexpected connection loss (the TS
    /// `watchForUnexpectedClose` counterpart, driven by the tool-call
    /// failure path): the entry flips to `Failed` with a connection-loss
    /// error, the client is closed, and listeners are notified — the caller
    /// then schedules the automatic reconnection
    /// ([`Self::spawn_auto_reconnect`]). A name that is not registered is a
    /// no-op.
    #[instrument(skip(self), fields(server = %name))]
    pub async fn mark_unexpectedly_disconnected(&self, name: &str) -> McpResult<()> {
        let conn_arc = {
            let connections = self.connections.read().await;
            connections.get(name).cloned()
        };
        let conn_arc = match conn_arc {
            Some(arc) => arc,
            None => return Ok(()), // Not registered — nothing to mark.
        };

        let client = {
            let mut conn = conn_arc.lock().await;
            // Invalidate any in-flight connect so its result cannot
            // resurrect the connection over the loss we just recorded.
            conn.attempt_id += 1;
            let old = conn.client.take();
            conn.state = McpConnectionState::Failed {
                error: "connection to the MCP server was lost".to_string(),
            };
            old
        };
        if let Some(client) = client
            && let Err(e) = client.close().await
        {
            warn!(server = %name, error = %e, "error closing lost client");
        }

        self.notify_listeners(
            name.to_string(),
            McpConnectionState::Failed {
                error: "connection to the MCP server was lost".to_string(),
            },
        );
        debug!(server = %name, "marked unexpectedly disconnected");
        Ok(())
    }

    /// Schedule the automatic reconnection loop for `name` with the default
    /// backoff (see [`ReconnectBackoff::default`]).
    ///
    /// Single-flight per server: a newer trigger supersedes any loop already
    /// running. Recovery is event-driven — the successful `reconnect`
    /// notifies `Connected`, and the engine's status listener re-registers
    /// the server's tools.
    pub fn spawn_auto_reconnect(manager: Arc<Self>, name: &str) {
        Self::spawn_auto_reconnect_with(manager, name, ReconnectBackoff::default());
    }

    /// Schedule the automatic reconnection loop for `name` with an explicit
    /// backoff profile (the test seam; production callers use
    /// [`Self::spawn_auto_reconnect`]).
    ///
    /// The loop exits when the reconnection succeeds, when the server
    /// leaves the manager (removed / unregistered), when it settles in a
    /// state a reconnection cannot fix (`NeedsAuth` — user authorization
    /// required, or `Disabled`), when an external operation supersedes it
    /// (a newer trigger bumps the generation bookkeeping), or when the
    /// attempt budget is exhausted (`max_attempts` — a permanently broken
    /// server stops retrying instead of spinning forever).
    pub fn spawn_auto_reconnect_with(manager: Arc<Self>, name: &str, backoff: ReconnectBackoff) {
        let generation = {
            let mut gens = manager
                .auto_reconnect_gens
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let generation = gens.entry(name.to_string()).or_insert(0);
            *generation += 1;
            *generation
        };
        let name = name.to_string();
        info!(server = %name, ?backoff, "scheduling automatic MCP reconnection");
        tokio::spawn(async move {
            let mut delay = backoff.base;
            for attempt in 1..=backoff.max_attempts {
                // Superseded by a newer trigger, or a previous loop exited
                // and cleaned up: only the latest generation keeps running.
                let superseded = {
                    let gens = manager
                        .auto_reconnect_gens
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    gens.get(&name).copied() != Some(generation)
                };
                if superseded {
                    debug!(server = %name, "auto-reconnect loop superseded; exiting");
                    return;
                }
                // A server the manager no longer holds cannot be reconnected.
                let Some(config) = manager.get_server_config(&name).await else {
                    debug!(server = %name, "auto-reconnect target unregistered; exiting");
                    manager.clear_auto_reconnect(&name, generation);
                    return;
                };
                if !config.is_enabled() {
                    debug!(server = %name, "auto-reconnect target disabled; exiting");
                    manager.clear_auto_reconnect(&name, generation);
                    return;
                }

                let result = manager.reconnect(&name).await;
                let state = {
                    let connections = manager.connections.read().await;
                    match connections.get(&name) {
                        Some(conn) => Some(conn.lock().await.state.clone()),
                        None => None,
                    }
                };
                match (result, state) {
                    // Reconnected: the loop's job is done (the Connected
                    // notification re-registers the tools).
                    (Ok(()), Some(McpConnectionState::Connected { .. })) => {
                        info!(server = %name, attempt, "automatic MCP reconnection succeeded");
                        manager.clear_auto_reconnect(&name, generation);
                        return;
                    }
                    // Authorization is required — retrying cannot fix it.
                    (_, Some(McpConnectionState::NeedsAuth)) => {
                        info!(server = %name, "auto-reconnect target needs auth; exiting");
                        manager.clear_auto_reconnect(&name, generation);
                        return;
                    }
                    _ => {}
                }

                warn!(
                    server = %name, attempt,
                    max_attempts = backoff.max_attempts,
                    "automatic MCP reconnection attempt failed; backing off"
                );
                if attempt == backoff.max_attempts {
                    warn!(server = %name, "automatic MCP reconnection gave up after the attempt budget");
                    manager.clear_auto_reconnect(&name, generation);
                    return;
                }
                tokio::time::sleep(delay).await;
                delay = std::cmp::min(delay.mul_f64(backoff.factor), backoff.cap);
            }
        });
    }

    /// Remove this loop's generation bookkeeping when it owns the entry.
    fn clear_auto_reconnect(&self, name: &str, generation: u32) {
        let mut gens = self
            .auto_reconnect_gens
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if gens.get(name).copied() == Some(generation) {
            gens.remove(name);
        }
    }

    /// Notify all registered listeners of a state change.
    fn notify_listeners(&self, server_name: String, state: McpConnectionState) {
        if let Ok(listeners) = self.status_listeners.read() {
            for listener in listeners.iter() {
                listener(server_name.clone(), state.clone());
            }
        }
    }
}

impl Default for McpConnectionManager {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// OAuth applicability + reconnect backoff
// ---------------------------------------------------------------------------

/// Whether the server is a remote transport (HTTP/SSE) with OAuth
/// configured — the 401 flow's applicability test (adjudication A4: stdio
/// servers never produce a 401, so they are out of scope by design).
fn uses_remote_oauth(config: &McpServerConfig) -> bool {
    matches!(
        config.transport,
        crate::transport::McpTransportConfig::Http { .. }
            | crate::transport::McpTransportConfig::Sse { .. }
    ) && matches!(config.auth, Some(McpAuthConfig::OAuth { .. }))
}

/// The failure state for a connect-path error: a 401-class failure
/// (`AuthRequired`) on a remote server with OAuth configured lands the
/// connection in `needs-auth` (the authenticate tool drives the flow from
/// there); everything else is an ordinary `Failed` carrying `error_text`.
fn connect_failure_state(
    config: &McpServerConfig,
    error: &McpError,
    error_text: &str,
) -> McpConnectionState {
    if matches!(error, McpError::AuthRequired) && uses_remote_oauth(config) {
        McpConnectionState::NeedsAuth
    } else {
        McpConnectionState::Failed {
            error: error_text.to_string(),
        }
    }
}

/// The automatic-reconnection backoff profile: after each failed attempt
/// the delay grows by `factor` up to `cap`; after `max_attempts` attempts
/// the loop gives up (a permanently broken server stops retrying instead
/// of spinning forever — the manual `mcp.reconnect` and a config-write
/// reconcile remain available).
#[derive(Debug, Clone, Copy)]
pub struct ReconnectBackoff {
    /// Delay before the first retry.
    pub base: Duration,
    /// Multiplier applied after each failed attempt.
    pub factor: f64,
    /// Upper bound for the delay between attempts.
    pub cap: Duration,
    /// Total attempt budget (the initial attempt counts as one).
    pub max_attempts: u32,
}

impl Default for ReconnectBackoff {
    fn default() -> Self {
        Self {
            base: Duration::from_secs(1),
            factor: 2.0,
            cap: Duration::from_secs(30),
            max_attempts: 8,
        }
    }
}

impl std::fmt::Debug for McpConnectionManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpConnectionManager")
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::McpTransportConfig;

    fn make_config(command: &str) -> McpServerConfig {
        McpServerConfig {
            transport: McpTransportConfig::Stdio {
                command: command.to_string(),
                args: vec![],
                env: None,
                cwd: None,
            },
            enabled: None,
            startup_timeout_ms: Some(1000),
            tool_timeout_ms: None,
            enabled_tools: None,
            disabled_tools: None,
            auth: None,
        }
    }

    #[tokio::test]
    async fn should_report_failure_when_server_connection_fails() {
        // "echo" is not a valid MCP server — it will fail to handshake.
        // But the manager should not panic and should report the failure.
        let manager = McpConnectionManager::new();
        let mut configs = HashMap::new();
        configs.insert("bad-server".to_string(), make_config("echo"));

        let results = manager.connect_all(configs).await;

        let result = results.get("bad-server").unwrap();
        assert!(result.is_err(), "expected connection failure for echo");

        let connections = manager.list_connections().await;
        let (_, state) = &connections[0];
        assert!(
            matches!(state, McpConnectionState::Failed { .. }),
            "expected Failed state, got {state:?}"
        );
    }

    #[tokio::test]
    async fn should_skip_server_when_disabled_in_config() {
        let manager = McpConnectionManager::new();
        let config = McpServerConfig {
            transport: McpTransportConfig::Stdio {
                command: "echo".to_string(),
                args: vec![],
                env: None,
                cwd: None,
            },
            enabled: Some(false),
            startup_timeout_ms: None,
            tool_timeout_ms: None,
            enabled_tools: None,
            disabled_tools: None,
            auth: None,
        };
        let mut configs = HashMap::new();
        configs.insert("disabled-server".to_string(), config);

        let results = manager.connect_all(configs).await;
        let result = results.get("disabled-server").unwrap();
        assert!(result.is_ok(), "disabled server should return Ok");

        let connections = manager.list_connections().await;
        let (_, state) = &connections[0];
        assert_eq!(state, &McpConnectionState::Disabled);
    }

    #[tokio::test]
    async fn should_return_error_when_calling_tool_on_unregistered_server() {
        let manager = McpConnectionManager::new();
        let result = manager
            .call_tool("nonexistent", "tool", serde_json::json!({}))
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn should_return_ok_when_disconnecting_nonexistent_server() {
        let manager = McpConnectionManager::new();
        let result = manager.disconnect("nonexistent").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn should_unregister_connection_when_remove_is_called() {
        // Given a manager with a registered (disabled) server
        let manager = McpConnectionManager::new();
        let config = McpServerConfig {
            transport: McpTransportConfig::Stdio {
                command: "echo".to_string(),
                args: vec![],
                env: None,
                cwd: None,
            },
            enabled: Some(false),
            startup_timeout_ms: None,
            tool_timeout_ms: None,
            enabled_tools: None,
            disabled_tools: None,
            auth: None,
        };
        manager
            .connect_one("temp-server", config)
            .await
            .expect("disabled server should connect ok");
        assert_eq!(manager.list_connections().await.len(), 1);

        // When remove is called
        manager
            .remove("temp-server")
            .await
            .expect("remove should succeed");

        // Then the registration is gone entirely, not left behind in Pending
        assert!(
            manager.list_connections().await.is_empty(),
            "remove must delete the connection entry"
        );
    }

    #[tokio::test]
    async fn should_return_ok_when_removing_nonexistent_server() {
        // Given an empty manager
        let manager = McpConnectionManager::new();

        // When removing a server that was never registered
        let result = manager.remove("ghost").await;

        // Then removal is a no-op success
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn should_return_empty_when_no_servers_connected() {
        let manager = McpConnectionManager::new();
        let tools = manager.list_all_tools().await;
        assert!(tools.is_empty());
    }

    #[tokio::test]
    async fn should_fire_listener_when_connection_state_changes() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let manager = McpConnectionManager::new();
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = Arc::clone(&counter);
        manager.add_status_listener(Box::new(move |_name, _state| {
            counter_clone.fetch_add(1, Ordering::Relaxed);
        }));

        let mut configs = HashMap::new();
        configs.insert("bad".to_string(), make_config("echo"));
        let _ = manager.connect_all(configs).await;

        // Should have fired at least once (Pending initial, then Failed).
        assert!(counter.load(Ordering::Relaxed) > 0);
    }

    #[tokio::test]
    async fn should_return_disabled_servers_in_toggle_state_when_server_is_disabled() {
        // Given a registered server that is disabled and has a denied tool
        let manager = McpConnectionManager::new();
        let mut config = make_config("echo");
        config.enabled = Some(false);
        config.disabled_tools = Some(vec!["secret_tool".to_string()]);
        manager
            .connect_one("off-server", config)
            .await
            .expect("disabled server should register ok");
        manager
            .disable_server("off-server")
            .await
            .expect("disable should succeed");

        // When the toggle-state sources are read (the same aggregation the
        // embedded `mcp.toggleState` dispatch performs)
        let disabled_servers: Vec<String> = manager
            .list_connections()
            .await
            .into_iter()
            .filter(|(_, state)| matches!(state, McpConnectionState::Disabled))
            .map(|(name, _)| name)
            .collect();
        let disabled_tools: Vec<String> = manager
            .get_server_config("off-server")
            .await
            .expect("config should exist")
            .disabled_tools
            .unwrap_or_default()
            .into_iter()
            .map(|tool| format!("off-server/{tool}"))
            .collect();

        // Then toggle state contains the disabled server and its denied tools
        assert_eq!(disabled_servers, vec!["off-server".to_string()]);
        assert_eq!(disabled_tools, vec!["off-server/secret_tool".to_string()]);
    }

    #[tokio::test]
    async fn should_enable_server_at_runtime_when_enable_server_is_called() {
        // Given a disabled server (never attempted to connect)
        let manager = McpConnectionManager::new();
        let mut config = make_config("echo");
        config.enabled = Some(false);
        manager
            .connect_one("off-server", config)
            .await
            .expect("disabled server should register ok");
        assert_eq!(
            manager
                .get_server_config("off-server")
                .await
                .unwrap()
                .enabled,
            Some(false)
        );

        // When enable_server is called
        let _ = manager.enable_server("off-server").await;

        // Then the config is re-enabled and a connect attempt is made
        // ("echo" cannot complete the MCP handshake, so it ends Failed).
        assert_eq!(
            manager
                .get_server_config("off-server")
                .await
                .unwrap()
                .enabled,
            Some(true)
        );
        let connections = manager.list_connections().await;
        let (_, state) = connections.iter().find(|(n, _)| n == "off-server").unwrap();
        assert!(
            matches!(state, McpConnectionState::Failed { .. }),
            "expected Failed after enabling, got {state:?}"
        );
    }

    #[tokio::test]
    async fn should_disable_server_at_runtime_when_disable_server_is_called() {
        // Given a registered server whose connect attempt failed
        let manager = McpConnectionManager::new();
        let _ = manager
            .connect_one("live-server", make_config("echo"))
            .await;

        // When disable_server is called
        manager
            .disable_server("live-server")
            .await
            .expect("disable should succeed");

        // Then the server is marked Disabled and the config flag is cleared
        let connections = manager.list_connections().await;
        let (_, state) = connections
            .iter()
            .find(|(n, _)| n == "live-server")
            .unwrap();
        assert_eq!(state, &McpConnectionState::Disabled);
        assert_eq!(
            manager
                .get_server_config("live-server")
                .await
                .unwrap()
                .enabled,
            Some(false)
        );
    }

    #[tokio::test]
    async fn should_block_tool_calls_when_tool_is_disabled() {
        // Given a registered server (tool filtering applies against the
        // stored config even before a connection is established)
        let manager = McpConnectionManager::new();
        let mut config = make_config("echo");
        config.enabled = Some(false);
        manager
            .connect_one("srv", config)
            .await
            .expect("disabled server should register ok");

        // When the tool is disabled at runtime
        manager
            .disable_tool("srv", "blocked")
            .await
            .expect("disable_tool should succeed");

        // Then it lands in the denylist and the call-time filter blocks it
        // (`call_tool` rejects via `is_tool_allowed`).
        let cfg = manager.get_server_config("srv").await.unwrap();
        assert_eq!(
            cfg.disabled_tools.as_deref(),
            Some(&["blocked".to_string()][..])
        );
        assert!(!cfg.is_tool_allowed("blocked"));
        assert!(cfg.is_tool_allowed("other"));
    }

    /// Given a pool with two registered (disabled) servers, when the
    /// registered configs are snapshotted, then every entry's config is
    /// returned keyed by name — the reconcile diff's runtime-state input.
    #[tokio::test]
    async fn should_return_the_registered_configs_when_snapshotting_the_pool() {
        let manager = McpConnectionManager::new();
        let mut alpha = make_config("echo");
        alpha.enabled = Some(false);
        let mut beta = make_config("cat");
        beta.enabled = Some(false);
        manager
            .connect_one("alpha", alpha)
            .await
            .expect("disabled entry registers without spawning");
        manager
            .connect_one("beta", beta)
            .await
            .expect("disabled entry registers without spawning");

        let snapshot = manager.registered_configs().await;

        assert_eq!(snapshot.len(), 2, "one config per registered entry");
        assert_eq!(snapshot["alpha"].transport, make_config("echo").transport);
        assert_eq!(snapshot["beta"].transport, make_config("cat").transport);
    }

    /// Given a registered server, when its stored config is updated in
    /// place, then the new config is visible to `get_server_config` while
    /// the connection state and registration are untouched — the
    /// refresh-without-reconnect path (tool filters and call timeouts are
    /// read from the stored config at use time).
    #[tokio::test]
    async fn should_swap_the_stored_config_without_touching_the_state_when_updating_it() {
        let manager = McpConnectionManager::new();
        let mut config = make_config("echo");
        config.enabled = Some(false);
        manager
            .connect_one("srv", config)
            .await
            .expect("disabled entry registers without spawning");

        let mut updated = make_config("echo");
        updated.enabled = Some(false);
        updated.enabled_tools = Some(vec!["only-this".to_string()]);
        manager
            .update_server_config("srv", updated.clone())
            .await
            .expect("updating a registered server's config must succeed");

        assert_eq!(
            manager.get_server_config("srv").await,
            Some(updated),
            "the stored config is the new one"
        );
        let connections = manager.list_connections().await;
        assert_eq!(connections.len(), 1, "the registration stays");
        assert_eq!(
            connections[0].1,
            McpConnectionState::Disabled,
            "the state is untouched by a config-only update"
        );

        // An unregistered name answers an error instead of silently
        // succeeding.
        assert!(
            manager
                .update_server_config("ghost", make_config("echo"))
                .await
                .is_err(),
            "updating an unregistered server must fail"
        );
    }

    /// Given a registered server with no tool call in flight (and an
    /// unregistered name), when the idle wait runs, then it reports idle
    /// immediately — the reconcile disconnect path never stalls on a quiet
    /// pool.
    #[tokio::test]
    async fn should_report_idle_immediately_when_no_call_is_in_flight() {
        let manager = McpConnectionManager::new();
        let mut config = make_config("echo");
        config.enabled = Some(false);
        manager
            .connect_one("quiet", config)
            .await
            .expect("disabled entry registers without spawning");

        let started = std::time::Instant::now();
        assert!(
            manager
                .wait_for_idle("quiet", std::time::Duration::from_secs(5))
                .await,
            "a quiet registered server must report idle"
        );
        assert!(
            manager
                .wait_for_idle("never-registered", std::time::Duration::from_secs(5))
                .await,
            "an unregistered name must report idle (nothing can be in flight)"
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(1),
            "both waits must return immediately, took {:?}",
            started.elapsed()
        );
    }
}
