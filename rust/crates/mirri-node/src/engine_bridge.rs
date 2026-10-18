//! Engine bridge — wraps the embedded engine for FFI consumption.
//!
//! The NAPI layer (Phase 4.1) calls these methods from JavaScript. The bridge
//! handles:
//! - Tokio runtime management (one shared runtime per process)
//! - Method dispatch (JS → Rust engine), fully async — no `block_on`
//! - Event forwarding (engine event bus → JS callback via ThreadsafeFunction)
//! - Reverse RPC (engine → JS requests, answered via `respond_reverse_rpc`)
//! - Error conversion (Rust errors → JS Error)
//!
//! The engine itself is an [`EmbeddedServer`](mirri_embedded::EmbeddedServer)
//! wired to `mirri-core`, so every RPC method runs a real engine turn — no
//! network, no IPC.

use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

use mirri_core::WorkspaceId;
use mirri_transport::adapter::{EngineAdapter, EngineError, ReverseRpcHandler};
use mirri_transport::frame::JsonRpcNotification;

/// Reverse RPC requests awaiting a response from the client, keyed by
/// request id. Each `oneshot::Sender` resolves the engine's parked call with
/// the client's answer (`Ok(value)` or `Err(message)`).
type PendingReverseRpcs = HashMap<i64, oneshot::Sender<Result<Value, String>>>;

/// A bridge between the Node.js FFI layer and the Rust engine.
///
/// Holds a shared Tokio runtime and an engine adapter (an
/// [`EmbeddedServer`](mirri_embedded::EmbeddedServer) in production, or any
/// other adapter in tests). The NAPI layer calls `call_method`, which
/// dispatches to the engine on the Tokio runtime; engine events and reverse
/// RPC requests are pushed to JS callbacks by forwarder tasks spawned on the
/// same runtime.
pub struct EngineBridge {
    /// `None` only while [`Drop`] is relocating the runtime to its shutdown
    /// thread.
    runtime: Option<Arc<tokio::runtime::Runtime>>,
    engine: Arc<dyn EngineAdapter>,
    /// Reverse RPC requests awaiting a response from the client (JS side),
    /// keyed by request id. [`Self::respond_reverse_rpc`] removes the entry
    /// and resolves the pending handler future.
    ///
    /// A `std::sync::Mutex` (not a tokio mutex): the entry must be inserted
    /// synchronously inside the `ReverseRpcHandler` closure — before the
    /// request is handed to the callback — so a client response can never
    /// arrive before its entry exists. The lock is held only for the
    /// HashMap insert/remove, never across an await.
    pending_reverse_rpcs: Arc<Mutex<PendingReverseRpcs>>,
    /// Monotonic source of reverse RPC request ids.
    next_reverse_rpc_id: Arc<AtomicI64>,
    /// Handle to the currently running event-forwarder task, if any.
    /// Aborted and replaced on each `start_event_forwarder` call so repeat
    /// subscriptions don't leak the old task and its ThreadsafeFunction.
    event_forwarder_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Handle to the currently running reverse-RPC-forwarder task, if any.
    /// Aborted and replaced on each `start_reverse_rpc_forwarder` call.
    reverse_rpc_forwarder_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl EngineBridge {
    /// Create a new bridge with a dedicated Tokio runtime and an embedded
    /// engine rooted at the current working directory, without an LLM
    /// provider factory (`message.send` reports that no provider is
    /// configured).
    pub fn new() -> Self {
        Self::with_home_dir(None, Vec::new())
    }

    /// Create a new bridge with a dedicated Tokio runtime and an embedded
    /// engine rooted at the current working directory, with an optional
    /// explicitly injected Mirri home directory and explicit skill
    /// directories.
    ///
    /// The home roots the engine's config.toml / profiles / plugins / MCP
    /// discovery and the session persistence root (see
    /// [`EmbeddedServer::for_workspace_root`]); `None` keeps the env-based
    /// resolution. The NAPI `createEngine(homeDir, skillDirs)` path threads
    /// the TS harness's isolated `homeDir` and the SDK's `skillDirs` through
    /// here: non-empty `skill_dirs` replace the engine's default user/project
    /// skill discovery (the v1/v2 SDK contract — see
    /// [`mirri_embedded::bootstrap::assemble`]).
    pub fn with_home_dir(
        home_dir: Option<std::path::PathBuf>,
        skill_dirs: Vec<std::path::PathBuf>,
    ) -> Self {
        let home_dir_for_pool = home_dir.clone();
        let mut server = mirri_embedded::EmbeddedServer::for_workspace_root(
            WorkspaceId::new("embedded"),
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            home_dir,
            skill_dirs,
        );
        // The user-layer MCP pool is a process-level asset: every engine
        // this NAPI process creates shares one pool per home (the desktop
        // N-workspaces case connects the user layer exactly once — spec
        // §4.2). The assembled engine's private pool is swapped out here,
        // before any connection can be claimed. Keyed by the same home the
        // config loader reads, so the pool and the config always agree.
        if let Some(base) =
            mirri_embedded::bootstrap::resolve_mcp_home_base(home_dir_for_pool.as_deref())
        {
            // The shared pool's OAuth token store resolves through the same
            // Mirri-home chain (the injected home wins, else the ambient
            // chain) so cached tokens survive across engines.
            let oauth_token_dir =
                mirri_embedded::bootstrap::mcp_oauth_token_dir(home_dir_for_pool.as_deref());
            server = server.with_user_mcp_pool(mirri_embedded::shared_user_mcp_pool_for_home(
                &base,
                oauth_token_dir,
            ));
        }
        Self::with_engine(Arc::new(server))
    }

    /// Create a new bridge driving the given engine adapter.
    ///
    /// Tests inject an [`EmbeddedServer`](mirri_embedded::EmbeddedServer)
    /// built on a [`MockLlmFactory`](mirri_test_harness::MockLlmFactory) so
    /// engine turns run against scripted LLM responses.
    pub fn with_engine(engine: Arc<dyn EngineAdapter>) -> Self {
        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("failed to build Tokio runtime"),
        );
        Self {
            runtime: Some(runtime),
            engine,
            pending_reverse_rpcs: Arc::new(Mutex::new(HashMap::new())),
            next_reverse_rpc_id: Arc::new(AtomicI64::new(0)),
            event_forwarder_handle: Mutex::new(None),
            reverse_rpc_forwarder_handle: Mutex::new(None),
        }
    }

