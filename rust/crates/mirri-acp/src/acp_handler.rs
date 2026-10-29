//! ACP protocol handler — maps ACP methods to the embedded engine.
//!
//! The handler drives an [`EngineAdapter`] (an
//! [`EmbeddedServer`](mirri_embedded::EmbeddedServer) in practice) that runs
//! the `mirri-core` engine in-process:
//!
//! - `initialize` — negotiate the ACP protocol version and capabilities
//! - `session/new` — create a session (`session.create`)
//! - `session/prompt` — send a prompt and wait for the turn (`message.send`,
//!   wait mode)
//! - `session/list` — list sessions (`session.list`)
//! - `session/load` — resume a closed session (`session.resume`) and
//!   replay its persisted history as `session/update` notifications
//!   (`session.getHistory`); `session/resume` resumes without the replay
//!   (v1's one differentiator between the two)
//! - `session/close` — close a session (`session.close`)
//! - `session/delete` — delete a session for real: the engine closes it
//!   (`archiveSession` — the same disk-facing teardown `session.close`
//!   uses, overlay included) and the host removes its persisted session
//!   directory from disk, so it disappears from `session/list` and can
//!   never be loaded again (the v1 `sessionCapabilities.delete` promise)
//! - `session/fork` — fork a session (`forkSession`)
//! - `session/cancel` — abort an in-flight turn (`message.abort`); ACP defines
//!   it as a notification, so the host transport never sends a response
//! - `$/cancel_request` — cancel the in-flight `session/prompt` carrying the
//!   named JSON-RPC request id (v1's standard request-cancellation channel);
//!   routes into the same cancel path as `session/cancel`, settling the
//!   prompt's response with the v1 `cancelled` stop reason and answering the
//!   session's pending `session/request_permission` asks with the `Cancelled`
//!   outcome
//! - `session/set_mode` — switch the session's ACP mode (`modeId`: default /
//!   plan / auto / yolo → the engine's plan + permission toggles), emitting
//!   a `current_mode_update` notification
//! - `session/set_config_option` — set a config option (`configId` + value:
//!   model → `setModel`, thinking → `setThinking`, mode → the mode toggles),
//!   responding with the refreshed `configOptions`
//! - `session/set_model` — set the session model directly (`setModel`)
//! - every other method fails with a well-formed JSON-RPC error
//!
//! Engine events are translated into ACP `session/update` notifications on a
//! broadcast channel (see [`AcpHandler::subscribe_updates`]); the host
//! transport (stdio binary, test harness) relays those to the client:
//! assistant text arrives as
//! `{"method": "session/update", "params": {"sessionId": …,
//! "update": {"sessionUpdate": "agent_message_chunk",
//! "content": {"type": "text", "text": …}}}}` — the official ACP v1
//! `SessionUpdate` vocabulary (the conformance suite validates every
//! emitted update against the vendored official schema snapshot).
//!
//! The engine's reverse RPC is bridged the other way: when the permission
//! gate parks a tool call on `approval.ask`, the handler (registered with
//! the engine through [`EngineAdapter::on_reverse_rpc`]) translates it into
//! a `session/request_permission` request and parks that on the
//! client-request channel (see [`AcpHandler::subscribe_client_requests`])
//! for the host transport to deliver and answer. Any delivery failure
//! (timeout, disconnect, malformed reply) resolves to a rejected approval
//! so the turn never hangs (TS `interaction-bridge.ts` parity).

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicBool, Ordering};

use mirri_transport::adapter::{EngineAdapter, EngineError};
use mirri_transport::frame::{JsonRpcNotification, RequestId};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc, oneshot};

use crate::mcp_servers::overlay_entries;
use crate::prompt_content::convert_prompt;
use crate::question;

/// The highest ACP protocol version this host implements.
const SUPPORTED_PROTOCOL_VERSION: i64 = 1;

/// Factory minting a fresh engine for a `session/new` / `session/load`
/// `cwd` — the seam of the **per-cwd engine pool**: same-cwd sessions
/// share the engine the pool caches under their (canonicalized) cwd, so
/// the engine's workspace-rooted FS / permission scope stays per-cwd
/// without ever making the engine's sessions cwd-aware (that would break
/// the workspace-level security model). The production serve loop binds
/// this to [`EmbeddedServer::for_workspace_root`](mirri_embedded::EmbeddedServer::for_workspace_root);
/// tests bind it to mock-LLM engines they can inspect.
pub type EngineFactory = Arc<dyn Fn(&Path) -> Arc<dyn EngineAdapter> + Send + Sync>;

/// Server error: a session method was called before `initialize`.
const ERROR_NOT_INITIALIZED: i64 = -32000;
/// Server error: the requested session does not exist or cannot be resumed.
const ERROR_SESSION_NOT_FOUND: i64 = -32002;
/// JSON-RPC error: the method is not known.
const ERROR_METHOD_NOT_FOUND: i64 = -32601;
/// JSON-RPC error: the method parameters are invalid.
const ERROR_INVALID_PARAMS: i64 = -32602;
/// JSON-RPC error: an internal error occurred while running the method.
const ERROR_INTERNAL: i64 = -32603;

/// Capacity of the `session/update` notification channel (P5e decision
/// B): 1024 slots cover long streaming turns (1000+ chunks) for the stdio
/// single-client topology, and a lagging consumer loses only UI chunks —
/// the durable session history lives in the wire log, and `session/load`
/// replays it. There is deliberately **no bounded blocking**: backpressure
/// on the update relay would cascade through the engine's event forwarder
/// and stall the whole turn for every slow client, which is strictly worse
/// than dropping display chunks. A lag is observable instead — the serve
/// loop warns (see `handle_update_relay_failure` in the binary).
const UPDATE_CHANNEL_CAPACITY: usize = 1024;

/// One request the engine's reverse-RPC bridge needs the ACP client to
/// answer — currently only `session/request_permission` (the bridge for the
/// engine's `approval.ask`).
///
/// The host transport (the stdio serve loop, or a test playing the client)
/// receives these through [`AcpHandler::subscribe_client_requests`] and
/// settles each one with [`PendingClientRequest::respond`]. Dropping the
/// request without responding models a client that went away — the awaiting
/// engine-side ask resolves to its rejected fallback.
#[derive(Debug)]
pub struct PendingClientRequest {
    /// The JSON-RPC method the client is asked to handle.
    pub method: String,
    /// The request parameters (e.g. the `RequestPermissionRequest` shape).
    pub params: Value,
    /// The one-shot channel that settles the engine-side ask.
    reply: Option<oneshot::Sender<Result<Value, String>>>,
}

impl PendingClientRequest {
    fn new(
        method: impl Into<String>,
        params: Value,
        reply: oneshot::Sender<Result<Value, String>>,
    ) -> Self {
        Self {
            method: method.into(),
            params,
            reply: Some(reply),
        }
    }

    /// Deliver the client's answer: `Ok(value)` is the successful
    /// JSON-RPC result, `Err(reason)` a failed / timed-out / disconnected
    /// request. Sending is infallible-by-design: a dropped receiver (the
    /// ask was resolved through another channel, e.g. the session's
    /// approval gate) silently discards the answer.
    pub fn respond(mut self, result: Result<Value, String>) {
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(result);
        }
    }
}

/// One in-flight `session/prompt`, registered under its client-facing
/// JSON-RPC request id — the `$/cancel_request` target namespace. The
/// engine that owns the session is captured at prompt start (the
/// per-cwd pool's session→engine routing), so a cancel never has to
/// re-resolve it.
struct InFlightPrompt {
    session_id: String,
    engine: Arc<dyn EngineAdapter>,
}

/// The per-session table of in-flight `session/request_permission` ask
/// cancel channels: session id → (bridge-minted ask seq → cancel sender).
type PendingPermissionAsks =
    Arc<std::sync::Mutex<HashMap<String, HashMap<u64, oneshot::Sender<()>>>>>;

/// A registered in-flight `session/request_permission` ask (F-P5b-4):
/// dropping the guard removes the registration when the ask settles
/// through any channel, so the per-session table never accumulates dead
/// entries.
struct PendingAskGuard {
    asks: PendingPermissionAsks,
    session_id: String,
    seq: u64,
}

impl Drop for PendingAskGuard {
    fn drop(&mut self) {
        let mut asks = self.asks.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(by_seq) = asks.get_mut(&self.session_id) {
            by_seq.remove(&self.seq);
            if by_seq.is_empty() {
                asks.remove(&self.session_id);
            }
        }
    }
}

/// ACP protocol handler that bridges ACP methods to the engine adapter.
pub struct AcpHandler {
    /// The engine every cwd-less request (and every session whose cwd is
    /// unknown) is dispatched to — the engine the serve loop rooted at
    /// its startup directory, or the test injection of [`Self::with_engine`].
    /// A **replaceable slot** (design 14 §2 / D3): a remote-mode
    /// connection death swaps in a new remote adapter (re-probe success)
    /// or the embedded engine (degrade), so the field is
    /// interior-mutable and every read goes through [`Self::current_engine`].
    engine: RwLock<Arc<dyn EngineAdapter>>,
    /// The per-cwd engine factory (the pool's mint). `None` degrades the
    /// handler to single-engine mode: a `cwd` is still validated, but
    /// every request runs on [`Self::engine`] (the `with_engine`
    /// injection contract — existing harnesses keep working).
    engine_factory: Option<EngineFactory>,
    /// The per-cwd engine pool: canonical cwd → engine. Same-cwd sessions
    /// share their engine (the pool is process-lifetime).
    cwd_engines: std::sync::Mutex<HashMap<PathBuf, Arc<dyn EngineAdapter>>>,
    /// The engine each session lives on: sessionId → engine. Recorded on
    /// `session/new` / `session/load` / `session/fork`; entries survive
    /// `session/close` (a later load routes back to the engine that owns
    /// the session's persistence) and are dropped on `session/delete`.
    session_engines: std::sync::Mutex<HashMap<String, Arc<dyn EngineAdapter>>>,
    /// The engines already wired (forwarder + reverse-RPC bridge), keyed
    /// by `Arc::as_ptr` — every engine of the pool needs its own wiring.
    wired_engines: std::sync::Mutex<HashSet<usize>>,
    /// Whether `initialize` completed successfully. All session methods
    /// require it (ACP §3.1).
    initialized: AtomicBool,
    /// Outbound `session/update` notifications, relayed to the host
    /// transport. The forwarder task is started lazily on the first session
    /// method (so constructing a handler outside a Tokio runtime is safe).
    updates: broadcast::Sender<JsonRpcNotification>,
    /// Outbound client requests (the reverse-RPC bridge's `session/
    /// request_permission` asks). Single-consumer: the host serve loop (or
    /// the test playing the client) takes the receiver once.
    client_requests: mpsc::UnboundedSender<PendingClientRequest>,
    /// The sole receiver for [`Self::client_requests`], handed to the first
    /// subscriber.
    client_request_receiver:
        std::sync::Mutex<Option<mpsc::UnboundedReceiver<PendingClientRequest>>>,
    /// Sessions with an in-flight `session/prompt` (most recent last) — the
    /// engine's `approval.ask` reverse RPC carries no session id, so the
    /// bridge addresses the permission request to the session whose prompt
    /// is running.
    active_prompt_sessions: Arc<std::sync::Mutex<Vec<String>>>,
    /// In-flight `session/prompt` requests, keyed by their client-facing
    /// JSON-RPC request id — what `$/cancel_request` names. This is a
    /// distinct namespace from the reverse-RPC bridge's pending
    /// client-request ids (those are the host's *outbound* request ids).
    in_flight_prompts: Arc<std::sync::Mutex<HashMap<RequestId, InFlightPrompt>>>,
    /// In-flight `session/request_permission` asks per session (keyed by a
    /// bridge-minted sequence): the cancel paths fire each ask's channel so
    /// the bridge resolves it with the v1 `Cancelled` outcome (F-P5b-4)
    /// instead of waiting for the client's never-arriving reply.
    pending_permission_asks: PendingPermissionAsks,
    /// The `clientCapabilities` the client advertised at `initialize`
    /// (G14 / F-P5b-1): `elicitation.form` decides whether a `question.
    /// ask` bridges to `elicitation/create` or degrades to the
    /// single-select permission surface. Shared with the reverse-RPC
    /// bridges (cloned into their closures).
    client_capabilities: Arc<std::sync::Mutex<Option<Value>>>,
    /// The working directory each pooled engine is rooted at (engine
    /// pointer → cwd): the per-session cwd `session/list` reports and the
    /// `<cwd>/.mirri/v3_sessions/<id>` directory `session/delete` removes.
    /// Recorded by the pool mint ([`Self::engine_for_cwd`]) and by the
    /// production serve loop for the default engine ([
    /// `Self::remember_engine_cwd`]); test-injected engines stay unknown
    /// (their sessions list with an empty cwd, the TS `cwd ?? ''` shape).
    engine_cwds: std::sync::Mutex<HashMap<usize, PathBuf>>,
    /// The engine-slot replacement machinery (design 14 D3): the
    /// re-probe path and the embedded fallback factory a connection
    /// death consults. `None` in embedded mode and in every legacy test
    /// injection (the slot then never replaces and transport errors map
    /// to plain error responses, the pre-design-14 behavior).
    /// Interior-mutable because [`Self::set_engine_replacement_path`]
    /// installs it after construction (the remote serve loop builds the
    /// handler first, the probe machinery second).
    engine_replacement: std::sync::Mutex<Option<EngineReplacement>>,
    /// The directory the ACP host process started in (design 14 D6): the
    /// root the remote-mode `session/list` pin translates into the
    /// workspace id it filters the server's answer down to (the IDE
    /// starts the host at its single project root — the list follows the
    /// host root, not any per-request cwd). `None` in embedded mode.
    host_start_root: Option<PathBuf>,
}

/// The replacement machinery of the handler's engine slot (design 14
/// §2 "引擎替换接缝" / D3): consulted on a transport-class engine error.
struct EngineReplacement {
    /// The re-probe path: one call resolves whether the desktop server
    /// is back (yielding a fresh engine to serve subsequent requests)
    /// or gone (`None` → degrade). Tests install a counting closure.
    probe_path: ProbePath,
    /// The embedded fallback factory: invoked at most once, when the
    /// probe fails — the engine the host degrades to.
    embedded_factory: EmbeddedEngineFactory,
    /// Whether the host already degraded to the embedded engine (D3:
    /// the degrade is one-way — once degraded, no probe ever runs
    /// again; the ACP process restart is the only way back).
    degraded: AtomicBool,
}

/// The re-probe path consulted once per connection death (the test seam:
/// production installs the desktop probe + version-gated connect, tests
/// install a counting closure).
pub(crate) type ProbePath = Arc<dyn Fn() -> Option<Arc<dyn EngineAdapter>> + Send + Sync>;

/// The embedded-engine factory the degrade path consults once.
pub(crate) type EmbeddedEngineFactory = Arc<dyn Fn() -> Arc<dyn EngineAdapter> + Send + Sync>;

impl AcpHandler {
    /// Create a handler backed by an embedded engine rooted at the current
    /// working directory, without an LLM provider factory (`session/prompt`
    /// reports that no provider is configured).
    pub fn new() -> Self {
        Self::with_engine(Arc::new(mirri_embedded::EmbeddedServer::for_cwd()))
    }

    /// Create a handler driving the given engine adapter.
    ///
    /// Tests inject an [`EmbeddedServer`](mirri_embedded::EmbeddedServer)
    /// built on a [`MockLlmFactory`](mirri_test_harness::MockLlmFactory) so
    /// prompts run against scripted LLM responses. No engine factory is
    /// installed: the handler runs in single-engine mode (a `cwd` is
    /// validated but everything dispatches to the given engine).
    pub fn with_engine(engine: Arc<dyn EngineAdapter>) -> Self {
        Self::with_engine_pool_inner(engine, None)
    }

    /// Create a handler with a **per-cwd engine pool**: `default` serves
    /// cwd-less requests and unknown sessions, while every request
    /// carrying a `cwd` routes to (or mints, through `factory`) the engine
    /// cached under that canonical cwd — same-cwd sessions share it. The
    /// production serve loop binds `factory` to an
    /// [`EmbeddedServer::for_workspace_root`](mirri_embedded::EmbeddedServer::for_workspace_root)
    /// constructor; tests bind it to mock-LLM engines they can inspect.
    pub fn with_engine_pool(default: Arc<dyn EngineAdapter>, factory: EngineFactory) -> Self {
        Self::with_engine_pool_inner(default, Some(factory))
    }

    /// The shared constructor of [`Self::with_engine`] and
    /// [`Self::with_engine_pool`].
    fn with_engine_pool_inner(
        engine: Arc<dyn EngineAdapter>,
        engine_factory: Option<EngineFactory>,
    ) -> Self {
        Self::with_engine_pool_and_updates(engine, engine_factory, UPDATE_CHANNEL_CAPACITY)
    }

    /// [`Self::with_engine_pool_inner`] with the update channel's capacity
    /// as a parameter — the seam tests use to exercise the lag path with a
    /// small channel instead of pushing a thousand notifications
    /// (production callers pass [`UPDATE_CHANNEL_CAPACITY`]).
    fn with_engine_pool_and_updates(
        engine: Arc<dyn EngineAdapter>,
        engine_factory: Option<EngineFactory>,
        update_capacity: usize,
    ) -> Self {
        let (updates, _) = broadcast::channel(update_capacity);
        let (client_requests, client_request_receiver) = mpsc::unbounded_channel();
        Self {
            engine: RwLock::new(engine),
            engine_factory,
            cwd_engines: std::sync::Mutex::new(HashMap::new()),
            session_engines: std::sync::Mutex::new(HashMap::new()),
            wired_engines: std::sync::Mutex::new(HashSet::new()),
            initialized: AtomicBool::new(false),
            updates,
            client_requests,
            client_request_receiver: std::sync::Mutex::new(Some(client_request_receiver)),
            active_prompt_sessions: Arc::new(std::sync::Mutex::new(Vec::new())),
            in_flight_prompts: Arc::new(std::sync::Mutex::new(HashMap::new())),
            pending_permission_asks: Arc::new(std::sync::Mutex::new(HashMap::new())),
            client_capabilities: Arc::new(std::sync::Mutex::new(None)),
            engine_cwds: std::sync::Mutex::new(HashMap::new()),
            engine_replacement: std::sync::Mutex::new(None),
            host_start_root: None,
        }
    }

    /// Create a handler driving a remote engine (design 14 D2/D6's remote
    /// mode): `session/new` / `session/load` translate their request cwd
    /// into the server's `workspaceId` before creating, and `session/list`
    /// filters the server's answer down to the workspace of
    /// `host_start_root` (the directory the ACP host process started in —
    /// D6's "ACP 宿主启动根"). The remote serve loop passes its startup
    /// workspace directory; tests pass an explicit root.
    pub fn with_remote_engine(engine: Arc<dyn EngineAdapter>, host_start_root: PathBuf) -> Self {
        let mut handler = Self::with_engine(engine);
        handler.host_start_root = Some(host_start_root);
        handler
    }

    /// Subscribe to the ACP `session/update` notifications this handler
    /// emits. The host transport relays each notification to the client.
    pub fn subscribe_updates(&self) -> broadcast::Receiver<JsonRpcNotification> {
        self.updates.subscribe()
    }

    /// Take the receiver for the client-bound requests the engine's
    /// reverse-RPC bridge emits (`session/request_permission`). The channel
    /// is single-consumer: the first caller gets the receiver — the stdio
    /// serve loop in production, the test harness when playing the client.
    ///
    /// # Panics
    ///
    /// Panics when a second subscriber asks for the already-taken
    /// receiver — a host either serves the client requests or does not.
    pub fn subscribe_client_requests(&self) -> mpsc::UnboundedReceiver<PendingClientRequest> {
        self.client_request_receiver
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .expect("client request receiver already subscribed (single consumer)")
    }

    /// Handle an ACP JSON-RPC method on behalf of one client request —
    /// [`Self::handle`] plus the request-scoped bookkeeping the
    /// `$/cancel_request` channel needs: an in-flight `session/prompt`
    /// registers under its client-facing request id for the turn's
    /// lifetime, so a `$/cancel_request` naming that id cancels exactly
    /// that turn (v1's standard request-cancellation channel — TS parity:
    /// the SDK aborts the request's signal, `server.ts` routes the abort
    /// into the same cancel path as `session/cancel`).
    pub async fn handle_request(
        &self,
        request_id: Option<RequestId>,
        method: &str,
        params: Value,
    ) -> Result<Value, (i64, String)> {
        if method == "session/prompt"
            && let (Some(id), Some(session_id)) =
                (request_id, params.get("sessionId").and_then(Value::as_str))
        {
            // Register synchronously, before the first await, so a
            // cancel that arrives the instant the prompt starts still
            // finds its target.
            let engine = self.engine_for_addressed_session(&params);
            self.in_flight_prompts
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(
                    id.clone(),
                    InFlightPrompt {
                        session_id: session_id.to_string(),
                        engine,
                    },
                );
            let result = self.handle(method, params).await;
            // Whatever the outcome — success, error, or an abort caused
            // by a cancel — the id's turn is over; the removal is
            // idempotent for cancels that already resolved it.
            self.in_flight_prompts
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            return result;
        }
        self.handle(method, params).await
    }

