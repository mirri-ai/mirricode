//! Shared server state handed to every transport handler.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use tokio::sync::{broadcast, watch};
use tokio_util::task::TaskTracker;
use uuid::Uuid;

use mirri_core::{ServerConfig, SessionManager};
use mirri_mcp::connection::McpConnectionManager;
use mirri_transport::adapter::EngineAdapter;

use crate::rpc::{InFlightTurns, RpcError, build_transport_registry};
use crate::ws_rpc::{EVENT_LOG_CAPACITY, SessionEventChannelRegistry};

/// Capacity of the app-level MCP event broadcast (`mcp.subscribe`, M7
/// F-9) — pool status transitions are a management-plane trickle, and a
/// lagged receiver drops events (the durable record is the config layer).
const MCP_EVENT_CHANNEL_CAPACITY: usize = 256;

/// Shared state for all transport servers (HTTP, WebSocket, IPC).
///
/// Cloning is cheap (every field is an `Arc` or a cheap `Clone`), so a clone
/// can be handed to each spawned connection handler.
///
/// After the M5-B3a thinning this is transport + auth + projection state
/// only: the engine lives behind the [`EngineAdapter`] seam (`engine`), the
/// transport registry owns the 24 serve-only handlers (`rpc_registry`), and
/// [`Self::dispatch`] is the single entry point every transport shares.
#[derive(Clone)]
pub struct ServerState {
    /// The session manager every RPC method operates on.
    pub session_manager: Arc<SessionManager>,
    /// The session-routing seam (W3, design 08 §3): every session-addressing
    /// transport-owned handler and WS subscription resolves the addressed
    /// session through here instead of holding `session_manager` — the
    /// composition root injects a registry-backed implementation so a session
    /// of a non-default workspace resolves on its own runtime. Defaults to
    /// the single-manager routing (with the engine restore hook attached).
    pub(crate) session_routing: Arc<dyn crate::rpc::SessionRouting>,
    /// Registry of the transport-owned RPC handlers shared by HTTP, WS, and
    /// IPC dispatch. Engine-domain methods are not registered here — they
    /// fall through to [`Self::engine`] via [`Self::dispatch`] when (and only
    /// when) [`mirri_proto::rpc`] marks them as served on the serve wire.
    pub rpc_registry: Arc<crate::rpc::RpcRegistry>,
    /// The engine behind the server: every method the transport registry
    /// does not own is dispatched to it (the registry gate in
    /// [`Self::dispatch`] keeps the serve wire surface exactly the
    /// `mirri_proto::rpc` availability facts).
    engine: Arc<dyn EngineAdapter>,
    /// Fire-mode `message.send` engine calls tracked by the dispatch wrapper
    /// (keyed `"{session_id}:{agent_id}:{turn_id}"`), so the transport's
    /// `session.status` / `session.snapshot` keep their `running`/`busy`
    /// derivation. The map is shared with the transport registry handlers
    /// and the in-flight watcher (`ws_rpc::spawn_in_flight_watcher`).
    in_flight_turns: InFlightTurns,
    /// Server version string reported by `GET /api/v1/meta`.
    pub version: String,
    /// Stable per-process server identifier reported by `GET /api/v1/meta`.
    pub server_id: String,
    /// When this server process was started (UTC).
    pub started_at: DateTime<Utc>,
    /// Watch channel; sending `true` requests a graceful shutdown of every
    /// transport server (HTTP server's `with_graceful_shutdown` signal).
    pub shutdown_tx: watch::Sender<bool>,
    /// Optional shared secret for Bearer-token authentication of the HTTP
    /// API. `Some` requires `Authorization: Bearer <token>` on every route
    /// except the public `healthz`/`meta` probes; `None` (the default) runs
    /// in open (development) mode where all routes are unauthenticated (the
    /// shutdown route still only accepts loopback peers). Never log this
    /// value.
    pub auth_token: Option<String>,
    /// Application-level heartbeat interval for the unified JSON-RPC
    /// WebSocket endpoint (`/api/v1/rpc`): the server sends a `ping`
    /// notification every interval and drops a connection that sends no
    /// frame for `3 × interval` (design §5.5). The value is advertised to
    /// clients as `heartbeatMs` in `server_hello`.
    pub heartbeat_interval: Duration,
    /// Per-session durable event logs and live fan-out backing `/api/v1/rpc`
    /// cursor replay. Shared by every connection (each channel survives the
    /// connections subscribed to it), so a client that disconnects and
    /// resubscribes with its cursor resumes where it left off.
    pub session_event_channels: Arc<SessionEventChannelRegistry>,
    /// Optional MCP connection manager. When present, the `/meta` endpoint
    /// reports `mcp: true` if at least one MCP server is connected.
    pub mcp_manager: Option<Arc<McpConnectionManager>>,
    /// Shared server configuration. B3a keeps the handle for the CLI's
    /// hot-reload watcher and `/meta`; the config mutating RPC methods
    /// (`config.get` / `config.set`) are engine-domain methods now.
    pub config: Arc<RwLock<ServerConfig>>,
    /// Where `config` was loaded from / `config.set` persists; `None` means
    /// no config file was loaded.
    pub config_path: Option<PathBuf>,
    /// Root of the files store backing `POST/GET /api/v1/files` — the same
    /// directory the engine's `message.send` entry resolution reads, so an
    /// uploaded file is immediately attachable to a prompt. `None` falls
    /// back to the ambient Mirri home's `files/` subdir at request time.
    pub files_dir: Option<PathBuf>,
    /// The app-level MCP pool event broadcast (M7, F-9): `event.mcp.server
    /// .status` notifications fanned out to every connection that called
    /// `mcp.subscribe`. Fed by a single forwarder task (spawned lazily on
    /// the first subscriber, [`Self::mcp_event_rx`]) that drains the
    /// engine's `subscribe_mcp_pool_events` broadcast and re-wraps each
    /// payload as a notification.
    mcp_event_tx: broadcast::Sender<mirri_transport::frame::JsonRpcNotification>,
    /// Guards the one-time spawn of the engine → broadcast forwarder (see
    /// [`Self::mcp_event_rx`]) — cheap to clone into the shared state.
    mcp_event_forwarder: Arc<std::sync::OnceLock<()>>,
    /// The app-level skill-catalog change event broadcast (ADR-010 Task
    /// 6): `skill.catalogChanged` notifications fanned out to every
    /// connection that called `skill.subscribe`. Fed by a single forwarder
    /// task (spawned lazily on the first subscriber,
    /// [`Self::skill_catalog_event_rx`]) that drains the engine's
    /// `subscribe_skill_catalog_events` broadcast and re-wraps each
    /// payload as a notification — the `mcp_event_rx` precedent shape.
    skill_catalog_event_tx: broadcast::Sender<mirri_transport::frame::JsonRpcNotification>,
    /// Guards the one-time spawn of the skill-catalog engine → broadcast
    /// forwarder (see [`Self::skill_catalog_event_rx`]).
    skill_catalog_event_forwarder: Arc<std::sync::OnceLock<()>>,
    /// Tracks every in-flight engine-call task (the spawned wrapper behind
    /// [`Self::dispatch_engine`]). The graceful-shutdown drain
    /// ([`Self::drain_engine_calls`]) waits on this tracker so in-flight
    /// RPCs complete — and their responses reach the clients — before the
    /// server returns and the runtime is dropped. Without the drain, the
    /// runtime drop aborts the tasks mid-call and every in-flight RPC
    /// fails with a dropped oneshot (the "engine call task vanished"
    /// incident, 2026-10-12).
    engine_call_tracker: Arc<TaskTracker>,
}