    /// Call an RPC method on the engine.
    ///
    /// The call hops onto the bridge's own Tokio runtime (the NAPI layer may
    /// drive this future from a different runtime) and awaits the engine's
    /// answer. A panic inside the dispatched task surfaces as an
    /// [`EngineError::Internal`].
    pub async fn call_method(&self, method: &str, params: Value) -> Result<Value, EngineError> {
        let engine = Arc::clone(&self.engine);
        let method = method.to_string();
        self.runtime()
            .handle()
            .spawn(async move { engine.call_method(&method, params).await })
            .await
            .map_err(|e| EngineError::Internal(format!("call_method task panicked: {e}")))?
    }

    /// Subscribe to engine events.
    ///
    /// Returns a receiver that drains the engine's event channel directly —
    /// used by tests and tooling that want the raw
    /// [`JsonRpcNotification`] stream. Production event flow goes through
    /// [`Self::start_event_forwarder`] instead.
    pub async fn subscribe_events(
        &self,
    ) -> Result<mpsc::Receiver<JsonRpcNotification>, EngineError> {
        let engine = Arc::clone(&self.engine);
        self.runtime()
            .handle()
            .spawn(async move { engine.subscribe_events().await })
            .await
            .map_err(|e| EngineError::Internal(format!("subscribe_events task panicked: {e}")))?
    }