    /// Handle an ACP JSON-RPC method.
    ///
    /// Returns the result value or an error (code, message).
    ///
    /// Every session-addressed method routes to the engine that owns the
    /// addressed session (the per-cwd pool's session→engine map — recorded
    /// at `session/new` / `session/load` / `session/fork` time), falling
    /// back to the default engine for unknown sessions; each engine is
    /// wired (event forwarder + reverse-RPC bridge) on its first use.
    pub async fn handle(&self, method: &str, params: Value) -> Result<Value, (i64, String)> {
        match method {
            "initialize" => {
                // Record the client's capabilities (G14): the elicitation
                // surface the question bridge reads is advertised here.
                *self
                    .client_capabilities
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = params.get("clientCapabilities").cloned();
                self.initialized.store(true, Ordering::SeqCst);
                Ok(initialize_result(&params))
            }
            "session/new" => {
                self.require_initialized()?;
                let engine = self.engine_for_new_session(&params).await?;
                self.ensure_wired(&engine).await?;
                self.session_new(engine, params).await
            }
            "session/prompt" => {
                self.require_initialized()?;
                let engine = self.engine_for_addressed_session(&params);
                self.ensure_wired(&engine).await?;
                self.session_prompt(engine, params).await
            }
            "session/list" => {
                self.require_initialized()?;
                self.session_list(params).await
            }
            "session/load" => {
                self.require_initialized()?;
                let engine = self.engine_for_load(&params).await?;
                self.ensure_wired(&engine).await?;
                // `session/load` replays the resumed history as
                // `session/update` notifications before the response
                // settles (v1's one differentiator vs `session/resume`).
                self.session_load(engine, params, true).await
            }
            "session/resume" => {
                self.require_initialized()?;
                let engine = self.engine_for_load(&params).await?;
                self.ensure_wired(&engine).await?;
                self.session_load(engine, params, false).await
            }
            "session/close" => {
                self.require_initialized()?;
                let engine = self.engine_for_addressed_session(&params);
                self.ensure_wired(&engine).await?;
                self.session_close(engine, params).await
            }
            "session/delete" => {
                self.require_initialized()?;
                let engine = self.engine_for_addressed_session(&params);
                self.ensure_wired(&engine).await?;
                self.session_delete(engine, params).await
            }
            "session/fork" => {
                self.require_initialized()?;
                let engine = self.engine_for_addressed_session(&params);
                self.ensure_wired(&engine).await?;
                self.session_fork(engine, params).await
            }
            "session/cancel" => {
                self.require_initialized()?;
                let engine = self.engine_for_addressed_session(&params);
                self.ensure_wired(&engine).await?;
                self.session_cancel(engine, params).await
            }
            "$/cancel_request" => {
                // v1's standard request-cancellation channel — a JSON-RPC
                // notification with `{"requestId": n}` params (the SDK's
                // `cancelRequest` protocol method). Routes into the same
                // cancel path as the `session/cancel` notification,
                // addressed by the client-facing request id instead of a
                // session id. It needs no `initialize` handshake: with no
                // prompt ever started there is nothing to cancel, and the
                // idempotent no-op keeps the notification harmless.
                let request_id = params
                    .get("requestId")
                    .and_then(request_id_from_value)
                    .ok_or_else(|| {
                        (
                            ERROR_INVALID_PARAMS,
                            "invalid params: missing 'requestId' (string or \
                             number) in $/cancel_request"
                                .to_string(),
                        )
                    })?;
                self.cancel_prompt_by_request_id(request_id).await
            }
            "session/set_mode" => {
                self.require_initialized()?;
                let engine = self.engine_for_addressed_session(&params);
                self.ensure_wired(&engine).await?;
                self.session_set_mode(engine, params).await
            }
            "session/set_config_option" => {
                self.require_initialized()?;
                let engine = self.engine_for_addressed_session(&params);
                self.ensure_wired(&engine).await?;
                self.session_set_config_option(engine, params).await
            }
            "session/set_model" => {
                self.require_initialized()?;
                let engine = self.engine_for_addressed_session(&params);
                self.ensure_wired(&engine).await?;
                self.session_set_model(engine, params).await
            }
            // Recognized but not wired yet: fail cleanly so the state
            // machine can still reject them before `initialize`.
            "session/update" | "session/event" | "session/auth" | "session/setMetadata" => {
                self.require_initialized()?;
                Err((
                    ERROR_METHOD_NOT_FOUND,
                    format!("method not implemented yet: {method}"),
                ))
            }
            _ => Err((
                ERROR_METHOD_NOT_FOUND,
                format!("method not found: {method}"),
            )),
        }
    }

    // --- ACP method implementations ---------------------------------------

    /// `session/new` — create a session and return its id. The request's
    /// `cwd` selects (or mints) the per-cwd engine the session lives on;
    /// its `mcpServers` becomes the session's MCP overlay (the engine
    /// connects them in the background — this response never waits for
    /// them, and per-server failures never fail the request).
    async fn session_new(
        &self,
        engine: Arc<dyn EngineAdapter>,
        params: Value,
    ) -> Result<Value, (i64, String)> {
        let mut rpc_params = serde_json::Map::new();
        if let Some(title) = params.get("title").and_then(Value::as_str) {
            rpc_params.insert("title".to_string(), Value::String(title.to_string()));
        }
        // The session-scoped MCP overlay: the v1 `mcpServers` request
        // parameter converted to engine overlay entries (unsupported
        // transports already warn-dropped in the conversion). The
        // field-present semantics (F-P5c-2 / F-P5d-1): a request that
        // carries `mcpServers` — even one that converts to zero entries —
        // installs an (empty) overlay; an absent field installs none
        // (nothing to replace on a fresh session either way).
        if let Some(mcp_servers) = params.get("mcpServers").filter(|value| !value.is_null()) {
            let overlay = overlay_entries(Some(mcp_servers));
            rpc_params.insert("mcpServers".to_string(), Value::Array(overlay));
        }
        // `additionalDirectories` is read (F-P5c-1) and honestly
        // warn-dropped: the engine's workspace scope is single-rooted
        // (no additional-roots slot), so the field is *not* advertised in
        // `sessionCapabilities` and a request carrying it is served at
        // the `cwd`'s scope with a loud warning, never a silent no-op.
        warn_ignored_additional_directories("session/new", params.get("additionalDirectories"));
        // Remote mode (design 14 D2): the request cwd must name one of the
        // server's REGISTERED workspaces — the create is routed to it via
        // `workspaceId`. An unregistered cwd is rejected before the engine
        // call ever runs (no silent default-workspace fallback: a misrouted
        // sandbox is a file-writes-in-the-wrong-place bug). The embedded
        // path is unchanged — it binds the workspace itself.
        if engine.is_remote()
            && let Some(cwd) = params.get("cwd").and_then(Value::as_str)
        {
            let workspace_id = self
                .translate_cwd_to_workspace_id(&engine, Path::new(cwd))
                .await?;
            rpc_params.insert("workspaceId".to_string(), Value::String(workspace_id));
        }
        let (engine, result) = match engine
            .call_method("session.create", Value::Object(rpc_params.clone()))
            .await
        {
            Ok(result) => (engine, result),
            // A transport-class failure on the remote engine: run the
            // design-14 D3 re-probe / degrade chain before surfacing it
            // (on a re-probe success the create is retried on the fresh
            // engine and the session is remembered against IT; the
            // degrade surfaces the original error — that request
            // failed, the NEXT one lands on the embedded slot).
            Err(error) if is_transport_class(&error) => {
                match self
                    .handle_engine_call(error, |fresh| async move {
                        fresh
                            .call_method("session.create", Value::Object(rpc_params.clone()))
                            .await
                    })
                    .await
                {
                    Ok(result) => (self.current_engine(), result),
                    Err(error) => return Err(map_engine_error(error)),
                }
            }
            Err(error) => return Err(map_engine_error(error)),
        };
        let session_id = result.get("id").and_then(Value::as_str).ok_or_else(|| {
            (
                ERROR_INTERNAL,
                "session.create returned no session id".to_string(),
            )
        })?;
        // Record which engine owns the session so every later
        // session-addressed method routes to it.
        self.remember_session_engine(session_id, &engine);
        // The v1 response surface: the fresh session's mode picker and
        // config options (model / thinking / mode), read from the engine
        // the session lives on.
        let (modes, config_options) = self.session_mode_surface(&engine, session_id).await?;
        Ok(json!({
            "sessionId": session_id,
            "modes": modes,
            "configOptions": config_options,
        }))
    }

    /// `session/prompt` — run one engine turn against the addressed session
    /// and return its stop reason.
    async fn session_prompt(
        &self,
        engine: Arc<dyn EngineAdapter>,
        params: Value,
    ) -> Result<Value, (i64, String)> {
        let session_id = required_session_id(&params, "session/prompt")?;
        // The v1 content blocks (text / image / resource_link / resource)
        // converted onto the engine's input (see `prompt_content`): a
        // text-only prompt keeps the plain `text` spelling, anything else
        // goes as the canonical part list.
        let input = match convert_prompt(params.get("prompt").unwrap_or(&Value::Null))? {
            crate::prompt_content::EnginePromptInput::Text(text) => {
                json!({ "session_id": session_id, "text": text, "mode": "wait" })
            }
            crate::prompt_content::EnginePromptInput::Parts(parts) => {
                json!({ "session_id": session_id, "input": parts, "mode": "wait" })
            }
        };
        // The prompt's session is the one the reverse-RPC bridge addresses
        // `session/request_permission` asks to (the engine's `approval.ask`
        // payload carries no session id) — track it for the turn's lifetime.
        self.push_active_prompt_session(&session_id);
        // Wait mode: the turn runs to completion before the response, and the
        // assistant chunks stream out through `session/update` meanwhile.
        // A transport-class failure (a dead remote connection) runs the
        // design-14 D3 chain first: on a re-probe success the turn is
        // retried on the fresh engine, on a degrade the original error
        // surfaces (in-flight turns are never migrated — D3).
        let turn = match engine.call_method("message.send", input.clone()).await {
            Ok(turn) => Ok(turn),
            Err(error) if is_transport_class(&error) => {
                self.handle_engine_call(error, |fresh| async move {
                    fresh.call_method("message.send", input.clone()).await
                })
                .await
            }
            Err(error) => Err(error),
        };
        self.pop_active_prompt_session(&session_id);
        let result = turn.map_err(map_engine_error)?;
        let stop_reason = result
            .pointer("/turn/stopReason")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        Ok(json!({ "stopReason": acp_stop_reason(&stop_reason) }))
    }