impl ServerState {
    /// Create a new server state bound to the given engine, session manager,
    /// version, and optional bearer auth token.
    ///
    /// The transport registry is populated with the 24 transport-owned
    /// handlers (see [`build_transport_registry`]); every other serve-wire
    /// method is dispatched to `engine` by [`Self::dispatch`].
    pub fn with_engine(
        engine: Arc<dyn EngineAdapter>,
        session_manager: Arc<SessionManager>,
        version: String,
        auth_token: Option<String>,
    ) -> Self {
        let started_at = Utc::now();
        let server_id = Uuid::new_v4().to_string();
        let (shutdown_tx, _shutdown_rx) = watch::channel(false);
        let session_event_channels = Arc::new(SessionEventChannelRegistry::new(EVENT_LOG_CAPACITY));
        let in_flight_turns = Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
        let config = Arc::new(RwLock::new(ServerConfig::default()));
        // Restore-on-demand hook: a transport-owned `session.get` /
        // `session.snapshot` addressing a persisted-only session (a server
        // restart left it on disk) resumes it through the engine adapter —
        // the full embedded assembly instead of a bare wire replay.
        let engine_for_restore = Arc::clone(&engine);
        let restore_hook: crate::rpc::EngineRestoreHook = Arc::new(move |session_id| {
            let engine = Arc::clone(&engine_for_restore);
            Box::pin(async move {
                engine
                    .call_method(
                        "session.restore",
                        serde_json::json!({ "sessionId": session_id }),
                    )
                    .await
                    .map(|_| ())
            })
        });
        // The single-manager routing carries the hook — the default wiring
        // the composition root's `with_session_routing` replaces (W3).
        let session_routing: Arc<dyn crate::rpc::SessionRouting> =
            Arc::new(crate::rpc::SingleManagerRouting::with_restore_hook(
                Arc::clone(&session_manager),
                Some(restore_hook),
            ));
        let rpc_registry = Arc::new(build_transport_registry(
            Arc::clone(&engine),
            Arc::clone(&session_manager),
            server_id.clone(),
            Arc::clone(&session_event_channels),
            Arc::clone(&in_flight_turns),
            Arc::clone(&session_routing),
            Arc::clone(&config),
        ));
        Self {
            session_manager,
            session_routing,
            rpc_registry,
            engine,
            in_flight_turns,
            version,
            server_id,
            started_at,
            shutdown_tx,
            auth_token,
            heartbeat_interval: Duration::from_secs(15),
            session_event_channels,
            mcp_manager: None,
            config,
            config_path: None,
            files_dir: None,
            mcp_event_tx: broadcast::channel(MCP_EVENT_CHANNEL_CAPACITY).0,
            mcp_event_forwarder: Arc::new(std::sync::OnceLock::new()),
            skill_catalog_event_tx: broadcast::channel(MCP_EVENT_CHANNEL_CAPACITY).0,
            skill_catalog_event_forwarder: Arc::new(std::sync::OnceLock::new()),
            engine_call_tracker: Arc::new(TaskTracker::new()),
        }
    }