    /// Forward engine events to `callback` until the engine is disposed.
    ///
    /// Spawns a task on the bridge's runtime that subscribes to the engine
    /// event stream and invokes `callback` for each notification, wrapped as
    /// `{ "method": ..., "params": ... }`. The callback runs on a Tokio
    /// worker thread — the NAPI layer wraps it in a `ThreadsafeFunction` so
    /// it can safely call into JavaScript. The task ends when the engine
    /// closes the event channel (e.g. on dispose).
    pub fn start_event_forwarder<F>(&self, callback: F)
    where
        F: Fn(serde_json::Value) + Send + 'static,
    {
        let engine = Arc::clone(&self.engine);
        let handle = self.runtime().spawn(async move {
            match engine.subscribe_events().await {
                Ok(mut rx) => {
                    while let Some(event) = rx.recv().await {
                        let payload = serde_json::json!({
                            "method": event.method,
                            "params": event.params,
                        });
                        callback(payload);
                    }
                }
                Err(e) => {
                    tracing::error!("subscribe_events failed: {e}");
                }
            }
        });
        // Abort the previous forwarder if any so its ThreadsafeFunction is
        // dropped and duplicate delivery stops.
        if let Some(prev) = self.event_forwarder_handle.lock().unwrap().take() {
            prev.abort();
        }
        *self.event_forwarder_handle.lock().unwrap() = Some(handle);
    }

    /// Forward reverse RPC requests to `callback` and relay responses back
    /// to the engine.
    ///
    /// Spawns a task on the bridge's runtime that registers a
    /// [`ReverseRpcHandler`] on the engine. Each engine-initiated request
    /// is assigned a unique request id, parked on a oneshot channel, and
    /// handed to `callback` as `(id, method, params)`; when the client
    /// answers via [`Self::respond_reverse_rpc`] the parked future resolves
    /// and the engine's call returns the client's result.
    pub fn start_reverse_rpc_forwarder<F>(&self, callback: F)
    where
        F: Fn(i64, String, Value) + Send + Sync + 'static,
    {
        let callback = Arc::new(callback);
        let engine = Arc::clone(&self.engine);
        let pending_reverse_rpcs = Arc::clone(&self.pending_reverse_rpcs);
        let next_reverse_rpc_id = Arc::clone(&self.next_reverse_rpc_id);
        let handle = self.runtime().spawn(async move {
            let handler: ReverseRpcHandler = Arc::new(move |method, params| {
                let callback = Arc::clone(&callback);
                let pending_reverse_rpcs = Arc::clone(&pending_reverse_rpcs);
                let request_id = next_reverse_rpc_id.fetch_add(1, Ordering::SeqCst);
                let (response_tx, response_rx) = oneshot::channel();
                // Park the response channel *before* notifying the client:
                // the client may answer synchronously inside `callback`, and
                // the response must always find its pending entry.
                pending_reverse_rpcs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(request_id, response_tx);
                callback(request_id, method, params);
                Box::pin(async move {
                    response_rx.await.map_err(|_| {
                        "reverse rpc response channel closed before a response was sent".to_string()
                    })?
                })
            });
            if let Err(e) = engine.on_reverse_rpc(handler).await {
                tracing::error!("on_reverse_rpc registration failed: {e}");
            }
        });
        // Abort the previous forwarder if any to avoid duplicate handlers.
        if let Some(prev) = self.reverse_rpc_forwarder_handle.lock().unwrap().take() {
            prev.abort();
        }
        *self.reverse_rpc_forwarder_handle.lock().unwrap() = Some(handle);
    }