    /// `session/list` — list every session the engines know, live **and
    /// persisted** (G9): the engine's `session.list` merges active
    /// (running) sessions with the persisted-only ones its session
    /// directory holds, and the ACP layer maps each onto the official v1
    /// `SessionInfo` (`sessionId` / `cwd` / `title` / `updatedAt`; the
    /// engine-side fields with no v1 slot — `createdAt`, `agentCount`,
    /// `tags`, `status` — ride under `_meta`, the reserved extensibility
    /// channel). A request `cwd` filters the way TS
    /// `filterSessionSummariesByCwd` does: only sessions of the engine
    /// rooted at that directory survive, sessions of an unknown-cwd
    /// engine (the test injection) stay visible — dropping them would
    /// make them unreachable from any listing.
    async fn session_list(&self, params: Value) -> Result<Value, (i64, String)> {
        let cwd_filter = params
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|cwd| !cwd.is_empty())
            .map(|cwd| {
                // Canonicalize the filter so symlinks and trailing
                // slashes do not hide the match (the pool keys engines
                // by their canonical cwd).
                std::fs::canonicalize(cwd).unwrap_or_else(|_| PathBuf::from(cwd))
            });
        let mut sessions: Vec<Value> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        // Remote mode (design 14 D6): the pin the loop filters each engine's
        // rows against — the host start root translated to its registered
        // workspace (see [`Self::remote_pinned_workspace`]). Computed once
        // up front so a translation failure logs (and degrades to "no
        // filter") exactly once, not per engine row.
        let pinned = self.remote_pinned_workspace().await;
        for engine in self.all_engines() {
            let engine_cwd = self.engine_cwd(&engine);
            if let Some(filter) = &cwd_filter {
                match &engine_cwd {
                    Some(engine_cwd) if engine_cwd != filter => continue,
                    // An engine whose cwd is unknown (test injection,
                    // remote engine) keeps its sessions visible — the TS
                    // `cwd === undefined` branch.
                    _ => {}
                }
            }
            let views = engine.call_method("session.list", json!({})).await;
            let views = match views {
                Ok(views) => views,
                // A transport-class failure on a remote engine: the D3
                // chain decides whether the slot still holds a remote
                // engine at all (a degrade lands here once; the degraded
                // slot's own listing runs in the next loop iteration's
                // `all_engines()` read — this request answers the
                // engines it could list).
                Err(error) if is_transport_class(&error) => {
                    match self
                        .handle_engine_call(error, |fresh| async move {
                            fresh.call_method("session.list", json!({})).await
                        })
                        .await
                    {
                        Ok(views) => views,
                        Err(error) => return Err(map_engine_error(error)),
                    }
                }
                Err(error) => return Err(map_engine_error(error)),
            };
            // W5: the engine's list answers the page envelope
            // (`{items, has_more}`) — unwrap the items (an older engine
            // answering the bare array still works through the same
            // fallback).
            let views = views["items"]
                .as_array()
                .cloned()
                .unwrap_or_else(|| views.as_array().cloned().unwrap_or_default());
            for view in views {
                let id = view.get("id").and_then(Value::as_str).map(str::to_string);
                let Some(id) = id else {
                    continue;
                };
                // Engines sharing a persistence root see each other's
                // sessions; the id dedup keeps the merged list honest.
                if !seen.insert(id.clone()) {
                    continue;
                }
                // The D6 read-side pin (remote mode): a row whose own root
                // (`cwd` — the session's workspace root, W4) is not the
                // pinned workspace's registered root is not this host's
                // project's session. A row with no cwd stays visible (the
                // engine-side `cwd ?? ''` branch — dropping it would make
                // it unreachable from any listing).
                if let Some((pinned_root, _)) = &pinned
                    && let Some(view_cwd) = view.get("cwd").and_then(Value::as_str)
                    && mirri_core::session::lexically_resolve(Path::new(view_cwd)) != *pinned_root
                {
                    continue;
                }
                sessions.push(session_info(&view, engine_cwd.as_deref()));
            }
        }
        Ok(json!({"sessions": sessions, "nextCursor": null}))
    }

    /// The workspace the remote-mode `session/list` pins to (design 14
    /// D6): the host start root translated through the current engine
    /// slot's `workspace.list` — returning the registered root (the row
    /// `cwd` match key, both lexically resolved) alongside the id.
    /// `None` when the handler is not in remote mode, or when the host
    /// root is not a registered workspace (the caller's fallback is the
    /// unfiltered list — logged here: the IDE still deserves a list, not
    /// an error response).
    async fn remote_pinned_workspace(&self) -> Option<(PathBuf, String)> {
        let engine = self.current_engine();
        if !engine.is_remote() {
            return None;
        }
        let root = self.host_start_root.as_ref()?.clone();
        match self.translate_cwd_to_workspace_id(&engine, &root).await {
            Ok(workspace_id) => {
                // The root of the matching registration — resolved by the
                // same lexical form the translation matched on, so a
                // differently-spelled registration still matches the rows.
                let listing = engine.call_method("workspace.list", json!({})).await.ok()?;
                let rows = listing["items"]
                    .as_array()
                    .cloned()
                    .unwrap_or_else(|| listing.as_array().cloned().unwrap_or_default());
                rows.iter()
                    .find(|row| row.get("id").and_then(Value::as_str) == Some(&workspace_id))
                    .and_then(|row| row.get("root").and_then(Value::as_str))
                    .map(|registered| {
                        (
                            mirri_core::session::lexically_resolve(Path::new(registered)),
                            workspace_id,
                        )
                    })
            }
            Err((code, message)) => {
                tracing::warn!(
                    root = %root.display(),
                    "session/list pinning skipped (design 14 D6): the host start root did not \
                     translate to a registered workspace — returning the unfiltered list; \
                     {message} (code {code})"
                );
                None
            }
        }
    }

    /// `session/load` / `session/resume` — resume a closed session. The
    /// request's `mcpServers` re-registers the session's MCP overlay (the
    /// v1 contract: the overlay is never persisted, the client re-sends
    /// the list on every load); its `cwd` routes an unknown session to
    /// the engine of that cwd.
    ///
    /// `session/load` additionally replays the resumed session's history
    /// as an ordered batch of `session/update` notifications (the v1
    /// differentiator vs `session/resume`, which deliberately skips the
    /// replay): each persisted message is projected onto the official v1
    /// `SessionUpdate` vocabulary — user/assistant text →
    /// `user_message_chunk` / `agent_message_chunk`, thinking →
    /// `agent_thought_chunk`, tool calls → `tool_call` + terminal
    /// `tool_call_update` — before the response settles with the mode and
    /// config-option surface.
    async fn session_load(
        &self,
        engine: Arc<dyn EngineAdapter>,
        params: Value,
        replay_history: bool,
    ) -> Result<Value, (i64, String)> {
        let session_id = required_session_id(&params, "session/load")?;
        let mut rpc_params = serde_json::Map::new();
        rpc_params.insert("session_id".to_string(), Value::String(session_id.clone()));
        // The overlay re-registration follows the same field-present
        // semantics as `session/new` (F-P5d-1): an explicit `mcpServers`
        // list always replaces — an empty (or fully warn-dropped) list
        // installs an empty overlay, clearing whatever the session had in
        // this process; an absent field keeps the session's existing
        // overlay (TS loadSession always re-registers the list it was
        // given, and a v1 client re-sends the list on every load).
        if let Some(mcp_servers) = params.get("mcpServers").filter(|value| !value.is_null()) {
            let overlay = overlay_entries(Some(mcp_servers));
            rpc_params.insert("mcpServers".to_string(), Value::Array(overlay));
        }
        warn_ignored_additional_directories("session/load", params.get("additionalDirectories"));
        // Remote mode (design 14 D2): a session unknown to this process is
        // claimed by the cwd it is loaded with — the resume is routed to
        // that cwd's registered workspace via `workspaceId`, exactly like
        // `session/new`'s create (a session of a registered root must land
        // in that root's sandbox, never the server default). An unregistered
        // cwd is rejected before the resume call runs.
        if engine.is_remote()
            && let Some(cwd) = params.get("cwd").and_then(Value::as_str)
        {
            let workspace_id = self
                .translate_cwd_to_workspace_id(&engine, Path::new(cwd))
                .await?;
            rpc_params.insert("workspaceId".to_string(), Value::String(workspace_id));
        }
        let (engine, result) = match engine
            .call_method("session.resume", Value::Object(rpc_params.clone()))
            .await
        {
            Ok(result) => (engine, result),
            // Remote mode (design 14 D5): the adapter's touch-to-subscribe
            // fired BEFORE this resume and the server's subscribe handler
            // already restored the persisted session on demand — the
            // engine's live-session guard then answers
            // `session.already_exists` for the resume itself. The load's
            // contract is "the session is loaded and observable", which
            // the restore already achieved: fall back to `session.reload`
            // (the engine's live fast path — same summary shape) instead
            // of failing the load. Embedded mode never hits this path (no
            // touch-to-subscribe runs ahead of an embedded resume).
            Err(EngineError::Rpc { code, ref data, .. })
                if code == -32603
                    && data
                        .as_ref()
                        .and_then(|data| data.get("domainCode"))
                        .and_then(Value::as_str)
                        == Some("session.already_exists") =>
            {
                let reload_params = json!({
                    "session_id": session_id,
                });
                match engine.call_method("session.reload", reload_params).await {
                    Ok(result) => (engine, result),
                    Err(error) if is_transport_class(&error) => {
                        match self
                            .handle_engine_call(error, |fresh| async move {
                                fresh
                                    .call_method(
                                        "session.resume",
                                        Value::Object(rpc_params.clone()),
                                    )
                                    .await
                            })
                            .await
                        {
                            Ok(result) => (self.current_engine(), result),
                            Err(error) => return Err(map_engine_error(error)),
                        }
                    }
                    Err(error) => return Err(map_engine_error(error)),
                }
            }
            // A transport-class failure on the remote engine: the D3
            // re-probe / degrade chain before surfacing it (a re-probe
            // success retries the resume on the fresh engine and routes
            // the session to it; the degrade surfaces the original
            // error, the NEXT request lands on the embedded slot).
            Err(error) if is_transport_class(&error) => {
                match self
                    .handle_engine_call(error, |fresh| async move {
                        fresh
                            .call_method("session.resume", Value::Object(rpc_params.clone()))
                            .await
                    })
                    .await
                {
                    Ok(result) => (self.current_engine(), result),
                    Err(error) => return Err(map_engine_error(error)),
                }
            }
            Err(error) => return Err(map_engine_error(error)),
        };
        let session_id = result.get("id").and_then(Value::as_str).ok_or_else(|| {
            (
                ERROR_INTERNAL,
                "session.resume returned no session id".to_string(),
            )
        })?;
        self.remember_session_engine(session_id, &engine);
        if replay_history {
            self.replay_history(&engine, session_id).await;
        }
        let (modes, config_options) = self.session_mode_surface(&engine, session_id).await?;
        Ok(json!({
            "sessionId": session_id,
            "modes": modes,
            "configOptions": config_options,
        }))
    }

    /// Replay the resumed session's persisted history as an ordered batch
    /// of `session/update` notifications. A history read failure degrades
    /// to a skipped replay (the load still succeeds — TS `replayHistory`
    /// parity: one broken session must never become an unloadable one).
    async fn replay_history(&self, engine: &Arc<dyn EngineAdapter>, session_id: &str) {
        let history = match engine
            .call_method("session.getHistory", json!({ "session_id": session_id }))
            .await
        {
            Ok(Value::Array(history)) => history,
            Ok(other) => {
                tracing::warn!(
                    session_id,
                    shape = %other,
                    "session/load history read returned an unexpected shape; replay skipped"
                );
                Vec::new()
            }
            Err(error) => {
                tracing::warn!(
                    session_id,
                    error = %error,
                    "session/load could not read the session history; replay skipped"
                );
                return;
            }
        };
        for update in project_history_to_updates(&history) {
            // No listeners is not an error: the update is dropped.
            let _ = self.updates.send(JsonRpcNotification {
                method: "session/update".to_string(),
                params: json!({
                    "sessionId": session_id,
                    "update": update,
                }),
            });
        }
    }

    /// Read the session's engine state and assemble the v1 response
    /// surface shared by `session/new`, `session/load`, and
    /// `session/resume`: the first-class `modes`
    /// (`SessionModeState{currentModeId, availableModes}`) and the
    /// `configOptions` list.
    ///
    /// `currentModeId` is derived from the engine's own state — plan mode
    /// active → `plan`, otherwise the agent's permission mode
    /// (`manual` → `default`, `auto`/`yolo` verbatim) — so a resumed
    /// session reports the mode its wire log replays, not a host-side
    /// guess. The config options are built from `agent.getConfig` (the
    /// model / thinking / permission values the engine reports).
    async fn session_mode_surface(
        &self,
        engine: &Arc<dyn EngineAdapter>,
        session_id: &str,
    ) -> Result<(Value, Value), (i64, String)> {
        // The mode surface's three facts (model / thinking / permission
        // mode), assembled engine-shape-agnostically: the embedded engine
        // answers the one-shot `agent.getConfig`, but that method is
        // retired on the serve wire (design 14's remote mode surfaces
        // this — the registry's fact table marks it embedded-only), so a
        // remote engine answers MethodNotFound and the same facts are
        // re-assembled from the serve wire's read faces: `agent.getModel`
        // + `agent.getEffectiveThinkingLevel` + `session.status`'s
        // `permission` field (all both/server-current). The embedded
        // path is byte-identical to before — the fallback only fires on
        // the remote shape. Note these read faces route through the
        // engine's restore-on-demand, so touching a remote persisted
        // session's mode surface revives it server-side — a touch is a
        // touch (design 14 D5's boundary), the second wire-side revival
        // path alongside touch-to-subscribe.
        let (permission_mode, model, thinking) = match engine
            .call_method("agent.getConfig", json!({ "session_id": session_id }))
            .await
        {
            Ok(config) => {
                let permission_mode = config
                    .get("permissionMode")
                    .and_then(Value::as_str)
                    .unwrap_or("manual")
                    .to_string();
                let model = config
                    .get("model")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let thinking = config
                    .get("thinkingEffort")
                    .and_then(Value::as_str)
                    .unwrap_or("none")
                    .to_string();
                (permission_mode, model, thinking)
            }
            Err(EngineError::MethodNotFound(_)) => {
                let model = engine
                    .call_method("agent.getModel", json!({ "session_id": session_id }))
                    .await
                    .map_err(map_engine_error)?
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let thinking = engine
                    .call_method(
                        "agent.getEffectiveThinkingLevel",
                        json!({ "session_id": session_id }),
                    )
                    .await
                    .map_err(map_engine_error)?
                    .as_str()
                    .unwrap_or("none")
                    .to_string();
                let status = engine
                    .call_method("session.status", json!({ "session_id": session_id }))
                    .await
                    .map_err(map_engine_error)?;
                let permission_mode = status
                    .get("permission")
                    .and_then(Value::as_str)
                    .unwrap_or("manual")
                    .to_string();
                (permission_mode, model, thinking)
            }
            Err(error) => return Err(map_engine_error(error)),
        };
        let plan = engine
            .call_method("plan.status", json!({ "session_id": session_id }))
            .await
            .map_err(map_engine_error)?;
        let plan_active = plan.get("active").and_then(Value::as_bool).unwrap_or(false);
        let current_mode = if plan_active {
            "plan"
        } else {
            match permission_mode.as_str() {
                "auto" => "auto",
                "yolo" => "yolo",
                _ => "default",
            }
        };
        // The engine's config wire carries the declared model catalog, so
        // the current model's `supportEfforts` opens the thinking select's
        // vocabulary: a model-declared custom spelling (`max`) is offered
        // to the host, not just tolerated as a currentValue. The same
        // catalog populates the model select — every configured alias is
        // pickable in the host UI, not just the active one.
        let engine_config = engine
            .call_method("config.get", json!({}))
            .await
            .map_err(map_engine_error)?;
        let declared_efforts: Vec<String> = engine_config
            .get("models")
            .and_then(|models| models.get(model.as_str()))
            .and_then(|alias| alias.get("supportEfforts"))
            .and_then(Value::as_array)
            .map(|efforts| {
                efforts
                    .iter()
                    .filter_map(|effort| effort.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let modes = json!({
            "currentModeId": current_mode,
            "availableModes": acp_available_modes(),
        });
        let config_options = json!([
            {
                "type": "select",
                "id": "model",
                "name": "Model",
                "category": "model",
                "currentValue": model,
                "options": acp_model_select_options(
                    engine_config.get("models").unwrap_or(&Value::Null),
                    &model,
                ),
            },
            {
                "type": "select",
                "id": "thinking",
                "name": "Thinking",
                "category": "thought_level",
                "currentValue": thinking,
                "options": acp_thinking_select_options(&declared_efforts),
            },
            {
                "type": "select",
                "id": "mode",
                "name": "Mode",
                "category": "mode",
                "currentValue": current_mode,
                "options": acp_mode_select_options(),
            },
        ]);
        Ok((modes, config_options))
    }

    /// Apply an ACP mode to the session's engine toggles (plan mode +
    /// permission mode — TS `acpModeToToggles` parity) and emit the
    /// `current_mode_update` notification.
    async fn apply_mode(
        &self,
        engine: &Arc<dyn EngineAdapter>,
        session_id: &str,
        mode_id: &str,
    ) -> Result<(), (i64, String)> {
        let want_plan = mode_id == "plan";
        // Only flip the plan toggle when it actually changes: entering an
        // already-active plan mode is an engine error, not a no-op.
        let plan = engine
            .call_method("plan.status", json!({ "session_id": session_id }))
            .await
            .map_err(map_engine_error)?;
        let plan_active = plan.get("active").and_then(Value::as_bool).unwrap_or(false);
        if want_plan != plan_active {
            let method = if want_plan { "enterPlan" } else { "cancelPlan" };
            engine
                .call_method(method, json!({ "session_id": session_id }))
                .await
                .map_err(map_engine_error)?;
        }
        engine
            .call_method(
                "setPermission",
                json!({
                    "session_id": session_id,
                    "mode": permission_for_mode_id(mode_id),
                }),
            )
            .await
            .map_err(map_engine_error)?;
        // The mode change is observable both ways: `current_mode_update`
        // serves clients reading the first-class `modes` state (the
        // refreshed `config_option_update` is the caller's business).
        let _ = self.updates.send(JsonRpcNotification {
            method: "session/update".to_string(),
            params: json!({
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": "current_mode_update",
                    "currentModeId": mode_id,
                },
            }),
        });
        Ok(())
    }

    /// `session/close` — close a session, freeing its resources. Persisted
    /// files stay on disk for a future `session/load` / `session/resume`;
    /// the session's MCP overlay is torn down with it.
    ///
    /// In remote mode (design 14 §D5: 会话关闭即退订), the host first
    /// releases its subscription on the engine — the close must not land
    /// on a session whose server-side routing slot the connection still
    /// holds. The unsubscribe is best-effort: its failure is warn-logged
    /// and the close proceeds (a failed unsubscribe must not keep a dead
    /// session subscribed from the IDE's point of view).
    async fn session_close(
        &self,
        engine: Arc<dyn EngineAdapter>,
        params: Value,
    ) -> Result<Value, (i64, String)> {
        let session_id = required_session_id(&params, "session/close")?;
        if engine.is_remote()
            && let Err(error) = engine.unsubscribe_session(&session_id).await
        {
            tracing::warn!(
                session_id,
                %error,
                "session/close: unsubscribing from the remote engine failed; closing anyway"
            );
        }
        engine
            .call_method("session.close", json!({ "session_id": session_id }))
            .await
            .map_err(map_engine_error)?;
        Ok(json!({}))
    }

    /// `session/delete` — delete a session for real (the v1
    /// `sessionCapabilities.delete` promise, G12): the engine closes it
    /// through `archiveSession` (the same disk-facing teardown a close
    /// does — activity views disposed, MCP overlay torn down), and the
    /// host then removes the session's persisted directory from disk, so
    /// it disappears from `session/list` and a later `session/load` /
    /// `session/resume` answers not found. The session's routing entry
    /// leaves with it.
    ///
    /// The on-disk location is the engine's deterministic layout
    /// `<engine cwd>/.mirri/v3_sessions/<session_id>/` (what
    /// `SessionManager::new` roots itself at — the rust engine's own
    /// storage root, ADR-006-R2); an engine whose cwd the
    /// host does not know (a remote engine — its files live server-side)
    /// is warn-dropped from the file removal, never a silent fake
    /// success.
    async fn session_delete(
        &self,
        engine: Arc<dyn EngineAdapter>,
        params: Value,
    ) -> Result<Value, (i64, String)> {
        let session_id = required_session_id(&params, "session/delete")?;
        engine
            .call_method("archiveSession", json!({ "session_id": session_id }))
            .await
            .map_err(map_engine_error)?;
        self.forget_session_engine(&session_id);

        // The unified delete executor through the ENGINE (M4 / SDR-004):
        // `retention.delete` is the converged single body — directory
        // lock, in-lock revalidation, blob-ledger decrement (the shared
        // home's ledger — the anchor stamps THIS engine's periodic
        // recheck later judges), directory removal, index row, and the
        // busy/AlreadyGone business semantics. The ACP host performing
        // its own `remove_dir_all` would bypass every step: no lock (a
        // concurrent restore/compress could race the removal), no ledger
        // decrement (the attachments' zero anchors would never be
        // stamped in the shared home), no idempotent AlreadyGone.
        //
        // A `retention.delete` failure that is NOT "unknown method"
        // (an engine predating the route) falls back to the legacy
        // local removal below, preserving the old behavior instead of
        // failing the delete outright.
        let mut local_fallback: Option<std::path::PathBuf> = None;
        match engine
            .call_method("retention.delete", json!({ "session_id": session_id }))
            .await
        {
            Ok(_result) => return Ok(json!({})),
            Err(error) => {
                // Only an engine WITHOUT the route (MethodNotFound) takes
                // the legacy degrade; any other failure (a busy lock, an
                // unreadable stream, an IO error) is a real business
                // failure of THIS delete — surface it, never "ok".
                if !matches!(error, EngineError::MethodNotFound(_)) {
                    return Err(map_engine_error(error));
                }
                tracing::warn!(
                    session_id,
                    "session/delete: the engine does not implement the unified \
                     retention.delete route; falling back to the legacy local removal"
                );
                // Materialize the local fallback only in the degrade path.
                if let Some(cwd) = self.engine_cwd(&engine) {
                    let sessions_root = cwd.join(".mirri").join("v3_sessions");
                    let mut session_dir = sessions_root.join(&session_id);
                    if !session_dir.exists()
                        && let Ok(entries) = std::fs::read_dir(&sessions_root)
                    {
                        for entry in entries.flatten() {
                            let candidate = entry.path().join(&session_id);
                            if candidate.is_dir() {
                                session_dir = candidate;
                                break;
                            }
                        }
                    }
                    local_fallback = Some(session_dir);
                }
            }
        }

        // The LEGACY local removal (pre-retention engines only): no
        // lock, no ledger — the best the ACP host can do without the
        // engine's route. The blob-reference extraction still runs
        // (warn-logged on failure) so the shared-home aging recheck's
        // next full rebuild sees the session's references.
        match local_fallback {
            Some(session_dir) => {
                // Retention attachment accounting (best-effort, the
                // degrade path): the session's blob references are
                // extracted from the record streams BEFORE the directory
                // removal (they must still exist) and reported — the
                // shared-home engine's next ledger rebuild counts from
                // disk, so an extraction failure here is a WARN, never a
                // silent drop (M4: the old `if let Ok` swallowed it).
                match mirri_core::blob_ledger::extract_blob_refs(&session_dir) {
                    Ok(references) if !references.is_empty() => {
                        tracing::info!(
                            session_id,
                            attachments = references.len(),
                            "session/delete (legacy path): the session's blob \
                             references were extracted and reported; the shared-home \
                             aging recheck reclaims the exclusive ones"
                        );
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::warn!(
                            session_id,
                            %error,
                            "session/delete (legacy path): the blob-reference \
                             extraction failed; the attachments stay (the one-way \
                             boundary's allowed side)"
                        );
                    }
                }
                match std::fs::remove_dir_all(&session_dir) {
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => {
                        return Err((
                            ERROR_INTERNAL,
                            format!(
                                "failed to delete the session files ({}): {error}",
                                session_dir.display()
                            ),
                        ));
                    }
                }
            }
            None => {
                tracing::warn!(
                    session_id,
                    "session/delete could not locate the engine's sessions root; \
                     the persisted files were left in place"
                );
            }
        }
        Ok(json!({}))
    }

    /// `session/fork` — fork a session's agent into a new session (on the
    /// same engine as the source session). `session/fork` is an UNSTABLE
    /// v1 method: the capability is advertised under the initialize
    /// response's `_meta` (see [`initialize_result`]), and the response
    /// surface is the forked session's id.
    async fn session_fork(
        &self,
        engine: Arc<dyn EngineAdapter>,
        params: Value,
    ) -> Result<Value, (i64, String)> {
        let session_id = required_session_id(&params, "session/fork")?;
        warn_ignored_additional_directories("session/fork", params.get("additionalDirectories"));
        let result = engine
            .call_method("forkSession", json!({ "session_id": session_id }))
            .await
            .map_err(map_engine_error)?;
        let new_session_id = result.get("id").and_then(Value::as_str).ok_or_else(|| {
            (
                ERROR_INTERNAL,
                "forkSession returned no session id".to_string(),
            )
        })?;
        self.remember_session_engine(new_session_id, &engine);
        Ok(json!({ "sessionId": new_session_id }))
    }

    /// `session/cancel` — cancel an in-flight turn of a session. ACP defines
    /// this method as a notification: the host transport processes it and
    /// never sends a response (the empty result is discarded there).
    async fn session_cancel(
        &self,
        engine: Arc<dyn EngineAdapter>,
        params: Value,
    ) -> Result<Value, (i64, String)> {
        let session_id = required_session_id(&params, "session/cancel")?;
        self.cancel_session_turn(&engine, &session_id).await?;
        Ok(json!({}))
    }

    /// Cancel the in-flight `session/prompt` carrying `request_id` (the
    /// `$/cancel_request` path): the same engine-cancel walk as
    /// [`Self::session_cancel`], addressed by the request id. Unknown,
    /// settled, or already-cancelled ids are idempotent no-ops — a cancel
    /// that lost the race with the response must never error.
    async fn cancel_prompt_by_request_id(
        &self,
        request_id: RequestId,
    ) -> Result<Value, (i64, String)> {
        let target = {
            let prompts = self
                .in_flight_prompts
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            prompts
                .get(&request_id)
                .map(|prompt| (prompt.session_id.clone(), Arc::clone(&prompt.engine)))
        };
        let Some((session_id, engine)) = target else {
            return Ok(json!({}));
        };
        self.cancel_session_turn(&engine, &session_id).await?;
        Ok(json!({}))
    }

    /// The shared cancel path of `session/cancel` and `$/cancel_request`:
    /// abort the session's in-flight engine turns first — the cooperative
    /// cancel token ends a wait-mode prompt turn, which then settles its
    /// `session/prompt` response with `stopReason: cancelled` — and settle
    /// the session's in-flight `session/request_permission` asks with the
    /// v1 `Cancelled` outcome (F-P5b-4: the host answers on the cancelling
    /// client's behalf, exactly like a v1 client that MUST respond to its
    /// pending permission requests with the Cancelled outcome), so
    /// neither the engine-side ask nor the client's UI waits out the
    /// bridge's 60 s timeout.
    async fn cancel_session_turn(
        &self,
        engine: &Arc<dyn EngineAdapter>,
        session_id: &str,
    ) -> Result<(), (i64, String)> {
        let abort = engine
            .call_method("message.abort", json!({ "session_id": session_id }))
            .await;
        // The asks settle whether or not the abort was acknowledged.
        self.cancel_pending_permission_asks(session_id);
        abort.map_err(map_engine_error)?;
        Ok(())
    }

    /// Fire every registered permission-ask channel of the session: each
    /// in-flight `session/request_permission` resolves as if the client
    /// had answered with the v1 `Cancelled` outcome (F-P5b-4). A reply that
    /// arrives later through the real client is discarded by design —
    /// the one-shot reply channel is already settled.
    fn cancel_pending_permission_asks(&self, session_id: &str) {
        if let Some(asks) = self
            .pending_permission_asks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id)
        {
            for (_, cancel_tx) in asks {
                let _ = cancel_tx.send(());
            }
        }
    }

    /// `session/set_mode` — switch the session's ACP mode (v1: the param
    /// is `modeId`, one of the four locked modes `default` / `plan` /
    /// `auto` / `yolo`). The mode maps onto the engine's two underlying
    /// toggles (plan mode + permission mode); the client observes the
    /// change through a `current_mode_update` notification. The pre-v1
    /// `mode` parameter spelling is rejected, not shimmed.
    async fn session_set_mode(
        &self,
        engine: Arc<dyn EngineAdapter>,
        params: Value,
    ) -> Result<Value, (i64, String)> {
        let session_id = required_session_id(&params, "session/set_mode")?;
        let mode_id = params
            .get("modeId")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                (
                    ERROR_INVALID_PARAMS,
                    "invalid params: missing 'modeId' in session/set_mode".to_string(),
                )
            })?;
        if !is_acp_mode_id(mode_id) {
            return Err((
                ERROR_INVALID_PARAMS,
                format!(
                    "invalid params: unknown modeId '{mode_id}' in session/set_mode; \
                     expected one of default, plan, auto, yolo"
                ),
            ));
        }
        self.apply_mode(&engine, &session_id, mode_id).await?;
        Ok(json!({}))
    }

    /// `session/set_config_option` — set a named session config option
    /// (v1: the params are `configId` + `value`). The `model` arm sets the
    /// engine model, the `thinking` arm the thinking effort, and the
    /// `mode` arm the ACP mode (same toggles as `session/set_mode`). Every
    /// option takes effect immediately — the response carries the
    /// refreshed `configOptions` and a `config_option_update` notification
    /// pushes the same surface. The pre-v1 `name` parameter spelling is
    /// rejected, not shimmed.
    async fn session_set_config_option(
        &self,
        engine: Arc<dyn EngineAdapter>,
        params: Value,
    ) -> Result<Value, (i64, String)> {
        let session_id = required_session_id(&params, "session/set_config_option")?;
        let config_id = params
            .get("configId")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                (
                    ERROR_INVALID_PARAMS,
                    "invalid params: missing 'configId' in session/set_config_option".to_string(),
                )
            })?;
        let value = params.get("value").cloned().ok_or_else(|| {
            (
                ERROR_INVALID_PARAMS,
                "invalid params: missing 'value' in session/set_config_option".to_string(),
            )
        })?;
        match config_id {
            "model" => {
                let model = value.as_str().ok_or_else(|| {
                    (
                        ERROR_INVALID_PARAMS,
                        "invalid params: 'value' must be a string model id for config option \
                         'model'"
                            .to_string(),
                    )
                })?;
                engine
                    .call_method(
                        "setModel",
                        json!({ "session_id": session_id, "model": model }),
                    )
                    .await
                    .map_err(map_engine_error)?;
            }
            "thinking" => {
                let thinking = value.as_str().ok_or_else(|| {
                    (
                        ERROR_INVALID_PARAMS,
                        "invalid params: 'value' must be a string thinking level for config \
                         option 'thinking'"
                            .to_string(),
                    )
                })?;
                // The engine's wire spelling is `none`; the boolean-style
                // `off` (the TS thinking values) is accepted as its alias.
                let thinking = if thinking == "off" { "none" } else { thinking };
                engine
                    .call_method(
                        "setThinking",
                        json!({ "session_id": session_id, "thinking": thinking }),
                    )
                    .await
                    .map_err(map_engine_error)?;
            }
            "mode" => {
                let mode_id = value.as_str().ok_or_else(|| {
                    (
                        ERROR_INVALID_PARAMS,
                        "invalid params: 'value' must be a string mode id for config option \
                         'mode'"
                            .to_string(),
                    )
                })?;
                if !is_acp_mode_id(mode_id) {
                    return Err((
                        ERROR_INVALID_PARAMS,
                        format!(
                            "invalid params: unknown mode id '{mode_id}' for config option \
                             'mode'; expected one of default, plan, auto, yolo"
                        ),
                    ));
                }
                self.apply_mode(&engine, &session_id, mode_id).await?;
            }
            unknown => {
                return Err((
                    ERROR_INVALID_PARAMS,
                    format!("invalid params: unknown config option '{unknown}'"),
                ));
            }
        }
        let (_, config_options) = self.session_mode_surface(&engine, &session_id).await?;
        let _ = self.updates.send(JsonRpcNotification {
            method: "session/update".to_string(),
            params: json!({
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": "config_option_update",
                    "configOptions": config_options,
                },
            }),
        });
        Ok(json!({ "configOptions": config_options }))
    }

    /// `session/set_model` — set the addressed session's model directly
    /// (unstable extension kept by the TS ACP server).
    async fn session_set_model(
        &self,
        engine: Arc<dyn EngineAdapter>,
        params: Value,
    ) -> Result<Value, (i64, String)> {
        let session_id = required_session_id(&params, "session/set_model")?;
        let model = params.get("model").and_then(Value::as_str).ok_or_else(|| {
            (
                ERROR_INVALID_PARAMS,
                "invalid params: missing 'model' in session/set_model".to_string(),
            )
        })?;
        engine
            .call_method(
                "setModel",
                json!({ "session_id": session_id, "model": model }),
            )
            .await
            .map_err(map_engine_error)?;
        Ok(json!({}))
    }

    // --- plumbing ---------------------------------------------------------

    /// Reject a call when `initialize` has not been negotiated yet.
    fn require_initialized(&self) -> Result<(), (i64, String)> {
        if self.initialized.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err((
                ERROR_NOT_INITIALIZED,
                "not initialized: send initialize before session methods".to_string(),
            ))
        }
    }

    // --- per-engine cwd bookkeeping ----------------------------------------

    /// The engine currently in the replaceable slot (design 14 §2): every
    /// read of the default-engine field goes through here, so a
    /// [`Self::replace_engine`] swap is visible to every later dispatch.
    fn current_engine(&self) -> Arc<dyn EngineAdapter> {
        let guard = self.engine.read().unwrap_or_else(|e| e.into_inner());
        Arc::clone(&guard)
    }

    /// Swap the default-engine slot and drop every routing cache keyed
    /// by the old engines (design 14 §2 "引擎替换接缝"):
    ///
    /// - `session_engines` / `engine_cwds` — a session remembered on the
    ///   dead engine must route to the new slot, never to the dead
    ///   adapter (`session/delete`'s file removal reads `engine_cwds`;
    ///   the old entries would name the dead engine's disk layout).
    /// - `cwd_engines` — in remote mode (no factory) this pool is empty
    ///   or irrelevant, and in embedded mode it stays untouched by this
    ///   call (the degrade target serves every cwd); clearing it is the
    ///   conservative choice that matches the remote-mode single-slot
    ///   contract.
    /// - `wired_engines` — `ensure_wired` is pointer-keyed, so the new
    ///   engine (a different `Arc` allocation) is not in the set and its
    ///   wiring (event forwarder + reverse-RPC bridge) re-runs on its
    ///   first dispatch.
    ///
    /// In-flight turns are NOT migrated: they already hold their engine
    /// `Arc` and settle against it (a dead remote answers their calls
    /// with the transport error — design 14 D3's "不迁移在途回合").
    pub fn replace_engine(&self, new: Arc<dyn EngineAdapter>) {
        *self.engine.write().unwrap_or_else(|e| e.into_inner()) = Arc::clone(&new);
        self.session_engines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.cwd_engines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.engine_cwds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.wired_engines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        tracing::info!(
            "engine slot replaced (design 14 D3): route caches cleared, \
             the new engine wires on its first dispatch"
        );
    }

    /// Install the engine-slot replacement machinery (design 14 D3):
    /// the re-probe path consulted once per connection death and the
    /// embedded fallback factory consulted once per degrade. Called by
    /// the production serve loop in remote mode; tests install counting
    /// closures. Idempotent: a second call replaces the machinery whole.
    pub fn set_engine_replacement_path(
        &self,
        probe_path: ProbePath,
        embedded_factory: EmbeddedEngineFactory,
    ) {
        *self
            .engine_replacement
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(EngineReplacement {
            probe_path,
            embedded_factory,
            degraded: AtomicBool::new(false),
        });
    }

    /// The handler-side wrapper of every engine call: when a
    /// transport-class error surfaces (a dead remote connection) and the
    /// replacement machinery is installed, run the one re-probe (design
    /// 14 D3):
    ///
    /// - probe success → the fresh engine takes the slot
    ///   ([`Self::replace_engine`]) and the ORIGINAL call is retried
    ///   once **on the fresh engine** (the closure receives it, so the
    ///   session is remembered against the engine that actually owns
    ///   it);
    /// - probe failure → the embedded engine takes the slot (the
    ///   one-way degrade) and the original error is returned (that
    ///   request failed — subsequent new-session calls land on the
    ///   embedded engine);
    /// - already degraded → return the original error (no probe, no
    ///   factory — D3's "降级后不再自动再探测").
    ///
    /// A machinery-less handler (embedded mode, legacy test injections)
    /// passes the error through unchanged — the pre-design-14 behavior.
    async fn handle_engine_call<F, Fut>(
        &self,
        error: EngineError,
        retry: F,
    ) -> Result<Value, EngineError>
    where
        F: FnOnce(Arc<dyn EngineAdapter>) -> Fut,
        Fut: Future<Output = Result<Value, EngineError>>,
    {
        // The one synchronous decision step (no await while the
        // machinery lock is held — the probe closure is sync by
        // contract): Reconnect (the fresh engine) or Degrade (the
        // embedded engine, already swapped into the slot). The
        // already-degraded case returns early above.
        enum Decision {
            Reconnect(Arc<dyn EngineAdapter>),
            Degrade,
        }
        let decision = {
            let machinery = self
                .engine_replacement
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let Some(replacement) = machinery.as_ref() else {
                return Err(error);
            };
            // Already degraded: the slot holds the embedded engine; the
            // old session's call keeps its NotConnected semantics (D3).
            if replacement.degraded.load(Ordering::SeqCst) {
                return Err(error);
            }
            match (replacement.probe_path)() {
                Some(new_remote) => Decision::Reconnect(new_remote),
                None => {
                    tracing::warn!(
                        "re-probe failed after connection death: degrading to the \
                         embedded engine for new sessions (design 14 D3 — one-way)"
                    );
                    let embedded = (replacement.embedded_factory)();
                    self.replace_engine(embedded);
                    replacement.degraded.store(true, Ordering::SeqCst);
                    Decision::Degrade
                }
            }
        };
        match decision {
            Decision::Degrade => Err(error),
            Decision::Reconnect(new_remote) => {
                tracing::info!("re-probe succeeded: swapping in the fresh remote engine");
                self.replace_engine(Arc::clone(&new_remote));
                retry(new_remote).await
            }
        }
    }
    /// Record the working directory an engine is rooted at — the
    /// production serve loop binds the default engine to the workspace
    /// directory; the pool mint records every factory engine itself.
    pub fn remember_engine_cwd(&self, engine: &Arc<dyn EngineAdapter>, cwd: PathBuf) {
        self.engine_cwds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(Arc::as_ptr(engine).cast::<()>() as usize, cwd);
    }

    /// The recorded cwd of an engine, when known (see [
    /// `Self::remember_engine_cwd`]).
    fn engine_cwd(&self, engine: &Arc<dyn EngineAdapter>) -> Option<PathBuf> {
        self.engine_cwds
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(Arc::as_ptr(engine).cast::<()>() as usize))
            .cloned()
    }

    /// Translate a request cwd into the remote server's workspace id
    /// (design 14 D2): list the server's registered workspaces
    /// (`workspace.list`), lexically resolve the cwd and each registered
    /// root (`lexically_resolve` — the registry's storage form; symlink
    /// folding is deliberately NOT done, matching how the desktop
    /// registers roots), and return the id of the row whose root is
    /// lexically equal to the cwd.
    ///
    /// A miss is an error, never a fallback: an unregistered cwd routed
    /// to the server's default workspace would sandbox the session's
    /// file access in an unrelated directory (a correctness bug, not a
    /// cosmetic one) — the caller surfaces this message to the IDE user,
    /// whose next step is to open the directory in the desktop app.
    async fn translate_cwd_to_workspace_id(
        &self,
        engine: &Arc<dyn EngineAdapter>,
        cwd: &Path,
    ) -> Result<String, (i64, String)> {
        let listing = match engine.call_method("workspace.list", json!({})).await {
            Ok(listing) => listing,
            // A transport-class failure on the remote engine (the desktop
            // server exited): run the design-14 D3 re-probe / degrade chain
            // HERE, before the create/resume this translation feeds —
            // without it, a connection death surfacing in the translation
            // (which runs before the engine's session call) would bypass
            // the chain entirely and answer a plain internal error where
            // the host should have reconnected or degraded.
            Err(error) if is_transport_class(&error) => {
                match self
                    .handle_engine_call(error, |fresh| async move {
                        fresh.call_method("workspace.list", json!({})).await
                    })
                    .await
                {
                    Ok(listing) => {
                        // The slot was replaced (re-probe success): the
                        // caller still holds the OLD engine Arc, but its
                        // create/resume paths already run their own D3
                        // chain — the translation below just matches the
                        // workspace rows, which this fresh listing carries.
                        listing
                    }
                    Err(error) => return Err(map_engine_error(error)),
                }
            }
            Err(error) => return Err(map_engine_error(error)),
        };
        translate_listing_for_cwd(&listing, cwd).ok_or_else(|| {
            (
                ERROR_INVALID_PARAMS,
                format!(
                    "directory {} is not a registered workspace of the desktop server; \
                     open it in the desktop app first",
                    cwd.display()
                ),
            )
        })
    }

    /// Wire the engine-facing machinery once per engine, before the first
    /// method on it proceeds: the engine-event forwarder and the
    /// reverse-RPC bridge (`approval.ask` → `session/request_permission`).
    /// Every engine of the per-cwd pool needs its own wiring — its events
    /// and its permission asks must reach the client exactly like the
    /// default engine's.
    async fn ensure_wired(&self, engine: &Arc<dyn EngineAdapter>) -> Result<(), (i64, String)> {
        let key = Arc::as_ptr(engine).cast::<()>() as usize;
        if self
            .wired_engines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&key)
        {
            return Ok(());
        }
        self.ensure_update_forwarder(engine).await?;
        self.ensure_reverse_rpc_bridge(engine).await?;
        self.wired_engines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key);
        Ok(())
    }

    /// Start the given engine's event → `session/update` forwarder once.
    /// The subscription is established before the method proceeds, so
    /// events published by the method itself are not missed (the channel
    /// buffers them until the forwarder task drains it). The task owns
    /// the engine's [`EventTranslator`] — the per-tool-call state the
    /// `tool_call` CREATE's content/rawInput (and the terminal update's
    /// tool name) derive from.
    async fn ensure_update_forwarder(
        &self,
        engine: &Arc<dyn EngineAdapter>,
    ) -> Result<(), (i64, String)> {
        let mut events = engine
            .subscribe_events()
            .await
            .map_err(|error| (ERROR_INTERNAL, error.to_string()))?;
        let updates = self.updates.clone();
        tokio::spawn(async move {
            let mut translator = EventTranslator::default();
            while let Some(notification) = events.recv().await {
                if let Some(update) = translator.translate(&notification) {
                    // No listeners is not an error: the update is dropped.
                    let _ = updates.send(update);
                }
            }
        });
        Ok(())
    }

    /// Register the reverse-RPC bridge with the given engine once — the
    /// seam the engine's `approval.ask` (permission gate `Ask` branch) and
    /// `question.ask` (AskUserQuestion tool) flow through. Without it the
    /// manual permission mode never reaches the client at all (the engine
    /// rejects with "no approval mechanism available").
    async fn ensure_reverse_rpc_bridge(
        &self,
        engine: &Arc<dyn EngineAdapter>,
    ) -> Result<(), (i64, String)> {
        let requests = self.client_requests.clone();
        let active_prompts = Arc::clone(&self.active_prompt_sessions);
        let pending_asks = Arc::clone(&self.pending_permission_asks);
        let client_capabilities = Arc::clone(&self.client_capabilities);
        let ask_seq = Arc::new(std::sync::atomic::AtomicU64::new(0));
        engine
            .on_reverse_rpc(Arc::new(move |method: String, params: Value| {
                let requests = requests.clone();
                let active_prompts = active_prompts.clone();
                let pending_asks = pending_asks.clone();
                let ask_seq = ask_seq.clone();
                let client_capabilities = client_capabilities.clone();
                Box::pin(async move {
                    match method.as_str() {
                        "approval.ask" => {
                            // The ask carries no session id; address it
                            // to the session of the most recently started
                            // in-flight prompt (the ACP model has one
                            // prompt turn in flight per client).
                            let session_id = active_prompts
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .last()
                                .cloned()
                                .ok_or_else(|| {
                                    "no in-flight prompt session to address a \
                                         permission request to"
                                        .to_string()
                                })?;
                            let params = approval_ask_to_request_permission(&session_id, &params);
                            let (reply_tx, reply_rx) = oneshot::channel();
                            // Register the ask (F-P5b-4): if the session's
                            // prompt is cancelled before the client
                            // answers, the cancel path fires `cancel_rx`
                            // and the ask settles with the v1 Cancelled
                            // outcome instead of waiting out the bridge
                            // timeout. The guard removes the registration
                            // when the ask settles through any channel.
                            let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
                            let seq = ask_seq.fetch_add(1, Ordering::Relaxed);
                            pending_asks
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .entry(session_id.clone())
                                .or_default()
                                .insert(seq, cancel_tx);
                            let _ask_registration = PendingAskGuard {
                                asks: pending_asks.clone(),
                                session_id: session_id.clone(),
                                seq,
                            };
                            let request = PendingClientRequest::new(
                                "session/request_permission",
                                params,
                                reply_tx,
                            );
                            if requests.send(request).is_ok() {
                                let reply = tokio::select! {
                                    reply = reply_rx => {
                                        approval_reply_from_client_result(reply)
                                    }
                                    // The prompt was cancelled: answer as
                                    // the v1 client that MUST respond to
                                    // its pending permission requests with
                                    // the Cancelled outcome.
                                    _ = cancel_rx => permission_response_to_approval(
                                        &json!({"outcome": {"outcome": "cancelled"}}),
                                    ),
                                };
                                Ok(reply)
                            } else {
                                Ok(rejected_fallback(
                                    "the permission bridge is not being served",
                                ))
                            }
                        }
                        // The AskUserQuestion tool's ask (F-P5b-1):
                        // bridged onto the client's interactive surface —
                        // `elicitation/create` (form mode) for clients
                        // that advertised `elicitation.form` at
                        // initialize, the single-select
                        // `session/request_permission` surface for
                        // everyone else (TS interaction-bridge parity).
                        // The reply maps onto the rust-native
                        // `{"answers": {…}}` shape; a dismissed question
                        // (or a delivery failure) answers the empty map —
                        // never fabricated, never a hang.
                        "question.ask" => {
                            // The ask carries no session id; address it to
                            // the session of the most recently started
                            // in-flight prompt (same rule as approval.ask).
                            let session_id = active_prompts
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .last()
                                .cloned()
                                .ok_or_else(|| {
                                    "no in-flight prompt session to address a \
                                         question to"
                                        .to_string()
                                })?;
                            let questions: Vec<Value> = params
                                .get("questions")
                                .and_then(Value::as_array)
                                .cloned()
                                .unwrap_or_default();
                            let elicitation_form = {
                                let capabilities = client_capabilities
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner());
                                question::elicitation_form_supported(capabilities.as_ref())
                            };
                            if elicitation_form {
                                let (reply_tx, reply_rx) = oneshot::channel();
                                let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
                                let seq = ask_seq.fetch_add(1, Ordering::Relaxed);
                                pending_asks
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .entry(session_id.clone())
                                    .or_default()
                                    .insert(seq, cancel_tx);
                                let _ask_registration = PendingAskGuard {
                                    asks: pending_asks.clone(),
                                    session_id: session_id.clone(),
                                    seq,
                                };
                                let request = PendingClientRequest::new(
                                    "elicitation/create",
                                    question::question_to_elicitation_params(
                                        &session_id,
                                        &questions,
                                    ),
                                    reply_tx,
                                );
                                if requests.send(request).is_ok() {
                                    let reply = tokio::select! {
                                        reply = reply_rx => {
                                            reply.ok().and_then(|reply| reply.ok())
                                        }
                                        _ = cancel_rx => None,
                                    };
                                    Ok(match reply {
                                        Some(response) => {
                                            question::elicitation_response_to_answers(
                                                &questions, &response,
                                            )
                                        }
                                        None => json!({"answers": {}}),
                                    })
                                } else {
                                    Ok(json!({"answers": {}}))
                                }
                            } else {
                                // Degraded single-select surface: only the
                                // first question is asked (TS degradation
                                // rule, logged loudly).
                                if questions.len() > 1 {
                                    tracing::warn!(
                                        dropped = questions.len() - 1,
                                        "question.ask degrading to the first question only \
                                         (the client advertised no elicitation capability)"
                                    );
                                }
                                let first = questions.first().cloned().unwrap_or(Value::Null);
                                let (reply_tx, reply_rx) = oneshot::channel();
                                let (cancel_tx, cancel_rx) = oneshot::channel::<()>();
                                let seq = ask_seq.fetch_add(1, Ordering::Relaxed);
                                pending_asks
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .entry(session_id.clone())
                                    .or_default()
                                    .insert(seq, cancel_tx);
                                let _ask_registration = PendingAskGuard {
                                    asks: pending_asks.clone(),
                                    session_id: session_id.clone(),
                                    seq,
                                };
                                let request = PendingClientRequest::new(
                                    "session/request_permission",
                                    question::question_to_permission_params(
                                        &session_id,
                                        &questions,
                                    ),
                                    reply_tx,
                                );
                                if requests.send(request).is_ok() {
                                    let reply = tokio::select! {
                                        reply = reply_rx => {
                                            reply.ok().and_then(|reply| reply.ok())
                                        }
                                        _ = cancel_rx => None,
                                    };
                                    Ok(match reply {
                                        Some(response) => question::permission_response_to_answers(
                                            &first, &response,
                                        ),
                                        None => json!({"answers": {}}),
                                    })
                                } else {
                                    Ok(json!({"answers": {}}))
                                }
                            }
                        }
                        other => Err(format!(
                            "reverse RPC method not bridged to the ACP client: {other}"
                        )),
                    }
                })
            }))
            .await
            .map_err(|error| (ERROR_INTERNAL, error.to_string()))
    }

    // --- per-cwd engine pool & session routing ----------------------------

    /// Resolve the engine for a `session/new`: the request's `cwd`,
    /// validated (absolute, existing) and canonicalized, selects the
    /// pooled engine — minted through the factory on first sight, shared
    /// by every later session of the same cwd. A request without a `cwd`
    /// (pre-v1 callers, tests) uses the default engine. Without a factory
    /// the handler degrades to single-engine mode: the `cwd` is still
    /// validated, but the default engine serves the request.
    async fn engine_for_new_session(
        &self,
        params: &Value,
    ) -> Result<Arc<dyn EngineAdapter>, (i64, String)> {
        match params.get("cwd").and_then(Value::as_str) {
            None => Ok(self.current_engine()),
            Some(cwd) => self.engine_for_cwd(cwd).await,
        }
    }

    /// Resolve (and validate) the pooled engine of a request `cwd`.
    async fn engine_for_cwd(&self, cwd: &str) -> Result<Arc<dyn EngineAdapter>, (i64, String)> {
        let path = Path::new(cwd);
        if !path.is_absolute() {
            return Err((
                ERROR_INVALID_PARAMS,
                format!("invalid params: 'cwd' must be an absolute path, got '{cwd}'"),
            ));
        }
        // The canonical form keys the pool (symlinks and trailing slashes
        // must not split one directory into two engines). A nonexistent
        // cwd has no canonical form — reject it before anything else. In
        // remote mode the check is skipped: the engine is not rooted at
        // the local filesystem (the desktop server's workspace registry
        // vouches for the root instead — the D2 translation rejects a
        // cwd no workspace claims), and a lexically-registered root that
        // this machine resolves differently (symlinked spellings) must
        // still route.
        if !self.current_engine().is_remote() {
            let canonical = std::fs::canonicalize(path).map_err(|error| {
                (
                    ERROR_INVALID_PARAMS,
                    format!("invalid params: 'cwd' cannot be resolved ({cwd}): {error}"),
                )
            })?;
            if let Some(engine) = self
                .cwd_engines
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&canonical)
            {
                return Ok(Arc::clone(engine));
            }
            let Some(factory) = &self.engine_factory else {
                // Single-engine mode (the `with_engine` injection contract):
                // the cwd is valid, but everything runs on the default engine.
                return Ok(self.current_engine());
            };
            let engine = factory(&canonical);
            self.cwd_engines
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(canonical.clone(), Arc::clone(&engine));
            // The engine's cwd is what `session/list` reports for its sessions
            // and where `session/delete` removes the persisted files (the
            // factory may hand back the default engine for the default root —
            // then this records the same cwd the serve loop did, idempotently).
            self.remember_engine_cwd(&engine, canonical);
            Ok(engine)
        } else {
            // Remote mode: the single slot serves every cwd (design 14 §2 —
            // no per-cwd pool across a wire), and registration is the D2
            // translation's business, not the local filesystem's.
            Ok(self.current_engine())
        }
    }

    /// Resolve the engine a session-addressed method must run on: the
    /// engine recorded for the addressed session, the default engine when
    /// the session is unknown (the engine then answers the not-found
    /// error itself, exactly like the single-engine era).
    fn engine_for_addressed_session(&self, params: &Value) -> Arc<dyn EngineAdapter> {
        params
            .get("sessionId")
            .and_then(Value::as_str)
            .and_then(|session_id| {
                self.session_engines
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(session_id)
                    .cloned()
            })
            .unwrap_or_else(|| self.current_engine())
    }

    /// Resolve the engine for `session/load` / `session/resume`: the
    /// engine recorded for the session (kept across `session/close`, so a
    /// close→load round trip routes back to the engine that owns the
    /// session's persistence), else the request `cwd`'s pooled engine
    /// (validated like `session/new`'s — a session unknown to this process
    /// is claimed by the cwd it is loaded with), else the default engine.
    async fn engine_for_load(
        &self,
        params: &Value,
    ) -> Result<Arc<dyn EngineAdapter>, (i64, String)> {
        if let Some(engine) = self.engine_for_addressed_session_checked(params) {
            return Ok(engine);
        }
        match params.get("cwd").and_then(Value::as_str) {
            Some(cwd) => self.engine_for_cwd(cwd).await,
            None => Ok(self.current_engine()),
        }
    }

    /// [`Self::engine_for_addressed_session`] as an `Option`: `None` when
    /// the session is unknown to the routing table.
    fn engine_for_addressed_session_checked(
        &self,
        params: &Value,
    ) -> Option<Arc<dyn EngineAdapter>> {
        params
            .get("sessionId")
            .and_then(Value::as_str)
            .and_then(|session_id| {
                self.session_engines
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(session_id)
                    .cloned()
            })
    }

    /// Record which engine owns a session (the session→engine routing
    /// entry every session-addressed method dispatches through).
    fn remember_session_engine(&self, session_id: &str, engine: &Arc<dyn EngineAdapter>) {
        self.session_engines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session_id.to_string(), Arc::clone(engine));
    }

    /// Drop a session's routing entry (the session was deleted).
    fn forget_session_engine(&self, session_id: &str) {
        self.session_engines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(session_id);
    }

    /// Every engine this handler dispatches to — the default engine plus
    /// each pooled one — deduplicated by pointer (for `session/list`).
    fn all_engines(&self) -> Vec<Arc<dyn EngineAdapter>> {
        let current = self.current_engine();
        let mut engines = vec![Arc::clone(&current)];
        let mut seen: HashSet<usize> = [Arc::as_ptr(&current).cast::<()>() as usize].into();
        for engine in self
            .cwd_engines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
        {
            let key = Arc::as_ptr(&engine).cast::<()>() as usize;
            if seen.insert(key) {
                engines.push(engine);
            }
        }
        engines
    }

    /// Record that a `session/prompt` turn started for the session.
    fn push_active_prompt_session(&self, session_id: &str) {
        self.active_prompt_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(session_id.to_string());
    }

    /// Record that the session's `session/prompt` turn ended.
    fn pop_active_prompt_session(&self, session_id: &str) {
        let mut sessions = self
            .active_prompt_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(index) = sessions.iter().rposition(|session| session == session_id) {
            sessions.remove(index);
        }
    }
}