    /// Route every session-addressing transport-owned handler and WS
    /// subscription through an explicit [`SessionRouting`]
    /// implementation (the composition root's registry-backed routing, W3):
    /// a session of a non-default workspace resolves on its own runtime
    /// (active table → index → bucket probe → not-found — never a silent
    /// default). Rebuilds the transport registry so its handlers share the
    /// new routing.
    pub fn with_session_routing(mut self, routing: Arc<dyn crate::rpc::SessionRouting>) -> Self {
        self.rpc_registry = Arc::new(build_transport_registry(
            Arc::clone(&self.engine),
            Arc::clone(&self.session_manager),
            self.server_id.clone(),
            Arc::clone(&self.session_event_channels),
            Arc::clone(&self.in_flight_turns),
            Arc::clone(&routing),
            Arc::clone(&self.config),
        ));
        self.session_routing = routing;
        self
    }

    /// A fresh receiver on the app-level MCP pool event broadcast, and the
    /// lazy start of the engine → broadcast forwarder (M7, F-9): the first
    /// `mcp.subscribe` spawns the single task that drains the engine's
    /// `subscribe_mcp_pool_events` broadcast and re-wraps every payload as
    /// an `event.mcp.server.status` notification on this channel. Engines
    /// without an app-level MCP surface answer `MethodNotFound` — the
    /// forwarder logs and exits, and subscribers simply never observe an
    /// event (the honest shape for a non-MCP engine).
    ///
    /// **Process-lifetime assumption (a ledger fact, not an accident):** the
    /// forwarder is spawned once per [`ServerState`] (guarded by the
    /// [`OnceLock`](std::sync::OnceLock) in [`Self::mcp_event_forwarder`])
    /// and has **no shutdown path by design** — the serve binary runs it
    /// until the process exits, exactly like the state's other background
    /// fans (the session event log tasks). The task holds one receiver on
    /// the engine's broadcast and a sender clone for as long as the state
    /// lives. Hosts that swap engines behind a long-lived [`ServerState`]
    /// must be aware of this: the forwarder keeps draining the engine it
    /// was started against, and a replaced engine's pool events will not
    /// reach this channel (build a fresh state per engine instead).
    pub(crate) fn mcp_event_rx(
        &self,
    ) -> broadcast::Receiver<mirri_transport::frame::JsonRpcNotification> {
        if self.mcp_event_forwarder.set(()).is_ok() {
            let engine = Arc::clone(&self.engine);
            let tx = self.mcp_event_tx.clone();
            tokio::spawn(async move {
                let mut rx = match engine.subscribe_mcp_pool_events() {
                    Ok(rx) => rx,
                    Err(error) => {
                        tracing::debug!(
                            %error,
                            "engine has no app-level MCP pool events; mcp.subscribe stays silent"
                        );
                        return;
                    }
                };
                loop {
                    match rx.recv().await {
                        Ok(payload) => {
                            let notification = mirri_transport::frame::JsonRpcNotification::new(
                                "event.mcp.server.status",
                                (*payload).clone(),
                            );
                            // A send failure means "no subscribers" — the
                            // forwarder keeps draining either way, so a
                            // late subscriber never misses the engine's
                            // channel position.
                            let _ = tx.send(notification);
                        }
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            tracing::warn!(
                                skipped,
                                "MCP pool event forwarder lagged; dropped events"
                            );
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
        }
        self.mcp_event_tx.subscribe()
    }

    /// A fresh receiver on the app-level skill-catalog change broadcast,
    /// and the lazy start of its engine → broadcast forwarder (ADR-010
    /// Task 6, the [`Self::mcp_event_rx`] precedent shape): the first
    /// `skill.subscribe` spawns the single task that drains the engine's
    /// `subscribe_skill_catalog_events` broadcast and re-wraps every
    /// payload as a `skill.catalogChanged` notification on this channel.
    /// Engines without a skill-catalog surface answer `MethodNotFound` —
    /// the forwarder logs and exits, and subscribers simply never observe
    /// an event (the honest shape for a non-watching engine).
    ///
    /// **Process-lifetime assumption** (same ledger fact as
    /// [`Self::mcp_event_rx`]): the forwarder is spawned once per
    /// [`ServerState`] and has no shutdown path by design — the serve
    /// binary runs it until the process exits.
    pub(crate) fn skill_catalog_event_rx(
        &self,
    ) -> broadcast::Receiver<mirri_transport::frame::JsonRpcNotification> {
        if self.skill_catalog_event_forwarder.set(()).is_ok() {
            let engine = Arc::clone(&self.engine);
            let tx = self.skill_catalog_event_tx.clone();
            tokio::spawn(async move {
                let mut rx = match engine.subscribe_skill_catalog_events() {
                    Ok(rx) => rx,
                    Err(error) => {
                        tracing::debug!(
                            %error,
                            "engine has no app-level skill catalog events; skill.subscribe stays silent"
                        );
                        return;
                    }
                };
                loop {
                    match rx.recv().await {
                        Ok(payload) => {
                            let notification =
                                mirri_transport::frame::JsonRpcNotification::new(
                                    "skill.catalogChanged",
                                    payload,
                                );
                            // A send failure means "no subscribers" — the
                            // forwarder keeps draining either way.
                            let _ = tx.send(notification);
                        }
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            tracing::warn!(
                                skipped,
                                "skill catalog event forwarder lagged; dropped events"
                            );
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
        }
        self.skill_catalog_event_tx.subscribe()
    }

    /// Configure the application-level heartbeat interval for the unified
    /// JSON-RPC WebSocket endpoint (`/api/v1/rpc`); the default is 15
    /// seconds.
    pub fn with_heartbeat_interval(mut self, interval: Duration) -> Self {
        self.heartbeat_interval = interval;
        self
    }

    /// Attach the MCP connection manager so `/meta` can report the `mcp`
    /// capability accurately.
    pub fn with_mcp_manager(mut self, manager: Arc<McpConnectionManager>) -> Self {
        self.mcp_manager = Some(manager);
        self
    }

    /// Attach the shared server config and its persisted path.
    ///
    /// Since the M5-B3a thinning the transport registry holds no config
    /// handlers (`config.get`/`config.set` are engine-domain methods), so
    /// the state simply stores the shared values — the CLI's hot-reload
    /// watcher and the engine observe them directly. The registry is
    /// rebuilt with the new Arc so config-reading handlers
    /// (`session.status`'s model-window read) observe the attached
    /// snapshot too — the same rebuild `with_session_routing` performs.
    pub fn with_config(
        mut self,
        config: Arc<RwLock<ServerConfig>>,
        config_path: Option<PathBuf>,
    ) -> Self {
        self.config = config;
        self.config_path = config_path;
        self.rpc_registry = Arc::new(build_transport_registry(
            Arc::clone(&self.engine),
            Arc::clone(&self.session_manager),
            self.server_id.clone(),
            Arc::clone(&self.session_event_channels),
            Arc::clone(&self.in_flight_turns),
            Arc::clone(&self.session_routing),
            Arc::clone(&self.config),
        ));
        self
    }

    /// Attach the files-store root the `/api/v1/files` routes use. Without
    /// it the routes resolve the ambient Mirri home's `files/` subdir at
    /// request time (the same directory the engine's entry resolution uses
    /// when it was built without an injected root — `mirri serve` passes
    /// both explicitly so the two always agree).
    pub fn with_files_dir(mut self, files_dir: PathBuf) -> Self {
        self.files_dir = Some(files_dir);
        self
    }

    /// The files-store root of this server (the injected override, or the
    /// ambient Mirri home's `files/` subdir).
    pub(crate) fn file_store(&self) -> mirri_persistence::FileStore {
        let root = self.files_dir.clone().unwrap_or_else(|| {
            mirri_core::config::mirri_home_dir()
                .map(|home| home.join("files"))
                .unwrap_or_else(|_| PathBuf::from(".mirri-code-files"))
        });
        mirri_persistence::FileStore::new(root)
    }

    /// Request a graceful shutdown of all transport servers.
    ///
    /// Uses `send_replace` so the flag is recorded even when no receiver is
    /// currently subscribed (e.g. tests running `http::serve` directly).
    pub fn request_shutdown(&self) {
        let _ = self.shutdown_tx.send_replace(true);
    }

    /// Get a receiver for the shutdown signal.
    pub fn shutdown_rx(&self) -> watch::Receiver<bool> {
        self.shutdown_tx.subscribe()
    }

    /// Drain every in-flight engine call before the server returns — the
    /// graceful-shutdown backstop. Closes the tracker (no new tracked
    /// tasks; already-spawned ones keep running) and waits for them to
    /// finish, bounded by `timeout` so a wedged engine call cannot hang
    /// the shutdown forever. Connections and their writers are still
    /// alive at this point, so the drained calls' responses reach their
    /// clients instead of dying as "task vanished" errors.
    pub async fn drain_engine_calls(&self, timeout: Duration) {
        self.engine_call_tracker.close();
        let wait = self.engine_call_tracker.wait();
        if tokio::time::timeout(timeout, wait).await.is_err() {
            tracing::warn!(
                ?timeout,
                "graceful shutdown timed out waiting for in-flight engine calls; aborting them"
            );
        }
    }

    /// Dispatch one RPC call — the single entry point all three transports
    /// share (WS `dispatch_registry`, HTTP `rpc_call`, IPC `handle_request`).
    ///
    /// Semantics (M5-B3a):
    ///
    /// 1. **Transport registry first** — methods with a local handler
    ///    (`session.get`, `file.*`, `btw.start`, …) run here.
    /// 2. **Engine fall-through only when the registry gate passes** — the
    ///    method must be a [`mirri_proto::rpc`] row whose availability
    ///    `serves_server()`. This keeps the serve wire surface exactly the
    ///    58 methods of today's fact table: embedded-only methods
    ///    (`cron.list`, `plan.enter`, …) and unknown names answer
    ///    `-32601`.
    /// 3. **In-flight tracking** — a fire-mode `message.send` engine call is
    ///    registered in the shared in-flight map (keyed
    ///    `"{session_id}:{agent_id}:{turn_id}"`) and reaped by a watcher on
    ///    the session's terminal turn event, so `session.status` /
    ///    `session.snapshot` keep their `running`/`busy` derivation.
    pub async fn dispatch(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        let params = if params.is_null() { json!({}) } else { params };

        if let Some(handler) = self.rpc_registry.get(method) {
            return handler.handle(params).await;
        }

        // The registry gate: the serve wire surface is a recorded fact, not
        // a routing rule — no method appears on the serve wire that the fact
        // table does not mark `serves_server`.
        let Some(entry) = mirri_proto::rpc::get(method) else {
            return Err(RpcError::MethodNotFound(method.to_string()));
        };
        if !entry.availability.serves_server() {
            return Err(RpcError::MethodNotFound(method.to_string()));
        }

        self.dispatch_engine(method, params).await
    }

    /// Run an engine call on the seam, mapping the adapter's errors back
    /// onto the RPC error surface and tracking fire-mode turns in the shared
    /// in-flight map.
    async fn dispatch_engine(&self, method: &str, params: Value) -> Result<Value, RpcError> {
        // Fire-mode message.send: [`Self::prepare_fire_watch`] subscribes the
        // session's terminal turn event channel BEFORE the engine call
        // starts and hands the live receiver to the in-flight watcher, so
        // the watcher cannot miss the turn's terminal event (even for an
        // instant turn whose terminal event is published synchronously from
        // inside the engine call).
        let fire_watch = if method == "message.send"
            && params.get("mode").and_then(Value::as_str) == Some("fire")
        {
            self.prepare_fire_watch(&params).await
        } else {
            None
        };

        let (tx, rx) = tokio::sync::oneshot::channel();
        let engine = Arc::clone(&self.engine);
        let method = method.to_string();
        let call_params = params.clone();
        // Track the engine call so the graceful-shutdown drain can wait for
        // it (a TaskTracker-spawned task keeps running after the tracker is
        // closed — only `wait` blocks until every tracked task finishes).
        let handle = self
            .engine_call_tracker
            .spawn(async move {
                let outcome = engine.call_method(&method, call_params).await;
                let _ = tx.send(outcome);
            });

        let outcome = rx.await.map_err(|_| {
            // The oneshot sender is dropped without sending exactly when the
            // engine-call task was cancelled mid-call — a runtime shutdown
            // abort, or a panic inside the engine handler. Report that
            // honestly instead of a bare "vanished" internal error: the
            // client can tell a terminating server from a broken one.
            RpcError::internal(
                "engine call terminated without a response (the task was \
                 cancelled or panicked — the server may be shutting down)",
            )
        })?;

        match outcome {
            Ok(value) => {
                // The engine call ended; for a fire-mode send the entry it
                // tracked is the turn that continues in the engine. Derive
                // the `{session}:{agent}:{turn}` key from the engine's own
                // result (the same key format the in-flight map has always
                // used) and let the watcher reap it at the turn's end.
                if let Some(watch) = fire_watch
                    && let (Some(agent_id), Some(turn_id)) = (
                        value.get("agent_id").and_then(Value::as_str),
                        value.get("turn_id").and_then(Value::as_str),
                    )
                {
                    let key = format!("{}:{}:{}", watch.session_id, agent_id, turn_id);
                    self.in_flight_turns
                        .lock()
                        .await
                        .insert(key.clone(), handle);
                    crate::ws_rpc::spawn_in_flight_watcher(
                        watch.live,
                        turn_id.to_string(),
                        key,
                        Arc::clone(&self.in_flight_turns),
                    );
                }
                Ok(value)
            }
            Err(engine_error) => Err(RpcError::from(engine_error)),
        }
    }

    /// Subscribe to the addressed session's event channel for the in-flight
    /// watcher BEFORE the engine call starts, so the watcher's live receiver
    /// is already positioned when the terminal event is published (an
    /// instant fire-mode turn can publish it synchronously from inside
    /// `call_method`). Returns `None` when the session does not exist (the
    /// engine call will fail with not-found anyway) or the message is not a
    /// fire-mode send.
    async fn prepare_fire_watch(&self, params: &Value) -> Option<FireWatch> {
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .or_else(|| params.get("session_id").and_then(Value::as_str))?;
        // Route through the seam (W3): the addressed session's artifacts come
        // from its owning workspace's manager, and a persisted-only session
        // (a restart left it on disk) is resumed on demand before the watcher
        // subscribes — the fire-mode send's events reach the live receiver.
        let mut artifacts = self.session_routing.session_artifacts(session_id);
        if artifacts.is_none() && !self.session_routing.is_closed(session_id) {
            self.session_routing.restore_session(session_id).await;
            artifacts = self.session_routing.session_artifacts(session_id);
        }
        let artifacts = artifacts?;
        let channel = self.session_event_channels.get_or_create(
            session_id,
            Arc::clone(&artifacts.event_bus),
            self.server_id.clone().into(),
        );
        // The live subscribe happens here — before the engine call — and
        // the receiver travels with the watch into the watcher. `None`
        // cursor: the watcher only cares about events that arrive from now
        // on, and the pre-engine-call position is exactly what makes the
        // instant-turn guarantee hold.
        let (_, _, live) = channel.subscribe(None);
        Some(FireWatch {
            session_id: session_id.to_string(),
            live,
        })
    }
}

/// The pre-subscribed channel handle a fire-mode `message.send` engine call
/// is tracked against: the session id for the in-flight key plus the live
/// receiver already positioned (subscribed in [`ServerState::prepare_fire_watch`],
/// before the engine call) so the in-flight watcher observes the turn's
/// terminal event no matter how fast the turn completes.
struct FireWatch {
    session_id: String,
    live: broadcast::Receiver<Arc<crate::ws_rpc::SessionEventRecord>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use mirri_core::SessionEvent;
    use mirri_core::SessionId;
    use mirri_transport::adapter::EngineError;
    use mirri_transport::adapter::ReverseRpcHandler;
    use mirri_transport::frame::JsonRpcNotification;
    use serde_json::json;
    use tokio::sync::mpsc;

    /// A stub engine that answers `session.create` with a fixed payload and
    /// treats every other method as unknown — enough to exercise the
    /// dispatch gate without booting a real engine.
    struct StubEngine;

    #[async_trait::async_trait]
    impl EngineAdapter for StubEngine {
        async fn call_method(&self, method: &str, _params: Value) -> Result<Value, EngineError> {
            match method {
                "session.create" => Ok(json!({ "id": "session-stub", "title": "stub" })),
                "message.send" => Ok(json!({
                    "ok": true,
                    "session_id": "session-stub",
                    "agent_id": "main",
                    "turn_id": "turn-stub",
                })),
                other => Err(EngineError::MethodNotFound(other.to_string())),
            }
        }

        async fn subscribe_events(
            &self,
        ) -> Result<mpsc::Receiver<JsonRpcNotification>, EngineError> {
            let (_tx, rx) = mpsc::channel(8);
            Ok(rx)
        }

        async fn on_reverse_rpc(&self, _handler: ReverseRpcHandler) -> Result<(), EngineError> {
            Ok(())
        }

        async fn dispose(&self) -> Result<(), EngineError> {
            Ok(())
        }
    }

    fn test_state(engine: Arc<dyn EngineAdapter>) -> ServerState {
        let tmp = tempfile::tempdir().expect("tempdir must be creatable");
        let workspace = Arc::new(mirri_core::WorkspaceScope::new(
            mirri_core::WorkspaceId::new("state-test"),
            tmp.path().to_path_buf(),
            mirri_core::WorkspaceScopeConfig::default(),
        ));
        let manager = Arc::new(SessionManager::new(workspace));
        ServerState::with_engine(engine, manager, "test-version".to_string(), None)
    }

    #[test]
    fn should_carry_engine_version_and_auth_token_when_state_is_built_with_engine() {
        // Given a stub engine and a session manager on a throwaway workspace
        let tmp = tempfile::tempdir().expect("tempdir must be creatable");
        let workspace = Arc::new(mirri_core::WorkspaceScope::new(
            mirri_core::WorkspaceId::new("state-test"),
            tmp.path().to_path_buf(),
            mirri_core::WorkspaceScopeConfig::default(),
        ));
        let manager = Arc::new(SessionManager::new(workspace));
        let engine: Arc<dyn EngineAdapter> = Arc::new(StubEngine);

        // When the state is constructed with the engine and a token
        let state = ServerState::with_engine(
            engine,
            manager,
            "0.1.0-state".to_string(),
            Some("secret-token".to_string()),
        );

        // Then the identity, auth token and version ride on the state
        assert_eq!(state.auth_token.as_deref(), Some("secret-token"));
        assert_eq!(state.version, "0.1.0-state");
        assert!(!state.server_id.is_empty(), "server id is minted");
    }

    #[tokio::test]
    async fn should_dispatch_transport_registry_first_then_engine_fall_through_and_gate() {
        // Given a state whose engine answers session.create and nothing else
        let state = test_state(Arc::new(StubEngine));

        // When a transport-owned method is called, the registry answers —
        // a session.get on a missing session is a registry NotFound, never
        // an engine MethodNotFound.
        let error = state
            .dispatch("session.get", json!({ "sessionId": "no-such-session" }))
            .await
            .expect_err("session.get is transport-owned");
        assert!(matches!(error, RpcError::NotFound(_)));

        // When a serve-wire engine method is called, the engine answers
        let result = state
            .dispatch("session.create", json!({ "title": "engine-session" }))
            .await
            .expect("engine fall-through succeeds");
        assert_eq!(result["id"], json!("session-stub"));

        // When an embedded-only method is called, the registry gate blocks it
        let error = state
            .dispatch("cron.list", json!({}))
            .await
            .expect_err("embedded-only method is gated off the serve wire");
        assert!(matches!(error, RpcError::MethodNotFound(ref m) if m == "cron.list"));

        // When a never-registered method is called, it is method-not-found
        let error = state
            .dispatch("no.such.method", json!({}))
            .await
            .expect_err("unknown method is method-not-found");
        assert!(matches!(error, RpcError::MethodNotFound(ref m) if m == "no.such.method"));
    }

    /// A stub engine that answers `message.send` "instantly": the turn's
    /// terminal event is published on the session's event bus synchronously
    /// from inside `call_method` — before the fire-mode response returns.
    /// That is exactly the worst case for the in-flight watcher's
    /// subscription ordering: with the live subscribe happening after the
    /// engine call, the terminal event would already be gone (a broadcast
    /// with no receivers drops) and the watcher would wait forever.
    struct InstantTurnEngine {
        manager: Arc<SessionManager>,
    }

    #[async_trait::async_trait]
    impl EngineAdapter for InstantTurnEngine {
        async fn call_method(&self, method: &str, params: Value) -> Result<Value, EngineError> {
            if method == "message.send" {
                let session_id = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .or_else(|| params.get("session_id").and_then(Value::as_str))
                    .expect("message.send carries a session id")
                    .to_string();
                let artifacts = self
                    .manager
                    .session_artifacts(&SessionId::new(session_id.clone()))
                    .expect("the addressed session exists");
                artifacts.event_bus.publish(SessionEvent::TurnStarted {
                    agent_id: "main".to_string(),
                    session_id: session_id.clone(),
                    turn_id: "turn-instant".to_string(),
                });
                artifacts.event_bus.publish(SessionEvent::TurnCompleted {
                    agent_id: "main".to_string(),
                    session_id: session_id.clone(),
                    turn_id: "turn-instant".to_string(),
                    stop_reason: "end_turn".to_string(),
                    wire_turn_id: 0,
                    duration_ms: None,
                });
                return Ok(json!({
                    "ok": true,
                    "session_id": session_id,
                    "agent_id": "main",
                    "turn_id": "turn-instant",
                }));
            }
            Err(EngineError::MethodNotFound(method.to_string()))
        }

        async fn subscribe_events(
            &self,
        ) -> Result<mpsc::Receiver<JsonRpcNotification>, EngineError> {
            let (_tx, rx) = mpsc::channel(8);
            Ok(rx)
        }

        async fn on_reverse_rpc(&self, _handler: ReverseRpcHandler) -> Result<(), EngineError> {
            Ok(())
        }

        async fn dispose(&self) -> Result<(), EngineError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn should_reap_instant_fire_mode_turn_when_terminal_event_publishes_during_engine_call() {
        // Given a state whose engine completes a fire-mode turn synchronously
        // from inside call_method (the terminal event is on the bus before
        // the fire-mode response returns) and a live session to address
        let tmp = tempfile::tempdir().expect("tempdir must be creatable");
        let workspace = Arc::new(mirri_core::WorkspaceScope::new(
            mirri_core::WorkspaceId::new("state-instant"),
            tmp.path().to_path_buf(),
            mirri_core::WorkspaceScopeConfig::default(),
        ));
        let manager = Arc::new(SessionManager::new(workspace));
        let now = chrono::Utc::now();
        manager
            .create_session_with_id(
                SessionId::new("instant"),
                mirri_core::SessionMetadata {
                    title: "instant turn".to_string(),
                    created_at: now,
                    updated_at: now,
                    tags: Default::default(),
                    archived: false,
                    is_custom_title: false,
                    work_dir: None,
                    last_prompt: None,
                    forked_from: None,
                    cache_root_id: None,
                    custom: None,
                    last_used_at: None,
                },
                mirri_core::SessionScopeConfig::default(),
            )
            .await
            .expect("create session");
        let engine: Arc<dyn EngineAdapter> = Arc::new(InstantTurnEngine {
            manager: Arc::clone(&manager),
        });
        let state =
            ServerState::with_engine(Arc::clone(&engine), manager, "0.1.0-test".to_string(), None);

        // When a fire-mode message.send is dispatched
        let result = state
            .dispatch(
                "message.send",
                json!({ "sessionId": "instant", "mode": "fire", "text": "hi" }),
            )
            .await
            .expect("fire-mode send succeeds");
        assert_eq!(result["turn_id"], "turn-instant");

        // Then the in-flight entry the dispatch wrapper registered is reaped:
        // the pre-engine-call subscribe in prepare_fire_watch observed the
        // terminal event call_method published, so the watcher ends instead
        // of leaving a stale running entry behind.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if state.in_flight_turns.lock().await.is_empty() {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "in-flight entry was never reaped: the watcher missed the instant turn's terminal event"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

/// A stub engine whose `session.create` takes 300ms — slow enough to be
/// in flight when a shutdown drain starts.
struct SlowEngine;

#[async_trait::async_trait]
impl EngineAdapter for SlowEngine {
    async fn call_method(&self, method: &str, _params: Value) -> Result<Value, EngineError> {
        if method == "session.create" {
            tokio::time::sleep(Duration::from_millis(300)).await;
            return Ok(json!({ "id": "session-slow" }));
        }
        Err(EngineError::MethodNotFound(method.to_string()))
    }

    async fn subscribe_events(
        &self,
    ) -> Result<mpsc::Receiver<JsonRpcNotification>, EngineError> {
        let (_tx, rx) = mpsc::channel(8);
        Ok(rx)
    }

    async fn on_reverse_rpc(&self, _handler: ReverseRpcHandler) -> Result<(), EngineError> {
        Ok(())
    }

    async fn dispose(&self) -> Result<(), EngineError> {
        Ok(())
    }
}
/// Given an engine call in flight, When the shutdown drain runs, Then the
/// in-flight call completes and returns its result — the drain must let
/// tracked engine calls finish instead of abandoning them to a runtime
/// drop (the "engine call task vanished" incident, 2026-10-12).
#[tokio::test]
async fn should_complete_in_flight_engine_calls_when_shutdown_drain_runs() {
    // Given a state whose engine answers slowly
    let state = test_state(Arc::new(SlowEngine));

    // When an engine call is dispatched (still in flight) and the shutdown
    // drain runs concurrently — closing the tracker first, exactly like
    // `serve`'s post-shutdown path
    let dispatch = tokio::spawn({
        let state = state.clone();
        async move { state.dispatch("session.create", json!({})).await }
    });
    // Yield once so the engine call actually starts before the tracker
    // closes (the close only blocks NEW tracked tasks).
    tokio::time::sleep(Duration::from_millis(50)).await;
    state.drain_engine_calls(Duration::from_secs(5)).await;

    // Then the in-flight call completes with its real result, not an
    // internal "task vanished" error
    let result = dispatch.await.expect("dispatch task joins");
    let value = result.expect("the drained engine call completes successfully");
    assert_eq!(value["id"], json!("session-slow"));
}

/// Given an engine call that never completes, When the shutdown drain runs
/// with a short timeout, Then the drain returns at the timeout instead of
/// hanging the shutdown forever.
#[tokio::test]
async fn should_return_at_the_timeout_when_a_drained_engine_call_never_completes() {
    // Given a state whose engine hangs forever
    struct HangingEngine;
    #[async_trait::async_trait]
    impl EngineAdapter for HangingEngine {
        async fn call_method(&self, method: &str, _params: Value) -> Result<Value, EngineError> {
            if method == "session.create" {
                std::future::pending::<()>().await;
            }
            Err(EngineError::MethodNotFound(method.to_string()))
        }
        async fn subscribe_events(
            &self,
        ) -> Result<mpsc::Receiver<JsonRpcNotification>, EngineError> {
            let (_tx, rx) = mpsc::channel(8);
            Ok(rx)
        }
        async fn on_reverse_rpc(&self, _handler: ReverseRpcHandler) -> Result<(), EngineError> {
            Ok(())
        }
        async fn dispose(&self) -> Result<(), EngineError> {
            Ok(())
        }
    }
    let state = test_state(Arc::new(HangingEngine));

    // When the engine call is in flight and the drain runs with a short
    // timeout
    let dispatch = tokio::spawn({
        let state = state.clone();
        async move { state.dispatch("session.create", json!({})).await }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let started = std::time::Instant::now();
    state.drain_engine_calls(Duration::from_millis(200)).await;

    // Then the drain returns around the timeout rather than hanging
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the drain must be bounded by its timeout"
    );
    dispatch.abort();
}
}