    /// Answer a pending reverse RPC request with the client's result.
    ///
    /// `Ok(value)` resolves the engine's pending call with `value`;
    /// `Err(message)` resolves it with a client-side error. Unknown request
    /// ids (already answered, or never issued) are logged and ignored.
    pub async fn respond_reverse_rpc(&self, id: i64, result: Result<Value, String>) {
        match self
            .pending_reverse_rpcs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id)
        {
            Some(response_tx) => {
                let _ = response_tx.send(result);
            }
            None => {
                tracing::warn!(
                    request_id = id,
                    "respond_reverse_rpc for unknown request id"
                );
            }
        }
    }

    /// Dispose of the engine, cancelling all in-flight work.
    ///
    /// Runs on the bridge's own Tokio runtime, mirroring
    /// [`Self::call_method`]. A panic inside the dispatched task surfaces as
    /// an [`EngineError::Internal`].
    pub async fn dispose(&self) -> Result<(), EngineError> {
        let engine = Arc::clone(&self.engine);
        self.runtime()
            .handle()
            .spawn(async move { engine.dispose().await })
            .await
            .map_err(|e| EngineError::Internal(format!("dispose task panicked: {e}")))?
    }

    /// Get a handle to the underlying Tokio runtime.
    pub fn runtime(&self) -> &tokio::runtime::Runtime {
        self.runtime
            .as_ref()
            .expect("runtime is only removed while EngineBridge drops")
    }
}

/// [`EngineBridge`] is the last owner of its multi-thread Tokio runtime, and
/// dropping a multi-thread runtime performs a blocking shutdown — which tokio
/// forbids from inside an async context (`Cannot drop a runtime in a context
/// where blocking is not allowed`). The last bridge reference can die inside
/// one: `#[tokio::test]` bodies run inside the test runtime, and the NAPI
/// layer drives its `async fn`s on napi-rs's own Tokio runtime. When the
/// final runtime reference is dropped from such a context, this impl relocates
/// the runtime to a plain thread so the shutdown always runs outside any
/// runtime context.
impl Drop for EngineBridge {
    fn drop(&mut self) {
        let Some(runtime) = self.runtime.take() else {
            return;
        };
        let Ok(runtime) = Arc::try_unwrap(runtime) else {
            // Another Arc owns the runtime; it drops there. Nothing to
            // relocate.
            return;
        };
        if tokio::runtime::Handle::try_current().is_err() {
            // Outside any runtime context: the blocking shutdown is allowed,
            // drop the runtime inline.
            drop(runtime);
            return;
        }
        // Inside a Tokio runtime context: hand the runtime to a plain
        // thread, whose blocking shutdown is allowed. The closure takes it
        // out of the `Option`, so a failed spawn drops it inline on the
        // current thread instead (best effort — thread spawn failure is
        // unrecoverable and rare).
        let mut runtime = Some(runtime);
        if let Ok(shutdown_thread) = std::thread::Builder::new()
            .name("mirri-engine-runtime-shutdown".to_string())
            .spawn(move || drop(runtime.take()))
        {
            let _ = shutdown_thread.join();
        }
    }
}

impl Default for EngineBridge {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use serde_json::json;
    use std::time::Duration;
    use tokio::time::{Instant, timeout};

    /// Test adapter that answers `engine.ping` by issuing a reverse RPC
    /// request to the registered handler — simulating an engine-initiated
    /// call into the client (JS) side.
    #[derive(Default)]
    struct ReverseRpcProbeAdapter {
        handler: std::sync::Mutex<Option<ReverseRpcHandler>>,
    }

    impl ReverseRpcProbeAdapter {
        /// Whether `on_reverse_rpc` has handed us the handler yet.
        fn has_registered_handler(&self) -> bool {
            self.handler
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some()
        }
    }

    #[async_trait]
    impl EngineAdapter for ReverseRpcProbeAdapter {
        async fn call_method(&self, method: &str, _params: Value) -> Result<Value, EngineError> {
            if method != "engine.ping" {
                return Err(EngineError::MethodNotFound(method.to_string()));
            }
            let handler = self
                .handler
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
                .expect("reverse rpc handler must be registered before engine.ping");
            handler("client.answer".to_string(), json!({ "question": "2+2" }))
                .await
                .map_err(EngineError::Internal)
        }

        async fn subscribe_events(
            &self,
        ) -> Result<mpsc::Receiver<JsonRpcNotification>, EngineError> {
            let (_tx, rx) = mpsc::channel(16);
            Ok(rx)
        }