impl Default for AcpHandler {
    fn default() -> Self {
        Self::new()
    }
}

/// Extract the required `sessionId` string from ACP params.
fn required_session_id(params: &Value, method: &str) -> Result<String, (i64, String)> {
    params
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            (
                ERROR_INVALID_PARAMS,
                format!("invalid params: missing 'sessionId' in {method}"),
            )
        })
}

/// Parse a request-id JSON value into its [`RequestId`] wire shape — a
/// number or a string (JSON-RPC 2.0 §4; ACP clients like Zed mint UUID
/// strings, this codebase's own clients integers). Any other shape is
/// `None`.
fn request_id_from_value(value: &Value) -> Option<RequestId> {
    match value {
        Value::Number(number) => number.as_i64().map(RequestId::Number),
        Value::String(text) => Some(RequestId::Str(text.clone())),
        _ => None,
    }
}

/// Warn about a request's `additionalDirectories` the host cannot honor:
/// the engine's workspace scope is single-rooted, so the field is never
/// advertised in `sessionCapabilities` and a request carrying it is
/// served at the `cwd`'s scope with this loud warning (TS
/// `warnIgnoredAdditionalDirs` parity — never a silent no-op).
fn warn_ignored_additional_directories(method: &str, additional: Option<&Value>) {
    let Some(dirs) = additional.and_then(Value::as_array) else {
        return;
    };
    if dirs.is_empty() {
        return;
    }
    tracing::warn!(
        method,
        dirs = ?dirs,
        "{method} ignores additionalDirectories (the engine's workspace scope \
         is single-rooted; the field is not advertised)"
    );
}

/// Map an engine session-list view onto the official v1 `SessionInfo`
/// (G9): the required `sessionId` / `cwd` plus `title` / `updatedAt`
/// (null when the engine reports none), with the engine-side fields that
/// have no v1 slot — `createdAt`, `agentCount`, `tags`, `status`
/// (`running` / `closed`) — under `_meta`, the reserved extensibility
/// channel (TS `sessionSummaryToSessionInfo` parity for the top-level
/// shape, `cwd ?? ''` for an engine whose root the host does not know).
fn session_info(view: &Value, engine_cwd: Option<&Path>) -> Value {
    let title = view
        .get("title")
        .and_then(Value::as_str)
        .filter(|title| !title.is_empty());
    json!({
        "sessionId": view.get("id").cloned().unwrap_or(Value::Null),
        "cwd": engine_cwd
            .map(|cwd| cwd.to_string_lossy().into_owned())
            .unwrap_or_default(),
        "title": title,
        "updatedAt": view.get("updated_at").cloned().unwrap_or(Value::Null),
        "_meta": {
            "createdAt": view.get("created_at").cloned().unwrap_or(Value::Null),
            "agentCount": view.get("agent_count").cloned().unwrap_or(json!(0)),
            "tags": view.get("tags").cloned().unwrap_or_else(|| json!([])),
            "status": view.get("status").cloned().unwrap_or(Value::Null),
        },
    })
}