        async fn on_reverse_rpc(&self, handler: ReverseRpcHandler) -> Result<(), EngineError> {
            *self.handler.lock().unwrap_or_else(|e| e.into_inner()) = Some(handler);
            Ok(())
        }

        async fn dispose(&self) -> Result<(), EngineError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn should_return_method_not_found_when_method_is_unknown() {
        let bridge = EngineBridge::new();
        let result = bridge.call_method("no.such.method", json!({})).await;
        assert!(matches!(
            result,
            Err(EngineError::MethodNotFound(ref m)) if m == "no.such.method"
        ));
    }

    #[tokio::test]
    async fn should_return_empty_channel_when_subscribing_to_events() {
        let bridge = EngineBridge::new();
        let mut rx = bridge.subscribe_events().await.unwrap();
        assert!(rx.try_recv().is_err()); // channel is empty
    }

    #[tokio::test]
    async fn should_forward_engine_events_when_event_forwarder_started() {
        // Given a bridge (no LLM provider: message.send echoes the user
        // message on the event bus, then fails the turn — the echo alone
        // must reach the forwarder)
        let bridge = EngineBridge::new();
        let received = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = Arc::clone(&received);
        bridge.start_event_forwarder(move |payload| {
            recorded
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(payload);
        });

        // When a session is created and a message is sent
        let created = bridge
            .call_method("session.create", json!({ "title": "event forwarder" }))
            .await
            .expect("session.create must succeed");
        let session_id = created["id"]
            .as_str()
            .expect("session.create returns a session id")
            .to_string();
        let _ = bridge
            .call_method(
                "message.send",
                json!({ "session_id": session_id, "text": "Hello", "mode": "fire" }),
            )
            .await;

        // Then the forwarder delivers the event.message.received notification
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            {
                let recorded = received.lock().unwrap_or_else(|e| e.into_inner());
                if recorded
                    .iter()
                    .any(|payload| payload["method"] == "event.message.received")
                {
                    break;
                }
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for a forwarded event"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn should_respond_when_engine_issues_reverse_rpc_request() {
        // Given a bridge over an adapter that issues a reverse RPC on engine.ping
        let probe = Arc::new(ReverseRpcProbeAdapter::default());
        let engine: Arc<dyn EngineAdapter> = probe.clone();
        let bridge = Arc::new(EngineBridge::with_engine(engine));
        let (request_tx, mut request_rx) = mpsc::channel::<(i64, String, Value)>(4);
        bridge.start_reverse_rpc_forwarder(move |request_id, method, params| {
            let _ = request_tx.try_send((request_id, method, params));
        });

        // And the handler registration has landed before the engine fires
        let registration_deadline = Instant::now() + Duration::from_secs(5);
        while !probe.has_registered_handler() {
            assert!(
                Instant::now() < registration_deadline,
                "reverse rpc handler never registered"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // When the engine issues a reverse RPC request
        let bridge_for_call = Arc::clone(&bridge);
        let engine_ping =
            tokio::spawn(
                async move { bridge_for_call.call_method("engine.ping", json!({})).await },
            );
        let (request_id, method, params) = timeout(Duration::from_secs(10), request_rx.recv())
            .await
            .expect("timed out waiting for the reverse rpc request")
            .expect("reverse rpc request channel closed unexpectedly");
        assert_eq!(method, "client.answer");
        assert_eq!(params["question"], "2+2");

        // And the client answers it
        bridge
            .respond_reverse_rpc(request_id, Ok(json!({ "answer": 4 })))
            .await;

        // Then the engine's call resolves with the client's answer
        let result = timeout(Duration::from_secs(10), engine_ping)
            .await
            .expect("timed out waiting for engine.ping to resolve")
            .expect("engine.ping task must not panic")
            .expect("engine.ping must succeed");
        assert_eq!(result["answer"], 4);
    }

    #[tokio::test]
    async fn should_succeed_when_disposing() {
        let bridge = EngineBridge::new();
        assert!(bridge.dispose().await.is_ok());
    }
}