/// Whether an engine error is connection-death shaped — the only class
/// that triggers the design-14 D3 re-probe / degrade chain. Business
/// errors (a not-found session, an invalid parameter, a method the
/// engine lacks) answer the request exactly as before.
/// Match a `workspace.list` listing against a request cwd (design 14
/// D2's lexical rule): the id of the row whose root is lexically equal to
/// the cwd, or `None` when no row claims it. Shared by the create, load,
/// and list-pinning translations.
fn translate_listing_for_cwd(listing: &Value, cwd: &Path) -> Option<String> {
    // W5's page envelope `{items, …}` over the bare-array legacy form.
    let rows = listing["items"]
        .as_array()
        .cloned()
        .unwrap_or_else(|| listing.as_array().cloned().unwrap_or_default());
    let wanted = mirri_core::session::lexically_resolve(cwd);
    for row in rows {
        let Some(root) = row.get("root").and_then(Value::as_str) else {
            continue;
        };
        if mirri_core::session::lexically_resolve(Path::new(root)) == wanted {
            let Some(id) = row.get("id").and_then(Value::as_str) else {
                continue;
            };
            return Some(id.to_string());
        }
    }
    None
}

/// Whether an engine error means the transport (not the business logic)
/// failed — the trigger set of the re-probe/degrade chain (design 14 D3).
fn is_transport_class(error: &EngineError) -> bool {
    matches!(error, EngineError::Transport(_) | EngineError::NotConnected)
}

/// Map an engine error to the ACP error shape `(code, message)`.
fn map_engine_error(error: EngineError) -> (i64, String) {
    match error {
        EngineError::MethodNotFound(method) => (
            ERROR_METHOD_NOT_FOUND,
            format!("method not found: {method}"),
        ),
        EngineError::Rpc {
            code: -32001,
            message,
            ..
        } => (ERROR_SESSION_NOT_FOUND, message),
        EngineError::Rpc {
            code: -32602,
            message,
            ..
        } => (ERROR_INVALID_PARAMS, message),
        EngineError::Rpc { message, .. }
        | EngineError::Internal(message)
        | EngineError::Transport(message) => (ERROR_INTERNAL, message),
        EngineError::NotConnected => (ERROR_INTERNAL, "engine not connected".to_string()),
    }
}

/// The canonical permission options surfaced to the ACP client for a tool
/// approval (the `CANONICAL_OPTIONS` of the TS `approval.ts` — order is
/// load-bearing: allow-once is the primary action, allow-always the
/// secondary, reject the terminal one).
///
/// The `plan_review` expansion of the TS mapper is not reproduced here: the
/// rust engine's approval asks do not carry plan-review display blocks yet.
fn canonical_permission_options() -> Value {
    json!([
        {"optionId": "approve_once", "name": "Approve once", "kind": "allow_once"},
        {
            "optionId": "approve_always",
            "name": "Approve for this session",
            "kind": "allow_always",
        },
        {"optionId": "reject", "name": "Reject", "kind": "reject_once"},
    ])
}

/// Translate an engine `approval.ask` payload (`{tool_name, tool_call_id,
/// action, display, reason, request_id?}`) into the ACP
/// `session/request_permission` request params (the official
/// `RequestPermissionRequest` shape: `{sessionId, toolCall, options}`).
///
/// Mirrors the TS `approval.ts` mappers: the tool call is scoped to the
/// engine's own tool call id (the same id the `tool_call` updates carry, so
/// the client correlates the prompt with the rendered tool card), the title
/// is the tool name, and the content carries the human-readable action
/// summary. The display block is not mapped — the rust engine's display
/// variants carry no diff/plan body (the only kinds the TS mapper renders).
fn approval_ask_to_request_permission(session_id: &str, ask: &Value) -> Value {
    let tool_name = ask
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("tool");
    let tool_call_id = ask
        .get("tool_call_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let action = ask
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("execute the tool");
    json!({
        "sessionId": session_id,
        "toolCall": {
            "toolCallId": tool_call_id,
            "title": tool_name,
            "content": [{
                "type": "content",
                "content": {"type": "text", "text": format!("Requesting approval to {action}")},
            }],
        },
        "options": canonical_permission_options(),
    })
}

/// Translate the client's `RequestPermissionResponse` (the reply to
/// `session/request_permission`) into the engine's approval response shape
/// (`{decision, scope?, feedback?}`).
///
/// Decision mapping (TS `permissionResponseToApprovalResponse`):
/// - a `cancelled` outcome → `decision: "cancelled"`;
/// - `approve_once` (and the legacy `approve` id) → `approved`, one-shot;
/// - `approve_always` (and the legacy `approve_for_session` id) →
///   `approved` with `scope: "session"`, so the engine installs a
///   session-runtime allow rule for subsequent calls of the same pattern;
/// - `reject`, any unknown optionId, or a malformed reply → `rejected`
///   (rejecting is strictly safer than approving).
fn permission_response_to_approval(response: &Value) -> Value {
    let outcome = &response["outcome"];
    if outcome.get("outcome").and_then(Value::as_str) == Some("cancelled") {
        return json!({"decision": "cancelled"});
    }
    let option_id = outcome.get("optionId").and_then(Value::as_str);
    match option_id {
        // Legacy 'approve' / 'approve_for_session' ids come from custom
        // ACP clients built against the pre-v1 Python SDK (TS parity).
        Some("approve_once") | Some("approve") => json!({"decision": "approved"}),
        Some("approve_always") | Some("approve_for_session") => {
            json!({"decision": "approved", "scope": "session"})
        }
        _ => json!({"decision": "rejected"}),
    }
}

/// Map the engine's wire stop reason (`message.send`'s `turn.stopReason`)
/// onto the official v1 `StopReason` vocabulary of the `session/prompt`
/// response (TS `turnEndReasonToStopReason` parity plus the engine-only
/// reasons): `aborted` — a turn cancelled through `session/cancel` or
/// `$/cancel_request` — is the v1 `cancelled` the client's terminal state
/// requires; a provider-filtered reply is the v1 `refusal`; and every
/// reason without a v1 counterpart settles `end_turn` so the client is
/// never stranded on an off-vocabulary literal (errors belong on the
/// JSON-RPC error channel, not `stopReason`).
fn acp_stop_reason(wire_reason: &str) -> &'static str {
    match wire_reason {
        "end_turn" => "end_turn",
        "max_tokens" => "max_tokens",
        "aborted" => "cancelled",
        "filtered" => "refusal",
        _ => "end_turn",
    }
}

/// The rejected fallback for a bridged approval ask that could not be
/// delivered or answered (timeout, disconnect, malformed reply) — rejecting
/// on failure is strictly safer than approving (TS parity:
/// `handleApproval` catches every request failure as `decision:
/// 'rejected'`). The reason rides along as the rejection feedback so the
/// turn explains *why* the tool was denied.
fn rejected_fallback(reason: &str) -> Value {
    json!({"decision": "rejected", "feedback": format!("permission request failed: {reason}")})
}

/// Fold the client's (or the transport's) answer to a bridged approval ask
/// into the engine's approval response: a delivered reply is mapped through
/// [`permission_response_to_approval`], every delivery failure — a failed
/// request, a timeout, a disconnect (the reply channel dropped) — resolves
/// to the rejected fallback. The ask never hangs.
fn approval_reply_from_client_result(
    reply: Result<Result<Value, String>, oneshot::error::RecvError>,
) -> Value {
    match reply {
        Ok(Ok(response)) => permission_response_to_approval(&response),
        Ok(Err(reason)) => rejected_fallback(&reason),
        Err(_) => rejected_fallback("client disconnected before responding"),
    }
}

/// One tool call's live translation state: the tool's name (from the
/// started event or the first delta that carried it) and the
/// streaming-accumulated argument fragments (the `tool.call.delta`
/// events) — what the `tool_call` CREATE's `rawInput` / `content` and
/// the terminal update's `name` derive from (G7).
#[derive(Default)]
struct EventTranslator {
    /// toolCallId → (name, accumulated argument fragments). Entries are
    /// added by `tool.call.delta` / `tool.call.started` and removed by the
    /// terminal `tool.result` (the ids are session-globally unique).
    tool_calls: HashMap<String, TrackedToolCall>,
}

/// The tracked state of one in-flight tool call.
#[derive(Default)]
struct TrackedToolCall {
    /// The tool's name, when a projection event carried it.
    name: Option<String>,
    /// The concatenated `argumentsPart` fragments streamed so far.
    arguments: String,
}

/// Heuristic map from the engine's tool `name` onto the official v1
/// `ToolKind` (TS `inferToolKind` parity): the engine's builtin names
/// (`Read` / `Write` / `Bash` / …) match the TS table verbatim — the one
/// rust-spelling addition is `FetchURL` (the TS table's `WebFetch`).
/// Pure and total: an unrecognized name lands on `other`, never blocking
/// the stream on an unknown tool.
fn infer_tool_kind(name: &str) -> &'static str {
    match name {
        "Read" | "Glob" | "Grep" => "read",
        "Write" | "Edit" => "edit",
        "Bash" | "Terminal" => "execute",
        "WebFetch" | "WebSearch" | "FetchURL" => "fetch",
        "Think" => "think",
        _ => "other",
    }
}

impl EventTranslator {
    /// Translate one engine event notification into an ACP `session/update`
    /// notification carrying the official v1 [`SessionUpdate`] vocabulary,
    /// or `None` when the event has no ACP counterpart (see the module
    /// docs of the previous free function — the vocabulary is unchanged;
    /// this adds the per-tool-call state the content-bearing fields need).
    fn translate(&mut self, notification: &JsonRpcNotification) -> Option<JsonRpcNotification> {
        let event_type = notification.method.strip_prefix("event.")?;
        let params = &notification.params;
        let session_id = params.get("sessionId").and_then(Value::as_str)?;
        let update = match event_type {
            // Internal bookkeeping: the user message is visible through
            // `session/prompt` params and has no ACP update counterpart.
            "message.received" => return None,
            // Workspace lifecycle (contract 3 sync — the W2 server-scoped
            // events): ACP's `session/update` vocabulary is session-scoped
            // and carries no workspace registry surface; the events are
            // deliberately dropped (an explicit arm, so the drop is
            // documented rather than falling through the unknown-event
            // catch-all).
            "workspace.created" | "workspace.updated" | "workspace.deleted" => return None,
            // Turn bookkeeping: v1 expresses turn boundaries through the
            // `session/prompt` response, not updates — a failed turn surfaces
            // as that response's JSON-RPC error.
            "turn.started" | "turn.ended" => return None,
            "assistant.delta" => {
                let text = params
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                json!({
                    "sessionUpdate": "agent_message_chunk",
                    "content": {"type": "text", "text": text},
                })
            }
            "thinking.delta" => {
                let text = params
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                json!({
                    "sessionUpdate": "agent_thought_chunk",
                    "content": {"type": "text", "text": text},
                })
            }
            // The streamed argument fragments accumulate into the tracked
            // call (what the later CREATE's `rawInput` / `content` show).
            // No wire update: v1's `ToolCallUpdate.content` is REPLACE
            // semantics and the CREATE has not been emitted yet — pushing
            // args before the create would orphan the client's card (the
            // TS lazy-create problem; the rust engine's started event
            // arrives after streaming, so the CREATE lands first).
            "tool.call.delta" => {
                let tool_call_id = params.get("toolCallId").and_then(Value::as_str)?;
                let tracked = self.tool_calls.entry(tool_call_id.to_string()).or_default();
                if let Some(name) = params.get("name").and_then(Value::as_str)
                    && tracked.name.is_none()
                {
                    tracked.name = Some(name.to_string());
                }
                if let Some(fragment) = params.get("argumentsPart").and_then(Value::as_str) {
                    tracked.arguments.push_str(fragment);
                }
                return None;
            }
            "tool.call.started" => {
                let tool_name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let tool_call_id = params
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let tracked = self.tool_calls.entry(tool_call_id.to_string()).or_default();
                if tracked.name.is_none() {
                    tracked.name = Some(tool_name.to_string());
                }
                // `title` is v1 ToolCall's only required field (the
                // projection carries no description, so the tool name is
                // the title); `kind` derives from the name (TS
                // `inferToolKind`); `rawInput` / `content` carry the
                // streamed arguments when any arrived before the dispatch
                // (G7) — otherwise they fall back to the wire `args`
                // object the field-level alignment added (non-streaming
                // providers emit no `tool.call.delta`, so without the
                // fallback their ACP tool cards would carry no input at
                // all). Both stay omitted when neither source exists.
                let mut update = json!({
                    "sessionUpdate": "tool_call",
                    "toolCallId": tool_call_id,
                    "title": tool_name,
                    "kind": infer_tool_kind(tool_name),
                    "status": "in_progress",
                });
                if !tracked.arguments.is_empty() {
                    update["rawInput"] = json!(tracked.arguments.clone());
                    update["content"] = json!([{
                        "type": "content",
                        "content": {"type": "text", "text": tracked.arguments.clone()},
                    }]);
                } else if let Some(args) = params.get("args").filter(|args| args.is_object()) {
                    // The wire's parsed argument object, serialized as a
                    // compact JSON string — the same shape the streamed
                    // fragments accumulate into `rawInput`.
                    let raw = serde_json::to_string(args).unwrap_or_default();
                    if !raw.is_empty() {
                        update["rawInput"] = json!(raw.clone());
                        update["content"] = json!([{
                            "type": "content",
                            "content": {"type": "text", "text": raw},
                        }]);
                    }
                }
                update
            }
            "tool.result" => {
                let tool_call_id = params
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let is_error = params
                    .get("isError")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                // The terminal update marks success/failure through the
                // status enum (v1 ToolCallStatus has a `failed` variant,
                // matching the TS events-map tool-result mapping). `name`
                // rides along when a projection event carried it (the
                // unstable ToolCallUpdate field — the client correlates
                // the result with the tool that produced it): the
                // translator's own tracking first, then the wire `name`
                // the field-level alignment added (a result whose started
                // event the translator never saw has no tracked entry);
                // `content` / `rawOutput` stay omitted: the engine's
                // tool.result projection carries no output (G7's
                // remaining gap).
                let tracked = self.tool_calls.remove(tool_call_id);
                let name = tracked.and_then(|tracked| tracked.name).or_else(|| {
                    params
                        .get("name")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                });
                let mut update = json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": tool_call_id,
                    "status": if is_error { "failed" } else { "completed" },
                });
                if let Some(name) = name {
                    update["name"] = json!(name);
                }
                update
            }
            "tool.progress" => {
                // Only a `status` update with non-empty text maps onto the
                // wire (as a tool-card title refresh, matching the TS
                // events-map); every other progress kind is dropped.
                let progress = params.get("update")?;
                if progress.get("kind").and_then(Value::as_str) != Some("status") {
                    return None;
                }
                let text = progress.get("text").and_then(Value::as_str)?;
                if text.is_empty() {
                    return None;
                }
                let tool_call_id = params
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": tool_call_id,
                    "title": text,
                })
            }
            _ => return None,
        };
        Some(JsonRpcNotification {
            method: "session/update".to_string(),
            params: json!({
                "sessionId": session_id,
                "update": update,
            }),
        })
    }
}

/// The locked 4-mode ACP taxonomy (TS `modes.ts` parity): `default` /
/// `plan` / `auto` / `yolo`, `default` first and `yolo` last (the array is
/// rendered as-is by the client).
fn acp_available_modes() -> Value {
    json!([
        {
            "id": "default",
            "name": "Default",
            "description": "Manual approvals; tools execute normally.",
        },
        {
            "id": "plan",
            "name": "Plan",
            "description": "Read-only planning; no tool execution.",
        },
        {
            "id": "auto",
            "name": "Auto",
            "description": "Auto-approve safe operations.",
        },
        {
            "id": "yolo",
            "name": "YOLO",
            "description": "Auto-approve everything.",
        },
    ])
}

/// The same taxonomy as the `mode` arm of the `configOptions` select (TS
/// `buildModeOption` parity — the select spells the entries `value`).
fn acp_mode_select_options() -> Value {
    json!([
        {
            "value": "default",
            "name": "Default",
            "description": "Manual approvals; tools execute normally.",
        },
        {
            "value": "plan",
            "name": "Plan",
            "description": "Read-only planning; no tool execution.",
        },
        {
            "value": "auto",
            "name": "Auto",
            "description": "Auto-approve safe operations.",
        },
        {
            "value": "yolo",
            "name": "YOLO",
            "description": "Auto-approve everything.",
        },
    ])
}

/// The `model` arm of the `configOptions` select: one option per alias the
/// engine's model catalog declares (`value` = the alias key, `name` = the
/// alias's `displayName` when present, the raw key otherwise), sorted by
/// alias key — the catalog is a HashMap on the wire, so the output is
/// sorted for deterministic tests and a stable host UI. A current model
/// missing from the catalog (env-overridden, remote-only) is appended at
/// the end so the currentValue is always represented; an entirely
/// empty/missing catalog falls back to the single current-model option.
fn acp_model_select_options(catalog: &Value, current: &str) -> Value {
    let Some(catalog) = catalog.as_object() else {
        return json!([{"value": current, "name": current}]);
    };
    let mut entries: Vec<(&String, &Value)> = catalog.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    let mut options: Vec<Value> = Vec::with_capacity(entries.len() + 1);
    let mut current_listed = false;
    for (alias, entry) in &entries {
        current_listed |= alias.as_str() == current;
        let name = entry
            .get("displayName")
            .and_then(Value::as_str)
            .unwrap_or(alias);
        options.push(json!({"value": alias, "name": name}));
    }
    if !current_listed {
        options.push(json!({"value": current, "name": current}));
    }
    json!(options)
}

/// The `thinking` arm of the `configOptions` select: `none` first, then
/// one option per effort the current model declares in `supportEfforts`
/// (the engine's open vocabulary — a model-declared `max` must be
/// pickable in the host UI, not just displayable as a currentValue). A
/// model with no (or an empty) declaration falls back to the four
/// canonical levels — the closed-vocabulary surface undeclared models
/// have always served.
fn acp_thinking_select_options(declared_efforts: &[String]) -> Value {
    if declared_efforts.is_empty() {
        return json!([
            {"value": "none", "name": "Thinking None"},
            {"value": "low", "name": "Thinking Low"},
            {"value": "medium", "name": "Thinking Medium"},
            {"value": "high", "name": "Thinking High"},
        ]);
    }
    let mut options = vec![json!({"value": "none", "name": "Thinking None"})];
    for effort in declared_efforts {
        let mut chars = effort.chars();
        let name = match chars.next() {
            Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
            None => String::new(),
        };
        options.push(json!({
            "value": effort,
            "name": format!("Thinking {name}"),
        }));
    }
    json!(options)
}

/// Whether the wire string is one of the four locked ACP mode ids.
fn is_acp_mode_id(value: &str) -> bool {
    matches!(value, "default" | "plan" | "auto" | "yolo")
}

/// The engine permission mode an ACP mode id maps onto (TS
/// `acpModeToToggles` parity: only `auto`/`yolo` carry a non-manual
/// permission; `plan` keeps manual approvals and adds the plan gate).
fn permission_for_mode_id(mode_id: &str) -> &'static str {
    match mode_id {
        "auto" => "auto",
        "yolo" => "yolo",
        _ => "manual",
    }
}

/// Project a persisted context history (`session.getHistory`'s serialized
/// `ContextMessage` array) into the ordered `session/update` payload
/// sequence a `session/load` replays (TS `projectHistoryToSessionUpdates`
/// parity):
///
/// - `user` messages: each non-empty text part → `user_message_chunk`;
/// - `assistant` messages: text parts → `agent_message_chunk`, `think`
///   parts → `agent_thought_chunk`, and each tool call → a `tool_call`
///   (the same CREATE shape the live event translation emits — tool
///   name as the title, `kind` derived from the name (F-P5d-2), and
///   `rawInput` / `content` from the persisted call arguments when the
///   history carries them);
/// - `tool` messages: the terminal `tool_call_update` (completed /
///   failed through the `is_error` flag, the text content surfaced as
///   tool-call content blocks, the calling tool's `name` attached when
///   the history slice saw its CREATE) — correlated through the tool
///   call id, so a tool result whose call is not in the history slice
///   is skipped rather than orphaning the client's card;
/// - every other role (system, unknown) has no ACP analogue and is
///   skipped.
///
/// Tool call ids flow through verbatim (the engine's ids are
/// session-globally unique — no turn prefixing, matching the live
/// translation).
fn project_history_to_updates(history: &[Value]) -> Vec<Value> {
    let mut updates = Vec::new();
    let mut seen_tool_calls: HashMap<String, String> = HashMap::new();
    for message in history {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let content = message
            .get("content")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        match role {
            "user" => {
                // Engine-injected system reminders (tools_added, background
                // task notices, …) ride user-role rows tagged
                // `origin.kind == "injection"`; they are context management,
                // not user input, and must not replay as user_message_chunk.
                if message.pointer("/origin/kind").and_then(Value::as_str) == Some("injection") {
                    continue;
                }
                for part in content {
                    if part.get("type").and_then(Value::as_str) == Some("text") {
                        let text = part.get("text").and_then(Value::as_str).unwrap_or_default();
                        if !text.is_empty() {
                            updates.push(json!({
                                "sessionUpdate": "user_message_chunk",
                                "content": {"type": "text", "text": text},
                            }));
                        }
                    }
                }
            }
            "assistant" => {
                for part in content {
                    let part_type = part.get("type").and_then(Value::as_str);
                    let text_key = if part_type == Some("think") {
                        "think"
                    } else {
                        "text"
                    };
                    if matches!(part_type, Some("text") | Some("think")) {
                        let text = part
                            .get(text_key)
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        if !text.is_empty() {
                            let literal = if part_type == Some("think") {
                                "agent_thought_chunk"
                            } else {
                                "agent_message_chunk"
                            };
                            updates.push(json!({
                                "sessionUpdate": literal,
                                "content": {"type": "text", "text": text},
                            }));
                        }
                    }
                }
                for tool_call in message
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or_default()
                {
                    let tool_call_id = tool_call.get("id").and_then(Value::as_str);
                    let Some(tool_call_id) = tool_call_id else {
                        continue;
                    };
                    let tool_name = tool_call
                        .pointer("/function/name")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    // The persisted arguments (an opaque string on the
                    // engine's wire) back the CREATE's `rawInput` and the
                    // args-preview `content` (G7 replay parity with the
                    // live translation).
                    let arguments = tool_call
                        .pointer("/function/arguments")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let mut create = json!({
                        "sessionUpdate": "tool_call",
                        "toolCallId": tool_call_id,
                        "title": tool_name,
                        "kind": infer_tool_kind(tool_name),
                        "status": "in_progress",
                    });
                    if !arguments.is_empty() {
                        create["rawInput"] = json!(arguments);
                        create["content"] = json!([{
                            "type": "content",
                            "content": {"type": "text", "text": arguments},
                        }]);
                    }
                    updates.push(create);
                    seen_tool_calls.insert(tool_call_id.to_string(), tool_name.to_string());
                }
            }
            "tool" => {
                let tool_call_id = message.get("tool_call_id").and_then(Value::as_str);
                let Some(tool_call_id) = tool_call_id else {
                    // A tool result with no correlation id cannot be
                    // attached to a card; skip rather than crash.
                    continue;
                };
                let Some(tool_name) = seen_tool_calls.get(tool_call_id) else {
                    // The matching assistant message is not in this
                    // history slice (compaction); an update for a tool
                    // call the client never saw would orphan the card.
                    continue;
                };
                let is_error = message
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let mut blocks = Vec::new();
                for part in content {
                    if part.get("type").and_then(Value::as_str) == Some("text")
                        && let Some(text) = part.get("text").and_then(Value::as_str)
                    {
                        if !text.is_empty() {
                            blocks.push(json!({
                                "type": "content",
                                "content": {"type": "text", "text": text},
                            }));
                        }
                        continue;
                    }
                    // Media parts become placeholders so the result card
                    // is not empty (TS parity).
                    let part_type = part.get("type").and_then(Value::as_str).unwrap_or("part");
                    blocks.push(json!({
                        "type": "content",
                        "content": {"type": "text", "text": format!("[{part_type}]")},
                    }));
                }
                updates.push(json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": tool_call_id,
                    "name": tool_name,
                    "status": if is_error { "failed" } else { "completed" },
                    "content": blocks,
                }));
            }
            _ => {}
        }
    }
    updates
}

/// Build the `initialize` result, negotiating the protocol version.
///
/// ACP version negotiation: the server responds with the highest protocol
/// version both sides support. This host implements only version 1, so any
/// client version ≥ 1 is clamped down to 1; a missing or malformed version
/// defaults to 1 as well.
///
/// The capability surface is the honest one (the batch's theme):
///
/// - `promptCapabilities.image: true` — the prompt conversion really
///   converts image blocks (`prompt_content`); `audio: false` (dropped
///   loudly) and `embeddedContext: true` (text resources wrapped, blobs
///   warn-dropped — the TS posture);
/// - `sessionCapabilities.delete: {}` — `session/delete` really deletes
///   (wire + persisted files);
/// - the UNSTABLE `fork` capability moved off the stable
///   `sessionCapabilities` surface into the response's `_meta` (G19): v1
///   marks `session/fork` unstable, so the stable surface stays free of
///   unstable promises while `_meta` (the reserved extensibility channel)
///   keeps the advertisement discoverable — `session/fork` itself is
///   served exactly as before.
fn initialize_result(params: &Value) -> Value {
    let protocol_version = match params.get("protocolVersion").and_then(Value::as_i64) {
        Some(client_version) if (1..=SUPPORTED_PROTOCOL_VERSION).contains(&client_version) => {
            client_version
        }
        _ => SUPPORTED_PROTOCOL_VERSION,
    };

    json!({
        "protocolVersion": protocol_version,
        "agentCapabilities": {
            "loadSession": true,
            "promptCapabilities": {"image": true, "audio": false, "embeddedContext": true},
            "mcpCapabilities": {"http": true, "sse": true},
            // sessionCapabilities values are empty objects, not booleans.
            "sessionCapabilities": {"list": {}, "resume": {}, "close": {}, "delete": {}}
        },
        // The UNSTABLE fork capability lives here, off the stable
        // `sessionCapabilities` surface (G19): `_meta` is the reserved
        // extensibility channel; the `session/fork` method is served
        // unchanged.
        "_meta": {"unstableSessionCapabilities": {"fork": {}}},
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn should_negotiate_protocol_version_1_when_client_sends_higher_version() {
        let handler = AcpHandler::new();
        let result = handler
            .handle("initialize", json!({"protocolVersion": 99}))
            .await
            .unwrap();
        assert_eq!(result["protocolVersion"], 1);
    }

    #[tokio::test]
    async fn should_respond_with_supported_capabilities_on_initialize() {
        let handler = AcpHandler::new();
        let result = handler
            .handle("initialize", json!({"protocolVersion": 1}))
            .await
            .unwrap();
        assert_eq!(result["protocolVersion"], 1);
        assert_eq!(result["agentCapabilities"]["loadSession"], true);
        assert_eq!(
            result["agentCapabilities"]["promptCapabilities"],
            json!({"image": true, "audio": false, "embeddedContext": true})
        );
        assert_eq!(
            result["agentCapabilities"]["mcpCapabilities"],
            json!({"http": true, "sse": true})
        );
        assert_eq!(
            result["agentCapabilities"]["sessionCapabilities"],
            json!({"list": {}, "resume": {}, "close": {}, "delete": {}}),
            "sessionCapabilities values must be empty objects, not booleans"
        );
        // The UNSTABLE fork capability is advertised off the stable
        // sessionCapabilities surface — under the reserved `_meta`
        // extensibility channel (G19).
        assert_eq!(
            result["_meta"]["unstableSessionCapabilities"],
            json!({"fork": {}})
        );
    }

    #[tokio::test]
    async fn should_default_to_protocol_version_1_when_client_omits_version() {
        let handler = AcpHandler::new();
        let result = handler.handle("initialize", json!({})).await.unwrap();
        assert_eq!(result["protocolVersion"], 1);
    }

    #[tokio::test]
    async fn should_reject_session_new_when_called_before_initialize() {
        let handler = AcpHandler::new();
        let (code, message) = handler.handle("session/new", json!({})).await.unwrap_err();
        assert_eq!(code, ERROR_NOT_INITIALIZED);
        assert!(!message.is_empty());
    }

    #[tokio::test]
    async fn should_accept_session_prompt_when_initialize_was_negotiated() {
        let handler = AcpHandler::new();
        handler
            .handle("initialize", json!({"protocolVersion": 1}))
            .await
            .unwrap();
        let (code, message) = handler
            .handle(
                "session/prompt",
                json!({"sessionId": "bogus-session", "prompt": "hi"}),
            )
            .await
            .unwrap_err();
        assert_eq!(code, ERROR_SESSION_NOT_FOUND);
        assert!(message.contains("bogus-session"));
    }

    #[tokio::test]
    async fn should_reject_session_prompt_when_session_id_is_missing() {
        let handler = AcpHandler::new();
        handler
            .handle("initialize", json!({"protocolVersion": 1}))
            .await
            .unwrap();
        let (code, _) = handler
            .handle("session/prompt", json!({"prompt": "hi"}))
            .await
            .unwrap_err();
        assert_eq!(code, ERROR_INVALID_PARAMS);
    }

    #[tokio::test]
    async fn should_return_method_not_found_for_unknown_methods() {
        let handler = AcpHandler::new();
        let (code, message) = handler
            .handle("unknown/method", json!({}))
            .await
            .unwrap_err();
        assert_eq!(code, ERROR_METHOD_NOT_FOUND);
        assert!(message.contains("unknown/method"));
    }

    #[tokio::test]
    async fn should_translate_assistant_delta_into_agent_message_chunk_update() {
        // Given an engine event notification carrying an assistant delta
        let notification = JsonRpcNotification {
            method: "event.assistant.delta".to_string(),
            params: json!({
                "sessionId": "session-1",
                "volatile": true,
                "agentId": "agent-1",
                "delta": "Hello",
            }),
        };

        // When it is translated
        let update = EventTranslator::default().translate(&notification).unwrap();

        // Then it becomes an ACP session/update notification carrying an
        // agent_message_chunk update with a text ContentBlock
        assert_eq!(update.method, "session/update");
        assert_eq!(update.params["sessionId"], "session-1");
        assert_eq!(
            update.params["update"],
            json!({
                "sessionUpdate": "agent_message_chunk",
                "content": {"type": "text", "text": "Hello"},
            })
        );
    }

    #[tokio::test]
    async fn should_translate_thinking_delta_into_agent_thought_chunk_update() {
        // Given an engine event notification carrying a thinking delta
        let notification = engine_event(
            "event.thinking.delta",
            json!({"agentId": "agent-1", "delta": "Reasoning"}),
        );

        // When it is translated
        let update = EventTranslator::default().translate(&notification).unwrap();

        // Then it becomes an agent_thought_chunk update with the same
        // ContentBlock shape as a message chunk
        assert_eq!(update.method, "session/update");
        assert_eq!(
            update.params["update"],
            json!({
                "sessionUpdate": "agent_thought_chunk",
                "content": {"type": "text", "text": "Reasoning"},
            })
        );
    }

    #[tokio::test]
    async fn should_not_translate_non_chunk_events_when_translating_engine_events() {
        // Given an engine event notification without an ACP counterpart
        let notification = JsonRpcNotification {
            method: "event.message.received".to_string(),
            params: json!({
                "sessionId": "session-1",
                "role": "user",
                "contentSummary": "hi",
            }),
        };

        // When it is translated
        let update = EventTranslator::default().translate(&notification);

        // Then there is no ACP update for it
        assert!(update.is_none());
    }

    /// Build an engine `event.<type>` notification with the given flat
    /// payload fields, addressed to `session-1`.
    fn engine_event(method: &str, payload: Value) -> JsonRpcNotification {
        let mut params = json!({ "sessionId": "session-1" });
        if let Value::Object(payload) = payload {
            for (key, value) in payload {
                params[&key] = value;
            }
        }
        JsonRpcNotification {
            method: method.to_string(),
            params,
        }
    }

    #[tokio::test]
    async fn should_not_translate_turn_bookkeeping_events_when_translating_engine_events() {
        // Given engine turn bookkeeping events (v1 has no turn-boundary
        // update: the turn settles through the session/prompt response)
        for notification in [
            engine_event("event.turn.started", json!({"turnId": "turn-1"})),
            engine_event(
                "event.turn.ended",
                json!({"turnId": "turn-1", "reason": "completed"}),
            ),
            engine_event(
                "event.turn.ended",
                json!({
                    "turnId": "turn-1",
                    "reason": "failed",
                    "error": {"code": "internal", "message": "boom", "retryable": false},
                }),
            ),
        ] {
            // When they are translated
            // Then there is no ACP update for any of them
            assert!(
                EventTranslator::default()
                    .translate(&notification)
                    .is_none(),
                "turn bookkeeping has no v1 update counterpart: {}",
                notification.params
            );
        }
    }

    #[tokio::test]
    async fn should_translate_tool_call_started_into_tool_call_create_update() {
        // Given an engine tool.call.started event
        let notification = engine_event(
            "event.tool.call.started",
            json!({"name": "Read", "toolCallId": "call-1"}),
        );

        // When it is translated
        let update = EventTranslator::default().translate(&notification).unwrap();

        // Then it becomes a tool_call CREATE update: the v1 shape with the
        // tool name as the (required, non-empty) title, execution announced
        // as in_progress, the kind derived from the tool name (G7), and no
        // rawInput/content (no argument fragments streamed before the
        // dispatch)
        assert_eq!(
            update.params["update"],
            json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call-1",
                "title": "Read",
                "kind": "read",
                "status": "in_progress",
            })
        );
    }

    #[tokio::test]
    async fn should_translate_tool_result_into_completed_tool_call_update() {
        // Given an engine tool.result event for a successful call
        let notification = engine_event(
            "event.tool.result",
            json!({"toolCallId": "call-1", "isError": false}),
        );

        // When it is translated
        let update = EventTranslator::default().translate(&notification).unwrap();

        // Then it becomes a terminal tool_call_update marking the call
        // completed (the projection carries no output, so content stays
        // omitted; the name stays omitted too — no projection event
        // carried it)
        assert_eq!(
            update.params["update"],
            json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call-1",
                "status": "completed",
            })
        );
    }

    #[tokio::test]
    async fn should_carry_the_wire_name_on_a_tool_result_with_no_tracked_started_event() {
        // Given an engine tool.result event whose started event the
        // translator never saw (a mid-stream join — no tracked entry) but
        // whose wire payload carries the tool name (the field-level
        // alignment)
        let notification = engine_event(
            "event.tool.result",
            json!({"toolCallId": "call-9", "name": "Bash", "isError": false}),
        );

        // When it is translated
        let update = EventTranslator::default().translate(&notification).unwrap();

        // Then the terminal update carries the name from the wire (the
        // tracked map is empty — the fallback keeps the client's tool card
        // attributed)
        assert_eq!(
            update.params["update"]["name"],
            json!("Bash"),
            "the wire name is the fallback: {}",
            update.params["update"]
        );
    }

    #[tokio::test]
    async fn should_carry_the_wire_args_as_raw_input_when_nothing_streamed() {
        // Given an engine tool.call.started event carrying the parsed
        // argument object (the field-level alignment) with no argument
        // fragments streamed before it
        let notification = engine_event(
            "event.tool.call.started",
            json!({
                "name": "Bash",
                "toolCallId": "call-2",
                "args": {"command": "cargo test"}
            }),
        );

        // When it is translated
        let update = EventTranslator::default().translate(&notification).unwrap();

        // Then the tool_call create update carries the arguments as the
        // official v1 `rawInput` string (non-streaming providers emit no
        // tool.call.delta, so without this fallback their ACP tool cards
        // would carry no input at all)
        assert_eq!(
            update.params["update"]["rawInput"],
            json!(r#"{"command":"cargo test"}"#),
            "rawInput carries the compact JSON of the wire args: {}",
            update.params["update"]
        );
        assert_eq!(
            update.params["update"]["content"],
            json!([{
                "type": "content",
                "content": {"type": "text", "text": r#"{"command":"cargo test"}"#},
            }]),
            "content mirrors rawInput: {}",
            update.params["update"]
        );
    }

    #[tokio::test]
    async fn should_translate_failed_tool_result_into_failed_tool_call_update() {
        // Given an engine tool.result event for a failed call
        let notification = engine_event(
            "event.tool.result",
            json!({"toolCallId": "call-1", "isError": true}),
        );

        // When it is translated
        let update = EventTranslator::default().translate(&notification).unwrap();

        // Then the terminal update marks the call failed (v1
        // ToolCallStatus carries a failed variant)
        assert_eq!(
            update.params["update"],
            json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call-1",
                "status": "failed",
            })
        );
    }

    #[tokio::test]
    async fn should_translate_status_tool_progress_into_title_refresh_update() {
        // Given an engine tool.progress event with a status update carrying
        // non-empty text
        let notification = engine_event(
            "event.tool.progress",
            json!({"toolCallId": "call-1", "update": {"kind": "status", "text": "working"}}),
        );

        // When it is translated
        let update = EventTranslator::default().translate(&notification).unwrap();

        // Then it becomes a tool_call_update refreshing the card title
        assert_eq!(
            update.params["update"],
            json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call-1",
                "title": "working",
            })
        );
    }

    #[tokio::test]
    async fn should_not_translate_non_status_tool_progress_when_translating_engine_events() {
        // Given tool.progress events whose updates are not status-kind (or
        // carry no text) — stdout chunks and progress percentages have no
        // v1 counterpart
        for notification in [
            engine_event(
                "event.tool.progress",
                json!({"toolCallId": "call-1", "update": {"kind": "stdout", "text": "noise"}}),
            ),
            engine_event(
                "event.tool.progress",
                json!({"toolCallId": "call-1", "update": {"kind": "progress", "percent": 42.0}}),
            ),
            engine_event(
                "event.tool.progress",
                json!({"toolCallId": "call-1", "update": {"kind": "status", "text": ""}}),
            ),
        ] {
            // When they are translated
            // Then there is no ACP update for any of them
            assert!(
                EventTranslator::default()
                    .translate(&notification)
                    .is_none(),
                "only status-kind progress maps onto the wire: {}",
                notification.params
            );
        }
    }

    #[tokio::test]
    async fn should_attach_streamed_arguments_to_the_tool_call_create_and_result_updates() {
        // Given the engine's volatile argument fragments streamed ahead of
        // the dispatch (the tool.call.delta projections arrive before the
        // started event) and a result that follows — G7's content-bearing
        // projection, driven through one translator like the forwarder
        // does
        let mut translator = EventTranslator::default();

        // When the fragments arrive first, they hold no wire update of
        // their own (pushing args before the CREATE would orphan the
        // client's tool card — v1's ToolCallUpdate.content is REPLACE
        // semantics)
        assert!(
            translator
                .translate(&engine_event(
                    "event.tool.call.delta",
                    json!({
                        "toolCallId": "call-1",
                        "name": "Read",
                        "argumentsPart": "{\"path\":",
                    }),
                ))
                .is_none()
        );
        assert!(
            translator
                .translate(&engine_event(
                    "event.tool.call.delta",
                    json!({"toolCallId": "call-1", "argumentsPart": " \"a.rs\"}"}),
                ))
                .is_none()
        );

        // The CREATE then carries the accumulated arguments (rawInput and
        // a text content block) plus the name-derived kind
        let created = translator
            .translate(&engine_event(
                "event.tool.call.started",
                json!({"name": "Read", "toolCallId": "call-1"}),
            ))
            .unwrap();
        assert_eq!(
            created.params["update"],
            json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "call-1",
                "title": "Read",
                "kind": "read",
                "status": "in_progress",
                "rawInput": "{\"path\": \"a.rs\"}",
                "content": [{
                    "type": "content",
                    "content": {"type": "text", "text": "{\"path\": \"a.rs\"}"},
                }],
            })
        );

        // And the terminal update carries the tool's name (the unstable
        // ToolCallUpdate field — the client correlates the result with the
        // tool that produced it)
        let settled = translator
            .translate(&engine_event(
                "event.tool.result",
                json!({"toolCallId": "call-1", "isError": false}),
            ))
            .unwrap();
        assert_eq!(
            settled.params["update"],
            json!({
                "sessionUpdate": "tool_call_update",
                "toolCallId": "call-1",
                "status": "completed",
                "name": "Read",
            })
        );
    }

    #[tokio::test]
    async fn should_admit_session_new_without_an_auth_gate_when_no_credential_is_present() {
        // Given an initialized handler and an engine that stores no
        // credential (the managed-login gate no longer exists — sessions
        // start on plain per-provider credentials)
        let handler = AcpHandler::with_engine(Arc::new(ProbeEngine::default()));
        handler
            .handle("initialize", json!({"protocolVersion": 1}))
            .await
            .unwrap();

        // When a session is created
        let (code, message) = handler.handle("session/new", json!({})).await.unwrap_err();

        // Then the request proceeds into the engine (the probe answers
        // session.create with an empty object, so the honest failure that
        // follows is the missing session id — never an auth gate)
        assert_eq!(code, ERROR_INTERNAL);
        assert!(
            message.contains("session.create returned no session id"),
            "{message}"
        );
    }

    #[tokio::test]
    async fn should_answer_method_not_found_when_authenticate_is_called_after_the_login_removal() {
        // Given an initialized handler (the ACP authenticate method left
        // with the managed-login surface)
        let handler = AcpHandler::new();
        handler
            .handle("initialize", json!({"protocolVersion": 1}))
            .await
            .unwrap();

        // When the removed authenticate method is called
        let (code, _) = handler
            .handle("authenticate", json!({"methodId": "login"}))
            .await
            .unwrap_err();

        // Then it answers the well-formed method-not-found error
        assert_eq!(code, ERROR_METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn should_answer_method_not_found_when_logout_is_called_after_the_login_removal() {
        // Given an initialized handler
        let handler = AcpHandler::new();
        handler
            .handle("initialize", json!({"protocolVersion": 1}))
            .await
            .unwrap();

        // When the removed logout method is called
        let (code, _) = handler.handle("logout", json!({})).await.unwrap_err();

        // Then it answers the well-formed method-not-found error
        assert_eq!(code, ERROR_METHOD_NOT_FOUND);
    }

    // ------------------------------------------------------------------
    // Replaced engine slot (design 14 D3 / §2 "引擎替换接缝")
    // ------------------------------------------------------------------

    /// Given a handler whose default engine is remote and whose probe
    /// path yields a fresh engine, when the connection dies and the
    /// re-probe succeeds, then the new engine takes the slot, the route
    /// caches are cleared (a session remembered on the dead engine routes
    /// to the new one), and the dead engine is never wired again.
    #[tokio::test]
    async fn should_replace_the_engine_slot_and_clear_route_caches_when_reconnected() {
        // Given a session created on the dead engine
        let dead = Arc::new(ScriptedEngine::default().say("session.create", "sess-1"));
        let dead_engine: Arc<dyn EngineAdapter> = dead.clone();
        let handler = AcpHandler::with_engine(Arc::clone(&dead_engine));
        handler
            .handle("initialize", json!({"protocolVersion": 1}))
            .await
            .unwrap();
        handler.handle("session/new", json!({})).await.unwrap();
        // A replacement engine with a distinct session.create answer.
        let replacement = Arc::new(ScriptedEngine::default().say("session.create", "sess-2"));
        let replacement_engine: Arc<dyn EngineAdapter> = replacement.clone();
        handler.replace_engine(Arc::clone(&replacement_engine));

        // When the remembered session is addressed (the route cache must
        // point at the NEW engine — the old entry named the dead one)
        let result = handler
            .handle("session/close", json!({"sessionId": "sess-1"}))
            .await
            .unwrap();

        // Then the new engine served it (session.close reached the
        // replacement, and its session.create answer proves the slot
        // holds it for new sessions too)
        assert_eq!(result, json!({}));
        assert_eq!(replacement.closed_sessions(), vec!["sess-1"]);
        let created = handler.handle("session/new", json!({})).await.unwrap();
        assert_eq!(created["sessionId"], json!("sess-2"));
        // And the dead engine saw no traffic after the replacement.
        assert!(dead.closed_sessions().is_empty());
    }

    /// Given a remote-mode handler whose engine died with a transport
    /// error and whose probe path ALWAYS fails (counting invocations),
    /// when further session-addressed calls arrive, then the probe path
    /// ran exactly once (the degrade attempt — design 14 D3: the degrade
    /// is one-way, no re-probe after it) and the new-session calls are
    /// served by the embedded engine.
    #[tokio::test]
    async fn should_probe_only_once_after_degrading_and_never_reprobe_again() {
        // Given a remote-mode handler over a dead engine (every engine
        // call answers a transport error, the shape RemoteAdapter
        // surfaces after connection death)
        let dead: Arc<dyn EngineAdapter> = Arc::new(ScriptedEngine::default().fail_transport());
        let handler = AcpHandler::with_engine(Arc::clone(&dead));
        handler
            .handle("initialize", json!({"protocolVersion": 1}))
            .await
            .unwrap();
        // A probe/connect path that always fails, counting invocations.
        let probe_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let embedded_minted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let probe_calls_for_path = Arc::clone(&probe_calls);
        let probe_path: ProbePath = Arc::new(move || {
            probe_calls_for_path.fetch_add(1, Ordering::SeqCst);
            None
        });
        let embedded_minted_for_factory = Arc::clone(&embedded_minted);
        let embedded_factory: EmbeddedEngineFactory = Arc::new(move || {
            embedded_minted_for_factory.fetch_add(1, Ordering::SeqCst);
            Arc::new(ScriptedEngine::default().say("session.create", "embedded-sess"))
                as Arc<dyn EngineAdapter>
        });
        handler.set_engine_replacement_path(probe_path, embedded_factory);

        // When the first engine call fails with a transport error and
        // the handler degrades, then two further session/new calls land
        // on the embedded engine
        let first = handler.handle("session/new", json!({})).await.unwrap_err();
        assert_eq!(first.0, ERROR_INTERNAL);
        let second = handler.handle("session/new", json!({})).await.unwrap();
        assert_eq!(second["sessionId"], json!("embedded-sess"));
        let third = handler.handle("session/new", json!({})).await.unwrap();
        assert_eq!(third["sessionId"], json!("embedded-sess"));

        // Then the probe path ran exactly once (the degrade attempt —
        // never re-probed after the degrade) and the embedded engine was
        // minted exactly once (the slot keeps serving it).
        assert_eq!(probe_calls.load(Ordering::SeqCst), 1);
        assert_eq!(embedded_minted.load(Ordering::SeqCst), 1);
    }

    /// Given a handler that knows its engine's root and a session
    /// directory the engine persisted there — and an engine WITHOUT the
    /// retention.delete route (the legacy-engine degrade shape: M4's
    /// unified-executor convergence only lands when the engine
    /// implements it), when the session is deleted, then the persisted
    /// files are removed.
    #[tokio::test]
    async fn should_remove_the_persisted_session_files_on_session_delete() {
        // Given a handler that knows its engine's root and a session
        // directory the engine persisted there — and an engine WITHOUT
        // the retention.delete route (the legacy-engine degrade shape:
        // M4's unified-executor convergence only lands when the engine
        // implements it)
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp
            .path()
            .join(".mirri")
            .join("v3_sessions")
            .join("session-1");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(session_dir.join("state.json"), "{}").unwrap();
        let probe = Arc::new(ProbeEngine::default());
        probe
            .retention_unsupported
            .lock()
            .unwrap()
            .insert("session-1".to_string());
        let engine: Arc<dyn EngineAdapter> = probe.clone();
        let handler = AcpHandler::with_engine(Arc::clone(&engine));
        handler.remember_engine_cwd(&engine, tmp.path().to_path_buf());
        handler
            .handle("initialize", json!({"protocolVersion": 1}))
            .await
            .unwrap();

        // When the session is deleted
        let result = handler
            .handle("session/delete", json!({"sessionId": "session-1"}))
            .await
            .unwrap();

        // Then the engine archived it, the engine's retention.delete was
        // ATTEMPTED (the convergence call), and — the route being absent —
        // the legacy local removal removed the persisted files (real
        // removal, never a fake success)
        assert_eq!(result, json!({}));
        assert_eq!(
            probe.retention_deleted.lock().unwrap().as_slice(),
            ["session-1".to_string()].as_slice(),
            "the unified delete must be attempted through the engine first"
        );
        assert!(
            !session_dir.exists(),
            "the persisted session directory must be removed"
        );
    }

    /// Given an engine that IMPLEMENTS retention.delete (the modern
    /// engine), when the ACP session/delete runs, then the deletion
    /// converges through the engine's unified executor — the ACP host
    /// performs NO local removal of its own (the engine owns the lock,
    /// the blob-ledger decrement, and the directory/index order).
    #[tokio::test]
    async fn should_delete_through_the_engine_unified_executor_when_the_route_exists() {
        // Given a session directory still on disk (the engine "failed to
        // remove it" — the probe never touches disk) and an engine whose
        // retention.delete answers ok
        let tmp = tempfile::tempdir().unwrap();
        let session_dir = tmp
            .path()
            .join(".mirri")
            .join("v3_sessions")
            .join("session-2");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(session_dir.join("state.json"), "{}").unwrap();
        let probe = Arc::new(ProbeEngine::default());
        let engine: Arc<dyn EngineAdapter> = probe.clone();
        let handler = AcpHandler::with_engine(Arc::clone(&engine));
        handler.remember_engine_cwd(&engine, tmp.path().to_path_buf());
        handler
            .handle("initialize", json!({"protocolVersion": 1}))
            .await
            .unwrap();

        // When the session is deleted
        let result = handler
            .handle("session/delete", json!({"sessionId": "session-2"}))
            .await
            .unwrap();

        // Then the engine's unified executor handled it (the observed
        // call), the answer is ok — and the ACP host did NOT remove the
        // directory itself (the probe's stand-in for "the engine's
        // executor owns the removal": the file outliving the call proves
        // no local remove_dir_all ran)
        assert_eq!(result, json!({}));
        assert_eq!(
            probe.retention_deleted.lock().unwrap().as_slice(),
            ["session-2".to_string()].as_slice()
        );
        assert!(
            session_dir.exists(),
            "the ACP host must not remove the files itself when the engine \
             implements the unified delete"
        );
    }

    #[tokio::test]
    async fn should_list_persisted_sessions_with_the_v1_session_info_shape() {
        // Given a handler that knows its engine's root and an engine that
        // reports a persisted-only (closed) session
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let probe = Arc::new(ProbeEngine {
            session_views: vec![json!({
                "id": "session-1",
                "title": "Past work",
                "created_at": 100,
                "updated_at": 200,
                "agent_count": 3,
                "tags": ["a", "b"],
                "status": "closed",
            })],
            ..ProbeEngine::default()
        });
        let handler = AcpHandler::with_engine(probe.clone());
        let engine: Arc<dyn EngineAdapter> = probe.clone();
        handler.remember_engine_cwd(&engine, root.clone());
        handler
            .handle("initialize", json!({"protocolVersion": 1}))
            .await
            .unwrap();

        // When the sessions are listed
        let result = handler.handle("session/list", json!({})).await.unwrap();

        // Then the persisted-only session appears (G9: the listing is the
        // engine's full view, never self-filtered down to running ones)
        // with the official v1 SessionInfo shape and the engine-side
        // extras under the reserved `_meta` channel
        assert_eq!(
            result["sessions"],
            json!([{
                "sessionId": "session-1",
                "cwd": root.to_string_lossy(),
                "title": "Past work",
                "updatedAt": 200,
                "_meta": {
                    "createdAt": 100,
                    "agentCount": 3,
                    "tags": ["a", "b"],
                    "status": "closed",
                },
            }])
        );
        assert_eq!(result["nextCursor"], Value::Null);
    }

    #[tokio::test]
    async fn should_filter_the_session_list_by_the_request_cwd() {
        // Given a handler that knows its engine's root and an engine with
        // one session
        let tmp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let probe = Arc::new(ProbeEngine {
            session_views: vec![json!({"id": "session-1", "status": "running"})],
            ..ProbeEngine::default()
        });
        let handler = AcpHandler::with_engine(probe.clone());
        let engine: Arc<dyn EngineAdapter> = probe.clone();
        handler.remember_engine_cwd(&engine, root);
        handler
            .handle("initialize", json!({"protocolVersion": 1}))
            .await
            .unwrap();

        // When the list is filtered by a symlinked spelling of the
        // engine's root, the session survives (the canonicalized compare
        // matches); filtered by another directory, it does not
        let matched = handler
            .handle("session/list", json!({"cwd": tmp.path().to_string_lossy()}))
            .await
            .unwrap();
        assert_eq!(matched["sessions"].as_array().unwrap().len(), 1);

        let other = tempfile::tempdir().unwrap();
        let filtered = handler
            .handle(
                "session/list",
                json!({"cwd": other.path().to_string_lossy()}),
            )
            .await
            .unwrap();
        assert_eq!(filtered["sessions"].as_array().unwrap().len(), 0);
    }

    /// A remote-shaped test handler: the engine slot holds a
    /// `is_remote() == true` probe whose `workspace.list` answers the
    /// given rows, and the host start root is recorded the way the serve
    /// loop records it (`remember_engine_cwd` on the slot engine).
    async fn remote_handler_with_workspaces(
        rows: Vec<(String, String)>,
        host_root: &Path,
    ) -> (AcpHandler, Arc<ProbeEngine>) {
        let probe = Arc::new(ProbeEngine {
            workspace_views: rows
                .into_iter()
                .map(|(id, root)| json!({"id": id, "root": root}))
                .collect(),
            create_id: Some("probe-session".to_string()),
            remote: true,
            ..ProbeEngine::default()
        });
        let engine: Arc<dyn EngineAdapter> = probe.clone();
        let handler = AcpHandler::with_remote_engine(Arc::clone(&engine), host_root.to_path_buf());
        handler
            .handle("initialize", json!({"protocolVersion": 1}))
            .await
            .unwrap();
        (handler, probe)
    }

    /// Given a remote-mode handler whose engine registers workspace wd-1
    /// at a lexical root, when `session/new` arrives with that root as
    /// its cwd, then the engine's `session.create` carries the workspace's
    /// id in `workspaceId` (design 14 D2: the ACP-side translation).
    #[tokio::test]
    async fn should_route_session_new_to_the_registered_workspace_of_the_cwd() {
        let root = PathBuf::from("/lexically/resolved/proj");
        let (handler, probe) = remote_handler_with_workspaces(
            vec![("wd-1".to_string(), root.to_string_lossy().into_owned())],
            &root,
        )
        .await;

        handler
            .handle(
                "session/new",
                json!({"cwd": "/lexically/resolved/proj", "title": "t"}),
            )
            .await
            .expect("session/new on a registered root must succeed");

        let calls = probe.create_calls();
        assert_eq!(calls.len(), 1, "exactly one session.create must run");
        assert_eq!(
            calls[0]["workspaceId"],
            json!("wd-1"),
            "the create params must carry the registered workspace id"
        );
    }

    /// Given the same remote-mode handler, when `session/new` arrives with
    /// a cwd that is not a registered workspace, then the response is a
    /// JSON-RPC error whose message says the directory is not registered,
    /// and the engine's `session.create` was never called (design 14 D2:
    /// an unregistered root is rejected, never silently routed to the
    /// server's default workspace — a misrouted sandbox is a correctness
    /// bug, not a cosmetic one).
    #[tokio::test]
    async fn should_reject_session_new_when_the_cwd_is_not_a_registered_workspace() {
        let root = PathBuf::from("/lexically/resolved/proj");
        let (handler, probe) = remote_handler_with_workspaces(
            vec![("wd-1".to_string(), root.to_string_lossy().into_owned())],
            &root,
        )
        .await;

        let (code, message) = handler
            .handle("session/new", json!({"cwd": "/nowhere"}))
            .await
            .expect_err("an unregistered cwd must be rejected");

        assert_eq!(code, ERROR_INVALID_PARAMS);
        assert!(
            message.contains("not a registered workspace"),
            "the error must explain the registration miss: {message}"
        );
        assert!(
            probe.create_calls().is_empty(),
            "session.create must never run for an unregistered cwd"
        );
    }

    /// Given a remote-mode engine holding sessions of two workspaces,
    /// when `session/list` runs, then only the host start root's
    /// workspace's sessions are returned (design 14 D6: US11.4's pinning
    /// rides the ACP host's read-side filter in remote mode — the IDE
    /// lists its own project's sessions, not the whole desktop server's).
    #[tokio::test]
    async fn should_filter_session_list_to_the_host_root_workspace_in_remote_mode() {
        let host_root = PathBuf::from("/host/root");
        let probe = Arc::new(ProbeEngine {
            workspace_views: vec![
                json!({"id": "wd-host", "root": "/host/root"}),
                json!({"id": "wd-other", "root": "/other/root"}),
            ],
            session_views: vec![
                json!({"id": "mine", "cwd": "/host/root"}),
                json!({"id": "theirs", "cwd": "/other/root"}),
            ],
            remote: true,
            ..ProbeEngine::default()
        });
        let engine: Arc<dyn EngineAdapter> = probe.clone();
        let handler = AcpHandler::with_remote_engine(Arc::clone(&engine), host_root.clone());
        handler
            .handle("initialize", json!({"protocolVersion": 1}))
            .await
            .unwrap();
        handler.remember_engine_cwd(&engine, host_root);

        let listed = handler
            .handle("session/list", json!({}))
            .await
            .expect("the remote list must succeed");
        let sessions = listed["sessions"].as_array().unwrap();
        assert_eq!(sessions.len(), 1, "only the host root's sessions");
        assert_eq!(sessions[0]["sessionId"], json!("mine"));
    }

    /// Given a remote-mode `session/new` request carrying `mcpServers`,
    /// when it routes to the registered workspace, then the captured
    /// `session.create` params carry BOTH the overlay entries and the
    /// `workspaceId` (design 14 D5a: the overlay rides the existing
    /// params — no new mechanism, no either/or).
    #[tokio::test]
    async fn should_pass_the_mcp_servers_overlay_through_in_remote_mode() {
        let root = PathBuf::from("/lexically/resolved/proj");
        let (handler, probe) = remote_handler_with_workspaces(
            vec![("wd-1".to_string(), root.to_string_lossy().into_owned())],
            &root,
        )
        .await;

        handler
            .handle(
                "session/new",
                json!({
                    "cwd": "/lexically/resolved/proj",
                    "mcpServers": [
                        {"name": "docs", "type": "http", "url": "https://example.test/mcp"}
                    ]
                }),
            )
            .await
            .expect("session/new with an overlay must succeed");

        let calls = probe.create_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0]["workspaceId"],
            json!("wd-1"),
            "the routing id must survive the overlay-carrying call"
        );
        let overlay = calls[0]["mcpServers"].as_array().expect("overlay entries");
        assert_eq!(overlay.len(), 1, "the one overlay entry survives");
        assert_eq!(overlay[0]["name"], json!("docs"));
        assert_eq!(
            overlay[0]["config"]["transport"]["url"],
            json!("https://example.test/mcp")
        );
    }

    /// Given a remote-mode handler with a live session, when
    /// `session/close` runs, then the engine's unsubscribe happens BEFORE
    /// the wire `session.close` call (design 14 D5: 会话关闭即退订 — the
    /// unsubscribe releases the server-side callback slot first, so the
    /// close cannot land on an adapter the server is already routing
    /// approvals away from).
    #[tokio::test]
    async fn should_unsubscribe_from_the_remote_engine_on_session_close() {
        let root = PathBuf::from("/lexically/resolved/proj");
        let (handler, probe) = remote_handler_with_workspaces(
            vec![("wd-1".to_string(), root.to_string_lossy().into_owned())],
            &root,
        )
        .await;

        handler
            .handle("session/close", json!({"sessionId": "probe-session"}))
            .await
            .expect("session/close in remote mode must succeed");

        let order = probe
            .call_order
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let unsubscribe_at = order
            .iter()
            .position(|entry| entry == "session.unsubscribe:probe-session")
            .expect("the close must first unsubscribe from the session");
        let close_at = order
            .iter()
            .position(|entry| entry == "session.close:probe-session")
            .expect("the close must then call session.close on the engine");
        assert!(
            unsubscribe_at < close_at,
            "unsubscribe must precede the engine close call: {order:?}"
        );
    }

    /// Given an embedded-mode handler (the engine not remote), when
    /// `session/close` runs, then the engine is asked NOTHING but the plain
    /// `session.close` (design 14 D5: the unsubscribe is remote-only — an
    /// embedded engine has no server-side subscription to release, and
    /// asking it for one would be a MethodNotFound error at best).
    #[tokio::test]
    async fn should_only_call_session_close_on_the_engine_in_embedded_mode() {
        let probe = Arc::new(ProbeEngine::default());
        let handler = AcpHandler::with_engine(Arc::clone(&probe) as Arc<dyn EngineAdapter>);
        handler
            .handle("initialize", json!({"protocolVersion": 1}))
            .await
            .unwrap();

        handler
            .handle("session/close", json!({"sessionId": "session-1"}))
            .await
            .expect("session/close in embedded mode must succeed");

        let order = probe
            .call_order
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        assert_eq!(
            order,
            vec!["session.close:session-1".to_string()],
            "the embedded close must be the plain session.close, no unsubscribe"
        );
    }

    #[tokio::test]
    async fn should_reject_set_mode_when_mode_id_is_unknown() {
        // Given an initialized handler
        let handler = AcpHandler::new();
        handler
            .handle("initialize", json!({"protocolVersion": 1}))
            .await
            .unwrap();

        // When set_mode is called with an unknown modeId
        let (code, message) = handler
            .handle(
                "session/set_mode",
                json!({"sessionId": "session-1", "modeId": "whisper"}),
            )
            .await
            .unwrap_err();

        // Then the request is rejected as invalid params
        assert_eq!(code, ERROR_INVALID_PARAMS);
        assert!(message.contains("whisper"));
    }

    #[tokio::test]
    async fn should_reject_set_mode_when_the_pre_v1_mode_param_is_used() {
        // Given an initialized handler
        let handler = AcpHandler::new();
        handler
            .handle("initialize", json!({"protocolVersion": 1}))
            .await
            .unwrap();

        // When set_mode uses the pre-v1 `mode` parameter spelling
        let (code, message) = handler
            .handle(
                "session/set_mode",
                json!({"sessionId": "session-1", "mode": "plan"}),
            )
            .await
            .unwrap_err();

        // Then the request is rejected as invalid params — v1 semantics,
        // no compatibility shim
        assert_eq!(code, ERROR_INVALID_PARAMS);
        assert!(
            message.contains("modeId"),
            "the error must point at the v1 parameter name: {message}"
        );
    }

    #[tokio::test]
    async fn should_reject_set_mode_when_session_id_is_missing() {
        // Given an initialized handler
        let handler = AcpHandler::new();
        handler
            .handle("initialize", json!({"protocolVersion": 1}))
            .await
            .unwrap();

        // When set_mode omits the session id
        let (code, _) = handler
            .handle("session/set_mode", json!({"modeId": "plan"}))
            .await
            .unwrap_err();

        // Then the request is rejected as invalid params
        assert_eq!(code, ERROR_INVALID_PARAMS);
    }

    #[tokio::test]
    async fn should_reject_set_config_option_when_config_id_is_unknown() {
        // Given an initialized handler
        let handler = AcpHandler::new();
        handler
            .handle("initialize", json!({"protocolVersion": 1}))
            .await
            .unwrap();

        // When set_config_option names an unknown option
        let (code, message) = handler
            .handle(
                "session/set_config_option",
                json!({"sessionId": "session-1", "configId": "temperature", "value": 0.7}),
            )
            .await
            .unwrap_err();

        // Then the request is rejected as invalid params
        assert_eq!(code, ERROR_INVALID_PARAMS);
        assert!(message.contains("temperature"));
    }

    #[tokio::test]
    async fn should_reject_set_config_option_when_the_pre_v1_name_param_is_used() {
        // Given an initialized handler
        let handler = AcpHandler::new();
        handler
            .handle("initialize", json!({"protocolVersion": 1}))
            .await
            .unwrap();

        // When set_config_option uses the pre-v1 `name` parameter spelling
        let (code, message) = handler
            .handle(
                "session/set_config_option",
                json!({"sessionId": "session-1", "name": "model", "value": "gpt-4o"}),
            )
            .await
            .unwrap_err();

        // Then the request is rejected as invalid params — v1 semantics,
        // no compatibility shim
        assert_eq!(code, ERROR_INVALID_PARAMS);
        assert!(
            message.contains("configId"),
            "the error must point at the v1 parameter name: {message}"
        );
    }

    #[tokio::test]
    async fn should_reject_set_config_option_when_model_value_is_not_a_string() {
        // Given an initialized handler
        let handler = AcpHandler::new();
        handler
            .handle("initialize", json!({"protocolVersion": 1}))
            .await
            .unwrap();

        // When the model option carries a non-string value
        let (code, _) = handler
            .handle(
                "session/set_config_option",
                json!({"sessionId": "session-1", "configId": "model", "value": 42}),
            )
            .await
            .unwrap_err();

        // Then the request is rejected as invalid params
        assert_eq!(code, ERROR_INVALID_PARAMS);
    }

    #[tokio::test]
    async fn should_reject_session_close_when_called_before_initialize() {
        // Given a fresh, uninitialized handler
        let handler = AcpHandler::new();

        // When a session method is called before initialize
        let (code, _) = handler
            .handle("session/close", json!({"sessionId": "session-1"}))
            .await
            .unwrap_err();

        // Then the state machine rejects it
        assert_eq!(code, ERROR_NOT_INITIALIZED);
    }

    // --- reverse-RPC approval bridge ---------------------------------------

    /// Given an engine approval.ask payload, when it is translated into the
    /// request-permission params, then the official RequestPermissionRequest
    /// shape is built: the addressed session, the tool call scoped to the
    /// engine's tool call id, and the canonical 3-option list.
    #[test]
    fn should_build_request_permission_params_from_an_approval_ask() {
        let ask = json!({
            "tool_name": "Bash",
            "tool_call_id": "call_1",
            "action": "execute Bash",
            "display": {"Command": {"command": "echo hi"}},
            "reason": "approval required",
            "request_id": "9b0f0d64-41a7-4b6e-9a4e-2d2f6c8b1f01",
        });

        let params = approval_ask_to_request_permission("session-1", &ask);

        assert_eq!(params["sessionId"], json!("session-1"));
        assert_eq!(params["toolCall"]["toolCallId"], json!("call_1"));
        assert_eq!(params["toolCall"]["title"], json!("Bash"));
        assert_eq!(
            params["toolCall"]["content"],
            json!([{
                "type": "content",
                "content": {"type": "text", "text": "Requesting approval to execute Bash"},
            }])
        );
        assert_eq!(
            params["options"],
            json!([
                {"optionId": "approve_once", "name": "Approve once", "kind": "allow_once"},
                {
                    "optionId": "approve_always",
                    "name": "Approve for this session",
                    "kind": "allow_always",
                },
                {"optionId": "reject", "name": "Reject", "kind": "reject_once"},
            ]),
            "the canonical options match the TS approval.ts CANONICAL_OPTIONS"
        );
    }

    /// Given the client's permission responses, when they are translated
    /// back, then each outcome maps to its approval decision — approve-once
    /// to a one-shot approval, approve-always to a session-scoped one
    /// (the engine installs a session allow rule), reject / unknown /
    /// cancelled to their deny decisions.
    #[test]
    fn should_map_permission_responses_to_approval_decisions() {
        let selected =
            |option_id: &str| json!({"outcome": {"outcome": "selected", "optionId": option_id}});

        assert_eq!(
            permission_response_to_approval(&selected("approve_once")),
            json!({"decision": "approved"})
        );
        assert_eq!(
            permission_response_to_approval(&selected("approve")),
            json!({"decision": "approved"}),
            "the legacy pre-v1 'approve' optionId maps like approve_once"
        );
        assert_eq!(
            permission_response_to_approval(&selected("approve_always")),
            json!({"decision": "approved", "scope": "session"})
        );
        assert_eq!(
            permission_response_to_approval(&selected("approve_for_session")),
            json!({"decision": "approved", "scope": "session"}),
            "the legacy pre-v1 'approve_for_session' optionId maps like approve_always"
        );
        assert_eq!(
            permission_response_to_approval(&selected("reject")),
            json!({"decision": "rejected"})
        );
        assert_eq!(
            permission_response_to_approval(&selected("totally-unknown")),
            json!({"decision": "rejected"}),
            "an unknown optionId rejects (never approves)"
        );
        assert_eq!(
            permission_response_to_approval(&json!({"outcome": {"outcome": "cancelled"}})),
            json!({"decision": "cancelled"})
        );
        // A malformed reply (no usable outcome) is a rejection too.
        assert_eq!(
            permission_response_to_approval(&json!({})),
            json!({"decision": "rejected"})
        );
    }

    /// Given an engine model catalog with several aliases — one carrying a
    /// `displayName`, one without — when the model select options are
    /// built, then they are sorted by alias key, the `displayName` surfaces
    /// as the option name (the raw key otherwise), and a current model
    /// missing from the catalog is appended at the end.
    #[test]
    fn should_build_model_select_options_from_the_engine_catalog() {
        let catalog = json!({
            "zz-alias": {"provider": "anthropic", "model": "zz-model"},
            "aa-alias": {
                "provider": "anthropic",
                "model": "aa-model",
                "displayName": "AA Display",
            },
            "mm-alias": {"provider": "anthropic", "model": "mm-model"},
        });

        // Current model inside the catalog: no duplicate appended.
        assert_eq!(
            acp_model_select_options(&catalog, "mm-alias"),
            json!([
                {"value": "aa-alias", "name": "AA Display"},
                {"value": "mm-alias", "name": "mm-alias"},
                {"value": "zz-alias", "name": "zz-alias"},
            ])
        );

        // Current model outside the catalog (env-overridden, remote-only):
        // appended at the end so the currentValue stays represented.
        assert_eq!(
            acp_model_select_options(&catalog, "env-override"),
            json!([
                {"value": "aa-alias", "name": "AA Display"},
                {"value": "mm-alias", "name": "mm-alias"},
                {"value": "zz-alias", "name": "zz-alias"},
                {"value": "env-override", "name": "env-override"},
            ])
        );
    }

    /// Given an empty or missing engine model catalog, when the model
    /// select options are built, then the result is the single
    /// current-model option — the pre-catalog behavior.
    #[test]
    fn should_fall_back_to_the_current_model_when_the_catalog_is_empty() {
        let current = json!([{"value": "only-model", "name": "only-model"}]);
        assert_eq!(acp_model_select_options(&json!({}), "only-model"), current);
        assert_eq!(
            acp_model_select_options(&Value::Null, "only-model"),
            current
        );
    }

    /// Given a permission request the client never answered (delivery
    /// failed, timed out, or the client disconnected), when the reply is
    /// folded, then the approval resolves to the rejected fallback — never
    /// an error, never a hang.
    #[tokio::test]
    async fn should_fall_back_to_rejected_when_the_client_reply_fails() {
        let reply = approval_reply_from_client_result(Ok(Err(
            "session/request_permission request timed out after 60000ms".to_string(),
        )));
        assert_eq!(reply["decision"], json!("rejected"));
        assert!(
            reply["feedback"]
                .as_str()
                .is_some_and(|feedback| feedback.contains("timed out")),
            "the fallback carries the failure reason: {reply}"
        );

        let (dropped_tx, dropped_rx) = oneshot::channel();
        drop(dropped_tx);
        let reply = approval_reply_from_client_result(dropped_rx.await);
        assert_eq!(
            reply["decision"],
            json!("rejected"),
            "a dropped reply channel (disconnect) rejects, never hangs: {reply}"
        );

        // And a delivered reply still maps through the option mapper.
        let reply = approval_reply_from_client_result(Ok(Ok(
            json!({"outcome": {"outcome": "selected", "optionId": "approve_once"}}),
        )));
        assert_eq!(reply["decision"], json!("approved"));
    }

    // --- $/cancel_request + update-channel capacity (P5e) -------------------

    /// Given the engine's wire stop reasons, when they are mapped onto the
    /// prompt response, then each lands on the official v1 StopReason
    /// vocabulary: `aborted` (a cancelled turn) is the v1 `cancelled`, a
    /// provider-filtered reply is the v1 `refusal`, and every engine-only
    /// reason without a v1 counterpart settles `end_turn` so the client is
    /// never stranded on an off-vocabulary literal.
    #[test]
    fn should_map_engine_stop_reasons_to_the_v1_stop_reason_vocabulary() {
        assert_eq!(acp_stop_reason("end_turn"), "end_turn");
        assert_eq!(acp_stop_reason("max_tokens"), "max_tokens");
        assert_eq!(acp_stop_reason("aborted"), "cancelled");
        assert_eq!(acp_stop_reason("filtered"), "refusal");
        // Engine-only reasons with no v1 counterpart keep the client
        // unblocked with the generic clean-turn reason.
        assert_eq!(acp_stop_reason("paused"), "end_turn");
        assert_eq!(acp_stop_reason("blocked_by_hook"), "end_turn");
        assert_eq!(acp_stop_reason("unknown"), "end_turn");
    }

    /// Given the production update channel, when exactly 1024
    /// notifications are published without a consumer draining, then every
    /// one is buffered (no lag) — and a fresh subscriber that falls 1025
    /// behind reports the lag with exactly one dropped. The counts are
    /// literals on purpose: a regression of the capacity back to 256
    /// fails the first phase (it would already report a lag of 768), and
    /// an accidental growth beyond 1024 fails the second (no lag would be
    /// reported at all) — the 1024-slot capacity (P5e decision B) covers
    /// long streaming turns before any chunk is lost.
    #[test]
    fn should_buffer_the_production_update_capacity_before_reporting_a_lag() {
        let handler = AcpHandler::new();
        let notification = |seq: i64| JsonRpcNotification {
            method: "session/update".to_string(),
            params: json!({"seq": seq}),
        };

        // Phase 1 — exactly the production capacity's worth: everything
        // buffers, the next receive is the oldest notification, not a lag
        // report.
        let mut buffered = handler.subscribe_updates();
        for seq in 0..1024i64 {
            handler
                .updates
                .send(notification(seq))
                .expect("a subscribed receiver takes every send");
        }
        match buffered.try_recv() {
            Ok(notification) => assert_eq!(notification.params["seq"], json!(0)),
            other => panic!(
                "1024 notifications must buffer without a lag at the \
                 production capacity, got {other:?}"
            ),
        }

        // Phase 2 — one notification past the capacity for a subscriber
        // that has drained nothing: the oldest slot is overwritten and the
        // next receive reports the lag instead of passing silently, then
        // the stream resumes with the oldest retained notification.
        let mut lagging = handler.subscribe_updates();
        for seq in 0..1025i64 {
            handler
                .updates
                .send(notification(seq))
                .expect("a subscribed receiver takes every send");
        }
        match lagging.try_recv() {
            Err(broadcast::error::TryRecvError::Lagged(skipped)) => assert_eq!(
                skipped, 1,
                "exactly one notification past the capacity is dropped"
            ),
            other => panic!(
                "the first notification past the capacity must report the lag, got {other:?}"
            ),
        }
        match lagging.try_recv() {
            Ok(notification) => assert_eq!(notification.params["seq"], json!(1)),
            other => panic!("the relay must resume after a lag, got {other:?}"),
        }
    }

    /// Given a shrunk test channel (the capacity knob the production
    /// constructor does not use), when it overflows, then the lag is
    /// reported at the injected capacity — the small-capability seam
    /// exercises the lag path without pushing a thousand notifications.
    #[test]
    fn should_report_a_lag_at_the_injected_update_capacity() {
        let handler = AcpHandler::with_engine_pool_and_updates(
            Arc::new(mirri_embedded::EmbeddedServer::for_cwd()),
            None,
            4,
        );
        let mut updates = handler.subscribe_updates();
        let notification = |seq: i64| JsonRpcNotification {
            method: "session/update".to_string(),
            params: json!({"seq": seq}),
        };
        for seq in 0..5 {
            handler
                .updates
                .send(notification(seq))
                .expect("a subscribed receiver takes every send");
        }
        match updates.try_recv() {
            Err(broadcast::error::TryRecvError::Lagged(skipped)) => assert_eq!(skipped, 1),
            other => panic!("a capacity-4 channel must lag after 5 sends, got {other:?}"),
        }
    }

    /// A capture writer for [`tracing_subscriber`] — the same pattern
    /// main.rs uses to assert the relay's warnings: a warn-level log line
    /// lands in the shared buffer the assertion reads back.
    #[derive(Default, Clone)]
    struct SharedBuf(Arc<std::sync::Mutex<Vec<u8>>>);

    impl SharedBuf {
        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone())
                .expect("the captured log is valid UTF-8")
        }
    }

    impl std::io::Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for SharedBuf {
        type Writer = SharedBuf;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Given a `session/new` request carrying `additionalDirectories` — a
    /// field the host reads but cannot honor (the engine's workspace scope
    /// is single-rooted) — when the request is served, then the host warns
    /// loudly (F-P5c-1: never a silent no-op) and serves the session at the
    /// `cwd`'s scope anyway.
    #[test]
    fn should_warn_when_session_new_carries_additional_directories() {
        let buf = SharedBuf::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(buf.clone())
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .finish();
        let handler = Arc::new(AcpHandler::with_engine(Arc::new(ProbeEngine::default())));

        let log = tracing::subscriber::with_default(subscriber, || {
            tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("test runtime must build")
                .block_on(async {
                    handler
                        .handle("initialize", json!({"protocolVersion": 1}))
                        .await
                        .unwrap();
                    // The request still runs (it settles against the
                    // probe's empty session.create answer — the point is
                    // the warning, not the error).
                    let _ = handler
                        .handle(
                            "session/new",
                            json!({"additionalDirectories": ["/elsewhere"]}),
                        )
                        .await;
                });
            buf.contents()
        });
        assert!(
            log.contains("session/new ignores additionalDirectories"),
            "the ignored field must warn, never silently no-op: {log}"
        );
    }

    /// A capture [`EngineAdapter`]: `session.list` answers with
    /// [`Self::session_views`], and every other `call_method` with an
    /// empty result; `on_reverse_rpc` records the handler, so a test can
    /// drive the bridge's `approval.ask` arm directly (no engine turn, no
    /// cancel token — the ask settles only through the bridge itself).
    #[derive(Default)]
    struct ProbeEngine {
        reverse_rpc: std::sync::Mutex<Option<mirri_transport::adapter::ReverseRpcHandler>>,
        /// What `session.list` reports (the engine session views).
        session_views: Vec<Value>,
        /// What `workspace.list` answers (design 14 D2 — the registered
        /// workspace rows `{items: [{id, root, …}]}`; a bare array answers
        /// the same way). Defaults to the empty listing: every cwd misses.
        workspace_views: Vec<Value>,
        /// The params of every `session.create` call, in arrival order —
        /// how tests assert the cwd → `workspaceId` routing landed on the
        /// wire params (design 14 D2) alongside the `mcpServers` overlay
        /// (D5a — both fields must survive the same call).
        created_params: std::sync::Mutex<Vec<Value>>,
        /// What `session.create` answers for the new session's id — `None`
        /// keeps the legacy empty-object answer (whose honest follow-up is
        /// the "returned no session id" internal error the pre-remote tests
        /// pin); the remote-routing tests set a real id so `session/new`
        /// completes its full success surface.
        create_id: Option<String>,
        /// Whether this probe masquerades as a remote adapter (design 14's
        /// `is_remote` signal — the D2/D6 routing paths arm on it).
        remote: bool,
        /// The session ids every `message.abort` named, in arrival order —
        /// how tests observe that a cancel actually reached the engine.
        aborted_sessions: std::sync::Mutex<Vec<String>>,
        /// The session ids every `retention.delete` named, in arrival order
        /// — how tests observe the ACP delete converged on the engine's
        /// unified executor (M4). When non-empty, the FIRST id listed
        /// makes the engine answer MethodNotFound — the legacy-engine
        /// degrade path's stand-in.
        retention_deleted: std::sync::Mutex<Vec<String>>,
        /// Session ids the probe answers MethodNotFound for (the
        /// pre-retention-engine degrade shape).
        retention_unsupported: std::sync::Mutex<std::collections::HashSet<String>>,
        /// The engine-side calls in arrival order, as `"<method>:<session>"`
        /// strings — how tests assert call ordering across DIFFERENT engine
        /// surfaces (e.g. that an `unsubscribe_session` precedes the wire
        /// `session.close`, design 14 D5: close即退订).
        call_order: std::sync::Mutex<Vec<String>>,
    }

    impl ProbeEngine {
        fn reverse_rpc_handler(&self) -> mirri_transport::adapter::ReverseRpcHandler {
            self.reverse_rpc
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .expect("the handler must be wired first")
        }

        /// The `session.create` calls the engine received, params in
        /// arrival order (the routing assertions' input).
        fn create_calls(&self) -> Vec<Value> {
            self.created_params
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        }

        /// Record a call into [`Self::call_order`] (the ordering witness).
        fn record_call(&self, method: &str, params: &Value) {
            let session = params
                .get("session_id")
                .or_else(|| params.get("sessionId"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            self.call_order
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(format!("{method}:{session}"));
        }
    }

    #[async_trait::async_trait]
    impl EngineAdapter for ProbeEngine {
        async fn call_method(&self, method: &str, params: Value) -> Result<Value, EngineError> {
            self.record_call(method, &params);
            match method {
                "session.list" => Ok(Value::Array(self.session_views.clone())),
                "workspace.list" => Ok(Value::Array(self.workspace_views.clone())),
                "session.create" => {
                    self.created_params
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(params);
                    match &self.create_id {
                        Some(id) => Ok(json!({"id": id})),
                        None => Ok(json!({})),
                    }
                }
                "message.abort" => {
                    self.aborted_sessions
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(
                            params
                                .get("session_id")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                        );
                    Ok(json!({}))
                }
                "retention.delete" => {
                    let session_id = params
                        .get("session_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    self.retention_deleted
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(session_id.clone());
                    if self
                        .retention_unsupported
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .contains(&session_id)
                    {
                        // The pre-retention engine shape: the route does
                        // not exist.
                        return Err(EngineError::MethodNotFound("retention.delete".to_string()));
                    }
                    Ok(json!({ "ok": true, "reclaimed_bytes": 0 }))
                }
                _ => Ok(json!({})),
            }
        }

        async fn subscribe_events(
            &self,
        ) -> Result<mpsc::Receiver<JsonRpcNotification>, EngineError> {
            Ok(mpsc::channel(1).1)
        }

        fn is_remote(&self) -> bool {
            self.remote
        }

        async fn unsubscribe_session(&self, session_id: &str) -> Result<(), EngineError> {
            self.record_call("session.unsubscribe", &json!({ "session_id": session_id }));
            Ok(())
        }

        async fn on_reverse_rpc(
            &self,
            handler: mirri_transport::adapter::ReverseRpcHandler,
        ) -> Result<(), EngineError> {
            *self.reverse_rpc.lock().unwrap() = Some(handler);
            Ok(())
        }

        async fn dispose(&self) -> Result<(), EngineError> {
            Ok(())
        }
    }

    /// Given an approval ask parked on the bridge (awaiting the client's
    /// reply) for a session whose prompt is in flight, when the prompt is
    /// cancelled — `$/cancel_request` naming its request id — then the ask
    /// settles with the v1 `Cancelled` outcome semantics (`decision:
    /// cancelled`) instead of waiting out the bridge's 60 s timeout
    /// (F-P5b-4: the host answers on the cancelling client's behalf).
    #[tokio::test]
    async fn should_answer_a_pending_permission_ask_as_cancelled_when_its_prompt_is_cancelled() {
        let probe = Arc::new(ProbeEngine::default());
        let engine: Arc<dyn EngineAdapter> = probe.clone();
        let handler = AcpHandler::with_engine(Arc::clone(&engine));
        handler
            .ensure_wired(&engine)
            .await
            .expect("wiring the probe engine must succeed");
        // The prompt session is in flight, and the bridge parks the ask on
        // it (the ask carries no session id — the bridge addresses it to
        // the most recent in-flight prompt session).
        handler.push_active_prompt_session("session-1");
        handler.in_flight_prompts.lock().unwrap().insert(
            RequestId::Number(9),
            InFlightPrompt {
                session_id: "session-1".to_string(),
                engine: Arc::clone(&engine),
            },
        );
        let ask = tokio::spawn(async move {
            probe.reverse_rpc_handler()(
                "approval.ask".to_string(),
                json!({
                    "tool_name": "Bash",
                    "tool_call_id": "call_cancel",
                    "action": "run",
                    "display": {},
                    "reason": "approval required",
                }),
            )
            .await
            .expect("the ask must settle, never hang")
        });
        let mut client_requests = handler.subscribe_client_requests();
        let pending =
            tokio::time::timeout(std::time::Duration::from_secs(5), client_requests.recv())
                .await
                .expect("the ask must reach the client side")
                .expect("the client request channel stays open");
        assert_eq!(pending.method, "session/request_permission");
        // Deliberately never answered by the "client".

        handler
            .handle("$/cancel_request", json!({"requestId": 9}))
            .await
            .expect("the cancel must succeed");

        let reply = tokio::time::timeout(std::time::Duration::from_secs(2), ask)
            .await
            .expect("the pending ask must settle on cancel (F-P5b-4)")
            .expect("the ask task must finish");
        assert_eq!(
            reply["decision"],
            json!("cancelled"),
            "the ask resolves as the v1 Cancelled outcome: {reply}"
        );
    }

    /// Given a `$/cancel_request` naming a request that is unknown,
    /// settled, or already cancelled, when it arrives, then it is an
    /// idempotent no-op — the JSON-RPC cancellation semantics (a server
    /// must tolerate cancels that lose the race with the response).
    #[tokio::test]
    async fn should_treat_dollar_cancel_request_for_an_unknown_id_as_a_no_op() {
        let handler = AcpHandler::new();
        let result = handler
            .handle("$/cancel_request", json!({"requestId": 999}))
            .await
            .expect("an unknown request id is a no-op, never an error");
        assert_eq!(result, json!({}));
    }

    /// Given a `$/cancel_request` whose `requestId` is a string (the id
    /// shape ACP clients like Zed mint — a UUID string on every request),
    /// when it names an unknown request, then it is the same idempotent
    /// no-op as integer ids — never an invalid-params rejection.
    #[tokio::test]
    async fn should_treat_dollar_cancel_request_for_a_string_request_id_as_a_no_op() {
        let handler = AcpHandler::new();
        let result = handler
            .handle(
                "$/cancel_request",
                json!({"requestId": "9b0f0d64-41a7-4b6e-9a4e-2d2f6c8b1f01"}),
            )
            .await
            .expect("a string requestId is accepted, not invalid params");
        assert_eq!(result, json!({}));
    }

    /// Given an in-flight `session/prompt` registered under a string
    /// request id, when `$/cancel_request` names that same string id, then
    /// the cancel routes to the engine's turn abort for that prompt's
    /// session — string ids key the in-flight table exactly like integer
    /// ids do.
    #[tokio::test]
    async fn should_cancel_the_in_flight_prompt_when_dollar_cancel_request_names_its_string_id() {
        let probe = Arc::new(ProbeEngine::default());
        let engine: Arc<dyn EngineAdapter> = probe.clone();
        let handler = AcpHandler::with_engine(Arc::clone(&engine));
        handler
            .in_flight_prompts
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                RequestId::Str("9b0f0d64-41a7-4b6e-9a4e-2d2f6c8b1f01".to_string()),
                InFlightPrompt {
                    session_id: "session-1".to_string(),
                    engine: Arc::clone(&engine),
                },
            );

        handler
            .handle(
                "$/cancel_request",
                json!({"requestId": "9b0f0d64-41a7-4b6e-9a4e-2d2f6c8b1f01"}),
            )
            .await
            .expect("the cancel must succeed");

        assert_eq!(
            probe.aborted_sessions.lock().unwrap().as_slice(),
            ["session-1"],
            "the string-id cancel must reach the engine's turn abort"
        );
    }

    /// Given a `$/cancel_request` without a usable `requestId` param, when
    /// it is handled, then it is rejected with invalid params — the
    /// malformed cancel is visible on the request channel (the well-formed
    /// notification form never carries a response either way).
    #[tokio::test]
    async fn should_reject_dollar_cancel_request_when_the_request_id_is_missing() {
        let handler = AcpHandler::new();
        let (code, _) = handler
            .handle("$/cancel_request", json!({}))
            .await
            .unwrap_err();
        assert_eq!(code, ERROR_INVALID_PARAMS);
    }

    /// A programmable capture [`EngineAdapter`] for the engine-slot
    /// tests: every call is answered from the scripted behavior —
    /// [`Self::say`] answers a method with a fixed result object
    /// (`session.create` maps its `say` value to `{"id": value}`),
    /// [`Self::fail_transport`] makes every call answer a transport
    /// error (the shape a dead [`RemoteAdapter`] surfaces), and
    /// `session.close` arrivals are recorded in order.
    #[derive(Default)]
    struct ScriptedEngine {
        /// The result `session.create` answers (the new session's id).
        create_id: Option<String>,
        /// When set, every call answers a transport error instead.
        transport_dead: bool,
        /// The session ids every `session.close` named, in arrival
        /// order — how tests observe which engine served a call.
        closed: std::sync::Mutex<Vec<String>>,
    }

    impl ScriptedEngine {
        /// Answer `session.create` with this session id.
        fn say(mut self, _method: &str, create_id: &str) -> Self {
            self.create_id = Some(create_id.to_string());
            self
        }

        /// Answer every call with a transport error (connection death).
        fn fail_transport(mut self) -> Self {
            self.transport_dead = true;
            self
        }

        /// The session ids every `session.close` named, in order.
        fn closed_sessions(&self) -> Vec<String> {
            self.closed.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl EngineAdapter for ScriptedEngine {
        async fn call_method(&self, method: &str, params: Value) -> Result<Value, EngineError> {
            if self.transport_dead {
                return Err(EngineError::Transport("connection closed".to_string()));
            }
            match method {
                "session.create" => Ok(json!({"id": self.create_id.clone().unwrap_or_default()})),
                "session.close" => {
                    self.closed.lock().unwrap().push(
                        params
                            .get("session_id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    );
                    Ok(json!({}))
                }
                "session.list" => Ok(json!([])),
                _ => Ok(json!({})),
            }
        }

        async fn subscribe_events(
            &self,
        ) -> Result<mpsc::Receiver<JsonRpcNotification>, EngineError> {
            Ok(mpsc::channel(1).1)
        }

        async fn on_reverse_rpc(
            &self,
            _handler: mirri_transport::adapter::ReverseRpcHandler,
        ) -> Result<(), EngineError> {
            Ok(())
        }

        async fn dispose(&self) -> Result<(), EngineError> {
            Ok(())
        }
    }
}
