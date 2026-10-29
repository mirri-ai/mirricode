//! Integration tests for `mirri-remote` — the WS-client implementation of
//! [`EngineAdapter`] — against the REAL `mirri-server` on an ephemeral port.
//!
//! The server is booted in-process with the same MockLlm-backed pattern as
//! `mirri-server`'s own test suite (each integration-test binary compiles its
//! own harness). A scripted WebSocket peer supplements the real server for
//! two behaviors the server cannot produce deterministically: batched array
//! frames (the server only coalesces under load) and a connection that hangs
//! or dies mid-call (dispose / death semantics).
//!
//! Naming follows the Given-When-Then convention.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use mirri_core::{
    AgentId, LlmFactory, ServerConfig, SessionId, SessionManager, WorkspaceId, WorkspaceScope,
    WorkspaceScopeConfig,
};
use mirri_embedded::EmbeddedServer;
use mirri_remote::{RemoteAdapter, SubscribePolicy};
use mirri_server::ServerState;
use mirri_test_harness::{MockLlm, MockLlmFactory};
use mirri_transport::adapter::{EngineAdapter, EngineError, ReverseRpcHandler};
use serde_json::{Value, json};
use std::sync::RwLock;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

/// The engine-side default model seeded into the test boot's config
/// snapshot: the embedded create chain requires a resolvable model (spec 11
/// R4 — no built-in fallback), so a `session.create` without an explicit
/// `modelId` needs SOME default configured, exactly like a real user's
/// config.toml would (the same suite-wide seed the mirri-server wire tests
/// and the cross-backend contract suite in 4cf03e13 carry).
const TEST_DEFAULT_MODEL: &str = "test-default-model";

/// The seeded engine config snapshot for the test boot.
fn test_engine_config() -> Arc<RwLock<ServerConfig>> {
    Arc::new(RwLock::new(ServerConfig {
        default_model: Some(TEST_DEFAULT_MODEL.to_string()),
        ..ServerConfig::default()
    }))
}

/// A running test server bound to an ephemeral localhost port — a copy of
/// `mirri-server/tests/common/mod.rs`'s `TestServer` (integration-test
/// binaries cannot import each other's test modules).
struct TestServer {
    addr: SocketAddr,
    state: ServerState,
    _tmp: tempfile::TempDir,
}

impl TestServer {
    /// Boot an open-mode server, optionally with a MockLlm factory (engine
    /// turns run against the scripted LLM) and a heartbeat interval. Since
    /// the M5-B3a thinning the server is transport + auth + projection only:
    /// the engine is an in-process [`EmbeddedServer`] behind the same seam
    /// `mirri serve` uses, so these tests also prove the RemoteAdapter ↔
    /// server ↔ embedded-engine stack.
    async fn start(
        llm_factory: Option<Arc<dyn LlmFactory>>,
        auth_token: Option<String>,
        heartbeat_interval: Option<Duration>,
    ) -> Self {
        let tmp = tempfile::tempdir().expect("create temp dir");
        let workspace = Arc::new(WorkspaceScope::new(
            WorkspaceId::new("remote-test-ws"),
            tmp.path().to_path_buf(),
            WorkspaceScopeConfig::default(),
        ));
        let manager = Arc::new(SessionManager::with_persistence_dir(
            workspace,
            tmp.path().join(".mirri").join("sessions"),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        ));
        let engine: Arc<dyn EngineAdapter> = Arc::new(EmbeddedServer::with_providers(
            Arc::clone(&manager),
            llm_factory,
            None,
            None,
            None,
            test_engine_config(),
            None,
        ));
        let state = ServerState::with_engine(engine, manager, "0.1.0-test".to_string(), auth_token);
        let state = match heartbeat_interval {
            Some(interval) => state.with_heartbeat_interval(interval),
            None => state,
        };
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let server_state = state.clone();
        tokio::spawn(async move {
            if let Err(error) = mirri_server::http::serve(listener, server_state, None).await {
                tracing::error!(%error, "test server exited with an error");
            }
        });
        Self {
            addr,
            state,
            _tmp: tmp,
        }
    }

    fn ws_rpc_url(&self) -> String {
        format!("ws://{}/api/v1/rpc", self.addr)
    }
}

/// A scripted JSON-RPC WebSocket peer speaking just enough of the unified
/// protocol for the adapter's handshake and auto-subscribe: it greets with
/// `server_hello`, accepts `client_hello`, answers `session.list` with a
/// pre-seeded session list (empty by default), and acknowledges
/// `session.subscribe` — recording every subscribe into a shared spy. Other
/// methods are left unanswered (a hanging call) or answered by the peer's
/// own arms — see [`ScriptedPeer::start`]. Killing the peer drops the socket,
/// which the adapter observes as a connection death.
struct ScriptedPeer {
    addr: SocketAddr,
    handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Every `session.subscribe` the peer served, recorded by session id.
    subscribe_calls: Arc<Mutex<Vec<String>>>,
    /// Every request method the peer received, in arrival order — pins
    /// wire-level ordering (e.g. a touch-subscribe preceding the touched
    /// call, or an unsubscribe between two touches).
    method_calls: Arc<Mutex<Vec<String>>>,
}

impl ScriptedPeer {
    /// Start a peer with an empty `session.list` response.
    async fn start() -> Self {
        Self::with_sessions(&[]).await
    }

    /// Start a peer whose `session.list` answers the given session ids —
    /// the persisted-sessions world a connecting adapter would observe.
    async fn with_sessions(session_ids: &[&str]) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");
        let sessions: Vec<Value> = session_ids
            .iter()
            .map(|id| json!({ "id": id }))
            .collect();
        let subscribe_calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let spy = Arc::clone(&subscribe_calls);
        let method_calls = Arc::new(Mutex::new(Vec::<String>::new()));
        let methods_spy = Arc::clone(&method_calls);
        let handle = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("peer accepts a connection");
            let ws = tokio_tungstenite::accept_async(stream)
                .await
                .expect("websocket upgrade");
            let (mut sink, mut incoming) = ws.split();
            let hello = json!({
                "jsonrpc": "2.0",
                "method": "server_hello",
                "params": { "serverId": "scripted-peer", "version": "0.0.1", "heartbeatMs": 15000 },
            });
            sink.send(Message::Text(hello.to_string().into()))
                .await
                .expect("send server_hello");
            loop {
                match incoming.next().await {
                    Some(Ok(Message::Text(text))) => {
                        let value: Value =
                            serde_json::from_str(text.as_str()).expect("peer parses the frame");
                        let method = value.get("method").and_then(Value::as_str);
                        let id = value.get("id").and_then(Value::as_i64);
                        if let Some(method) = method
                            && method != "client_hello"
                        {
                            methods_spy
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .push(method.to_string());
                        }
                        match (method, id) {
                            (Some("client_hello"), Some(id)) => {
                                send_json(
                                    &mut sink,
                                    json!({"jsonrpc":"2.0","id":id,"result":{"accepted":true}}),
                                )
                                .await;
                            }
                            (Some("session.list"), Some(id)) => {
                                send_json(
                                    &mut sink,
                                    json!({"jsonrpc":"2.0","id":id,"result":{"items": sessions.clone()}}),
                                )
                                .await;
                            }
                            (Some("session.subscribe"), Some(id)) => {
                                let session_id = value
                                    .get("params")
                                    .and_then(|params| params.get("sessionId"))
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string();
                                spy.lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .push(session_id);
                                send_json(
                                    &mut sink,
                                    json!({"jsonrpc":"2.0","id":id,"result":{"seq":0,"epoch":"peer"}}),
                                )
                                .await;
                            }
                            (Some("session.create"), Some(id)) => {
                                // Acknowledge a created session so the
                                // adapter's subscribe-before-complete path
                                // runs against a resolvable id.
                                send_json(
                                    &mut sink,
                                    json!({"jsonrpc":"2.0","id":id,"result":{"id":"sess-created"}}),
                                )
                                .await;
                            }
                            (Some("forkSession"), Some(id)) => {
                                // The ACP host's session/fork engine call —
                                // answers a forked session id distinct from the
                                // parent so the subscribe-before-complete arm
                                // resolves against it.
                                send_json(
                                    &mut sink,
                                    json!({"jsonrpc":"2.0","id":id,"result":{"id":"sess-forked"}}),
                                )
                                .await;
                            }
                            (Some("session.get"), Some(id)) => {
                                // A session-addressed read the touch-to-subscribe
                                // branch must guard.
                                send_json(
                                    &mut sink,
                                    json!({"jsonrpc":"2.0","id":id,"result":{"id":"sess-old","title":"touched"}}),
                                )
                                .await;
                            }
                            (Some("session.unsubscribe"), Some(id)) => {
                                send_json(
                                    &mut sink,
                                    json!({"jsonrpc":"2.0","id":id,"result":{"ok":true}}),
                                )
                                .await;
                            }
                            (Some("test.blast"), Some(id)) => {
                                // A batched array frame coalescing two event
                                // notifications, then the response — the
                                // server's `eventBatching` wire form.
                                let batch = json!([
                                    {"jsonrpc":"2.0","method":"event.turn.started","params":{"sessionId":"s1","volatile":false,"seq":1,"epoch":"peer"}},
                                    {"jsonrpc":"2.0","method":"event.assistant.delta","params":{"sessionId":"s1","volatile":true,"delta":"hi"}},
                                ]);
                                sink.send(Message::Text(batch.to_string().into()))
                                    .await
                                    .expect("peer sends the batched frame");
                                send_json(
                                    &mut sink,
                                    json!({"jsonrpc":"2.0","id":id,"result":{"blasted":true}}),
                                )
                                .await;
                            }
                            _ => {
                                // Unknown methods (e.g. `slow.ping`) are left
                                // unanswered: the caller's future stays pending.
                            }
                        }
                    }
                    Some(Ok(_)) => {}
                    _ => break,
                }
            }
        });
        Self {
            addr,
            handle: Mutex::new(Some(handle)),
            subscribe_calls,
            method_calls,
        }
    }

    fn url(&self) -> String {
        format!("ws://{}/api/v1/rpc", self.addr)
    }

    /// The session ids every `session.subscribe` call carried, in order.
    fn subscribe_calls(&self) -> Vec<String> {
        self.subscribe_calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The method of every request the peer received, in wire order.
    fn method_calls(&self) -> Vec<String> {
        self.method_calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Kill the peer: the socket drops and the adapter observes a closed
    /// connection.
    fn kill(&self) {
        if let Some(handle) = self.handle.lock().unwrap_or_else(|e| e.into_inner()).take() {
            handle.abort();
        }
    }
}

async fn send_json(
    sink: &mut futures::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
        Message,
    >,
    value: Value,
) {
    sink.send(Message::Text(value.to_string().into()))
        .await
        .expect("peer sends a frame");
}

/// Register a reverse-RPC handler that records every request and answers
/// with the given `decision` payload.
fn scripted_reverse_rpc_handler(
    requests: &Arc<Mutex<Vec<(String, Value)>>>,
    respond: Value,
) -> ReverseRpcHandler {
    let requests = Arc::clone(requests);
    Arc::new(move |method, params| {
        requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((method.clone(), params.clone()));
        let respond = respond.clone();
        Box::pin(async move { Ok(respond) })
    })
}

// ===========================================================================
// Handshake & caps
// ===========================================================================

#[tokio::test]
async fn should_complete_handshake_and_report_caps_when_connecting_to_server() {
    // Given a running server
    let server = TestServer::start(None, None, None).await;

    // When the adapter connects with an approval/question allowlist
    let adapter = RemoteAdapter::connect(&server.ws_rpc_url(), None, &["approval", "question"])
        .await
        .expect("handshake completes");

    // Then the server_hello facts are readable through caps()
    let caps = adapter.caps().expect("caps recorded from server_hello");
    assert!(!caps.server_id.is_empty(), "server id from server_hello");
    assert_eq!(caps.version, "0.1.0-test");
    assert_eq!(caps.heartbeat_ms, 15000);
    assert!(caps.event_batching, "the server advertises eventBatching");

    // And the connection serves requests (the client_hello was accepted)
    let sessions = adapter
        .call_method("session.list", json!({}))
        .await
        .expect("session.list succeeds");
    // W5: the engine's `session.list` answers the page envelope — a
    // non-empty `items` array is the list.
    assert!(
        sessions["items"].is_array(),
        "session.list returns its items page: {sessions}"
    );

    adapter.dispose().await.expect("dispose succeeds");
}

// ===========================================================================
// Forward RPC: results and error mapping
// ===========================================================================

#[tokio::test]
async fn should_map_results_and_errors_when_server_answers_requests() {
    // Given a connected adapter
    let server = TestServer::start(None, None, None).await;
    let adapter = RemoteAdapter::connect(&server.ws_rpc_url(), None, &["approval", "question"])
        .await
        .expect("connect");

    // When a registered method is called, its result comes back
    let sessions = adapter
        .call_method("session.list", json!({}))
        .await
        .expect("session.list succeeds");
    // W5: the engine's `session.list` answers the page envelope — a
    // non-empty `items` array is the list.
    assert!(
        sessions["items"].is_array(),
        "session.list returns its items page: {sessions}"
    );

    // When an unknown method is called, it maps to MethodNotFound
    let error = adapter
        .call_method("no.such.method", json!({}))
        .await
        .expect_err("unknown method fails");
    assert!(
        matches!(error, EngineError::MethodNotFound(ref method) if method == "no.such.method"),
        "unknown method maps to MethodNotFound: {error:?}"
    );

    // When a domain error is returned, it maps to Rpc{code,message,data}
    let error = adapter
        .call_method("session.close", json!({ "sessionId": "does-not-exist" }))
        .await
        .expect_err("closing an unknown session fails");
    match error {
        EngineError::Rpc {
            code,
            message,
            data,
        } => {
            assert_eq!(code, -32001, "session.not_found maps to -32001");
            assert!(!message.is_empty(), "rpc error carries the server message");
            assert_eq!(
                data.and_then(|d| d.get("domainCode").cloned()),
                Some(json!("session.not_found")),
                "rpc error carries the domain code"
            );
        }
        other => panic!("expected an Rpc error, got {other:?}"),
    }

    adapter.dispose().await.expect("dispose succeeds");
}

// ===========================================================================
// Events: subscribe-before-create ordering + envelope shapes
// ===========================================================================

#[tokio::test]
async fn should_stream_events_when_session_created_and_message_sent_after_subscription() {
    // Given a server with a scripted mock LLM and a connected adapter whose
    // event subscription is established BEFORE the session exists
    let mock = Arc::new(MockLlm::new());
    mock.push_text_response("Hello remote world");
    let server = TestServer::start(
        Some(Arc::new(MockLlmFactory::new(Arc::clone(&mock)))),
        None,
        None,
    )
    .await;
    let adapter = RemoteAdapter::connect(&server.ws_rpc_url(), None, &["approval", "question"])
        .await
        .expect("connect");
    let mut events = adapter.subscribe_events().await.expect("subscribe_events");

    // When a session is created (auto-subscribed before the call returns) and
    // a wait-mode message runs its turn
    let created = adapter
        .call_method("session.create", json!({ "title": "event test" }))
        .await
        .expect("session.create succeeds");
    let session_id = created["id"]
        .as_str()
        .expect("session.create returns a session id")
        .to_string();
    let turn = tokio::spawn({
        let adapter = Arc::clone(&adapter);
        let session_id = session_id.clone();
        async move {
            adapter
                .call_method(
                    "message.send",
                    json!({ "sessionId": session_id, "text": "Hello remote world", "mode": "wait" }),
                )
                .await
        }
    });

    // Then the subscriber observes the message echo and the assistant reply,
    // streaming chunk by chunk with the shared envelope shape
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut saw_message_received = false;
    let mut delta_text = String::new();
    while !saw_message_received || delta_text != "Hello remote world" {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for events; got message_received={saw_message_received}, delta_text={delta_text:?}"
        );
        let notification = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("event arrives within the timeout")
            .expect("event channel stays open");
        match notification.method.as_str() {
            "event.message.received" => {
                saw_message_received = true;
                assert_eq!(notification.params["sessionId"], session_id);
                assert_eq!(notification.params["volatile"], false);
                assert_eq!(notification.params["contentSummary"], "Hello remote world");
                assert!(
                    notification.params["seq"]
                        .as_u64()
                        .is_some_and(|seq| seq >= 1),
                    "durable events carry a per-session seq"
                );
                assert!(
                    notification.params["epoch"]
                        .as_str()
                        .is_some_and(|e| !e.is_empty()),
                    "durable events carry the epoch"
                );
                assert!(
                    notification.params["timestamp"].as_str().is_some(),
                    "events carry a timestamp"
                );
            }
            "event.assistant.delta" => {
                // Volatile streaming chunks never occupy cursor space.
                assert_eq!(notification.params["sessionId"], session_id);
                assert_eq!(notification.params["volatile"], true);
                delta_text.push_str(
                    notification.params["delta"]
                        .as_str()
                        .expect("delta params carry the delta text"),
                );
            }
            other => {
                // Durably-published turn events exist on the embedded engine;
                // mirri-server only publishes turn.failed today — ignore the
                // rest of the stream noise.
                tracing::debug!(method = other, "ignoring unrelated event");
            }
        }
    }

    // And the wait-mode call resolves with the completed turn
    let result = tokio::time::timeout(Duration::from_secs(15), turn)
        .await
        .expect("turn call resolves")
        .expect("turn task does not panic")
        .expect("message.send succeeds");
    assert_eq!(result["ok"], true);
    assert_eq!(result["turn"]["stopReason"], "end_turn");
    assert_eq!(mock.chat_call_count(), 1);

    adapter.dispose().await.expect("dispose succeeds");
}

// ===========================================================================
// Reverse RPC: approval round trips
// ===========================================================================

/// Create a session through the adapter and return its id. Titled: these
/// tests count the mock LLM's calls, and an untitled session's settle would
/// spawn the side title call too.
async fn create_session(adapter: &Arc<RemoteAdapter>) -> String {
    let created = adapter
        .call_method("session.create", json!({ "title": "remote adapter" }))
        .await
        .expect("session.create succeeds");
    created["id"]
        .as_str()
        .expect("session.create returns a session id")
        .to_string()
}

#[tokio::test]
async fn should_complete_turn_when_reverse_rpc_approval_is_approved() {
    // Given a server with a mock LLM scripted to call Bash then end the turn
    let mock = Arc::new(MockLlm::new());
    mock.push_tool_use_response(vec![mirri_core::LlmToolCall {
        id: "call_1".to_string(),
        name: "Bash".to_string(),
        args: json!({ "command": "echo hi" }),
    }]);
    mock.push_text_response("done");
    let server = TestServer::start(
        Some(Arc::new(MockLlmFactory::new(Arc::clone(&mock)))),
        None,
        None,
    )
    .await;
    let adapter = RemoteAdapter::connect(&server.ws_rpc_url(), None, &["approval", "question"])
        .await
        .expect("connect");

    // And a reverse-RPC handler that approves every approval.ask
    let requests = Arc::new(Mutex::new(Vec::<(String, Value)>::new()));
    adapter
        .on_reverse_rpc(scripted_reverse_rpc_handler(
            &requests,
            json!({ "decision": "approved" }),
        ))
        .await
        .expect("handler registered");
    let session_id = create_session(&adapter).await;

    // The auto-subscribe after session.create wired the WS transport's
    // reverse-RPC callback onto the session's artifacts, so the engine can
    // ask this connection for approval during its turns.
    let artifacts = server
        .state
        .session_manager
        .session_artifacts(&SessionId::new(&session_id))
        .expect("session artifacts");
    assert!(
        artifacts
            .reverse_rpc_callback
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_some(),
        "auto-subscribe must wire the reverse rpc callback"
    );

    // When a fire-mode message runs the tool-using turn (WAIT mode blocks
    // the connection loop server-side until the turn ends, so the approval
    // request could never be delivered — fire mode keeps the loop free)
    let sent = adapter
        .call_method(
            "message.send",
            json!({ "sessionId": session_id, "text": "run the tool", "mode": "fire" }),
        )
        .await
        .expect("fire mode returns immediately");
    assert_eq!(sent["ok"], true);
    assert!(
        sent["turn_id"].as_str().is_some(),
        "fire mode returns a turn id: {sent}"
    );

    // Then the approval request arrives over reverse RPC
    wait_for_reverse_rpc(&requests).await;
    let recorded = requests.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert_eq!(recorded.len(), 1, "exactly one approval was asked");
    assert_eq!(recorded[0].0, "approval.ask");
    assert_eq!(recorded[0].1["tool_name"], "Bash");
    assert_eq!(recorded[0].1["tool_call_id"], "call_1");

    // And the turn completes in the background: the approved tool ran and
    // the second (end-turn) LLM answer landed in context memory
    wait_for_turn_evidence(&server, &session_id, &mock, 2, "done").await;
    assert_eq!(mock.chat_call_count(), 2, "tool-use then end-turn answers");

    adapter.dispose().await.expect("dispose succeeds");
}

#[tokio::test]
async fn should_block_tool_and_complete_turn_when_reverse_rpc_approval_is_rejected() {
    // Given a server with a mock LLM scripted to call Bash then end the turn
    let mock = Arc::new(MockLlm::new());
    mock.push_tool_use_response(vec![mirri_core::LlmToolCall {
        id: "call_1".to_string(),
        name: "Bash".to_string(),
        args: json!({ "command": "echo hi" }),
    }]);
    mock.push_text_response("after the block");
    let server = TestServer::start(
        Some(Arc::new(MockLlmFactory::new(Arc::clone(&mock)))),
        None,
        None,
    )
    .await;
    let adapter = RemoteAdapter::connect(&server.ws_rpc_url(), None, &["approval", "question"])
        .await
        .expect("connect");

    // And a reverse-RPC handler that rejects with a feedback string
    let requests = Arc::new(Mutex::new(Vec::<(String, Value)>::new()));
    adapter
        .on_reverse_rpc(scripted_reverse_rpc_handler(
            &requests,
            json!({ "decision": "rejected", "feedback": "denied by user" }),
        ))
        .await
        .expect("handler registered");
    let session_id = create_session(&adapter).await;

    // When a fire-mode message runs the tool-using turn
    let sent = adapter
        .call_method(
            "message.send",
            json!({ "sessionId": session_id, "text": "run the tool", "mode": "fire" }),
        )
        .await
        .expect("fire mode returns immediately");
    assert_eq!(sent["ok"], true);

    // Then the approval request arrives over reverse RPC and is rejected
    wait_for_reverse_rpc(&requests).await;
    let recorded = requests.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert_eq!(recorded.len(), 1, "exactly one approval was asked");
    assert_eq!(recorded[0].0, "approval.ask");
    assert_eq!(recorded[0].1["tool_name"], "Bash");

    // And the turn completes with the tool blocked: the feedback is the
    // tool result in context memory, and the end-turn LLM answer followed
    wait_for_turn_evidence(&server, &session_id, &mock, 2, "after the block").await;
    assert!(memory_contains(&server, &session_id, "denied by user").await);
    assert_eq!(mock.chat_call_count(), 2, "tool-use then end-turn answers");

    adapter.dispose().await.expect("dispose succeeds");
}

/// Wait (with a deadline) for at least one reverse-RPC request to be
/// recorded by the scripted handler.
async fn wait_for_reverse_rpc(requests: &Arc<Mutex<Vec<(String, Value)>>>) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let recorded = requests.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if !recorded.is_empty() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for a reverse RPC request"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Whether any message in the session's main-agent context memory contains
/// `needle` in its text content.
async fn memory_contains(server: &TestServer, session_id: &str, needle: &str) -> bool {
    let Some(session) = server
        .state
        .session_manager
        .get_session(&SessionId::new(session_id))
        .await
    else {
        return false;
    };
    let Some(agent) = session.get_agent(&AgentId::new("main")).await else {
        return false;
    };
    agent
        .context_memory()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .messages()
        .iter()
        .any(|message| message.text_content().contains(needle))
}

/// Wait (with a deadline) until a fire-mode turn reaches its end state: the
/// mock was consulted `expected_chats` times and the context memory contains
/// the final assistant reply `evidence`.
async fn wait_for_turn_evidence(
    server: &TestServer,
    session_id: &str,
    mock: &MockLlm,
    expected_chats: u64,
    evidence: &str,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        let chats = mock.chat_call_count();
        let settled =
            chats >= expected_chats && memory_contains(server, session_id, evidence).await;
        if settled {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for the fire-mode turn to complete: chats={chats}, expected={expected_chats}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ===========================================================================
// Heartbeat
// ===========================================================================

#[tokio::test]
async fn should_stay_alive_across_heartbeats_when_pong_echoes_the_server() {
    // Given a server heartbeating every 250ms (a silent client is dropped
    // after 3 × 250ms) and a connected adapter. The windows are deliberately
    // wider than the minimum: a 100ms/300ms window flakes on loaded CI
    // runners, where a legitimate handshake can outlast the drop threshold.
    let server = TestServer::start(None, None, Some(Duration::from_millis(250))).await;
    let adapter = RemoteAdapter::connect(&server.ws_rpc_url(), None, &["approval", "question"])
        .await
        .expect("connect");

    // When the adapter idles through many beat windows (12 beats — four
    // times the server's drop threshold)
    tokio::time::sleep(Duration::from_millis(3000)).await;

    // Then the connection is still alive: the pong echoes kept the server
    // from dropping it, and the client's own liveness deadline (3 × 100ms)
    // was refreshed by the server's pings
    let sessions = adapter
        .call_method("session.list", json!({}))
        .await
        .expect("connection survives the heartbeat windows");
    // W5: the engine's `session.list` answers the page envelope.
    assert!(
        sessions["items"].is_array(),
        "session.list still works: {sessions}"
    );

    adapter.dispose().await.expect("dispose succeeds");
}

// ===========================================================================
// Authentication
// ===========================================================================

#[tokio::test]
async fn should_connect_with_matching_bearer_token_and_reject_missing_or_wrong_tokens() {
    // Given a server that requires a bearer token
    let server = TestServer::start(None, Some("test-secret".to_string()), None).await;

    // When the adapter offers the matching token, the handshake succeeds
    let adapter = RemoteAdapter::connect(
        &server.ws_rpc_url(),
        Some("test-secret"),
        &["approval", "question"],
    )
    .await
    .expect("matching bearer token connects");
    assert!(adapter.caps().is_some(), "handshake completed");
    adapter.dispose().await.expect("dispose succeeds");

    // A missing token is rejected with the same 401 as a wrong one
    let error = RemoteAdapter::connect(&server.ws_rpc_url(), None, &["approval", "question"])
        .await
        .err()
        .expect("missing token is rejected");
    assert!(
        matches!(&error, EngineError::Transport(message) if message.contains("401")),
        "missing token maps to a 401 transport error: {error:?}"
    );

    let error = RemoteAdapter::connect(
        &server.ws_rpc_url(),
        Some("wrong-token"),
        &["approval", "question"],
    )
    .await
    .err()
    .expect("wrong token is rejected");
    assert!(
        matches!(&error, EngineError::Transport(message) if message.contains("401")),
        "wrong token maps to a 401 transport error: {error:?}"
    );
}

// ===========================================================================
// Batched notification arrays (client-side flattening)
// ===========================================================================

#[tokio::test]
async fn should_flatten_batched_notifications_when_peer_sends_an_array_frame() {
    // Given a scripted peer whose `test.blast` answers with a batched array
    // frame (the server's `eventBatching` wire form) plus the response, and
    // a connected adapter with an event subscriber
    let peer = ScriptedPeer::start().await;
    let adapter = RemoteAdapter::connect(&peer.url(), None, &["approval", "question"])
        .await
        .expect("connect");
    let mut events = adapter.subscribe_events().await.expect("subscribe_events");

    // When a call trips the peer's batched answer
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        adapter.call_method("test.blast", json!({})),
    )
    .await
    .expect("test.blast succeeds")
    .expect("test.blast succeeds");
    assert_eq!(result["blasted"], true, "the response is correlated by id");

    // Then the batched members reach the subscriber individually, in order
    let first = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("first batched event arrives")
        .expect("event channel open");
    assert_eq!(first.method, "event.turn.started");
    assert_eq!(first.params["sessionId"], "s1");
    assert_eq!(first.params["seq"], 1);

    let second = tokio::time::timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("second batched event arrives")
        .expect("event channel open");
    assert_eq!(second.method, "event.assistant.delta");
    assert_eq!(second.params["delta"], "hi");

    adapter.dispose().await.expect("dispose succeeds");
}

// ===========================================================================
// Dispose & connection death
// ===========================================================================

#[tokio::test]
async fn should_fail_pending_and_subsequent_calls_when_disposed() {
    // Given a connected adapter with a call pending against a peer that
    // never answers it
    let peer = ScriptedPeer::start().await;
    let adapter = RemoteAdapter::connect(&peer.url(), None, &["approval", "question"])
        .await
        .expect("connect");
    let pending = tokio::spawn({
        let adapter = Arc::clone(&adapter);
        async move { adapter.call_method("slow.ping", json!({})).await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    // When the adapter is disposed
    adapter.dispose().await.expect("dispose succeeds");

    // Then the pending call fails with NotConnected
    let error = tokio::time::timeout(Duration::from_secs(5), pending)
        .await
        .expect("pending call resolves on dispose")
        .expect("pending task does not panic")
        .expect_err("pending call fails");
    assert!(
        matches!(error, EngineError::NotConnected),
        "pending call fails with NotConnected: {error:?}"
    );

    // And subsequent calls fail the same way
    let error = adapter
        .call_method("session.list", json!({}))
        .await
        .expect_err("subsequent call fails");
    assert!(
        matches!(error, EngineError::NotConnected),
        "subsequent call fails with NotConnected: {error:?}"
    );

    // And dispose is idempotent
    adapter.dispose().await.expect("second dispose succeeds");
}

#[tokio::test]
async fn should_fail_pending_with_transport_error_when_connection_dies() {
    // Given a connected adapter with a call pending against a peer that
    // never answers it
    let peer = ScriptedPeer::start().await;
    let adapter = RemoteAdapter::connect(&peer.url(), None, &["approval", "question"])
        .await
        .expect("connect");
    let pending = tokio::spawn({
        let adapter = Arc::clone(&adapter);
        async move { adapter.call_method("slow.ping", json!({})).await }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    // When the peer drops the connection
    peer.kill();

    // Then the pending call fails with a transport error naming the close
    let error = tokio::time::timeout(Duration::from_secs(5), pending)
        .await
        .expect("pending call resolves on connection death")
        .expect("pending task does not panic")
        .expect_err("pending call fails");
    assert!(
        matches!(&error, EngineError::Transport(message) if message == "connection closed"),
        "pending call fails with Transport(\"connection closed\"): {error:?}"
    );

    // And subsequent calls fail with NotConnected
    let error = adapter
        .call_method("session.list", json!({}))
        .await
        .expect_err("call after death fails");
    assert!(
        matches!(error, EngineError::NotConnected),
        "call after death fails with NotConnected: {error:?}"
    );
}

// ===========================================================================
// Subscribe policy (design 14 D5)
// ===========================================================================

#[tokio::test]
async fn should_not_subscribe_any_existing_session_when_connected_with_own_sessions_only_policy() {
    // Given a scripted peer with two already-persisted sessions and a spy
    // recording every `session.subscribe` call
    let peer = ScriptedPeer::with_sessions(&["sess-a", "sess-b"]).await;

    // When an adapter connects with the OwnSessionsOnly policy
    let adapter = RemoteAdapter::connect_with_policy(
        &peer.url(),
        None,
        &["approval"],
        SubscribePolicy::OwnSessionsOnly,
    )
    .await
    .expect("connect");

    // Then no session.subscribe call was recorded during the handshake
    // (settle window: the skipped block is observable only once the peer has
    // had a chance to NOT receive anything)
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        peer.subscribe_calls().is_empty(),
        "OwnSessionsOnly must not subscribe to existing sessions at connect time: {:?}",
        peer.subscribe_calls()
    );

    // And a session-creating call through the adapter still subscribes
    // (create → subscribe-before-complete)
    let _ = adapter
        .call_method("session.create", json!({}))
        .await
        .expect("session.create succeeds");
    assert_eq!(
        peer.subscribe_calls(),
        vec!["sess-created".to_string()],
        "session.create must still auto-subscribe under OwnSessionsOnly"
    );

    adapter.dispose().await.expect("dispose succeeds");
}

#[tokio::test]
async fn should_subscribe_every_existing_session_when_connected_with_all_sessions_policy() {
    // Given a scripted peer with two already-persisted sessions
    let peer = ScriptedPeer::with_sessions(&["sess-a", "sess-b"]).await;

    // When an adapter connects with the default AllSessions policy
    let adapter = RemoteAdapter::connect_with_policy(
        &peer.url(),
        None,
        &["approval"],
        SubscribePolicy::AllSessions,
    )
    .await
    .expect("connect");

    // Then every listed session was subscribed at connect time (the
    // unchanged default semantics)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while peer.subscribe_calls().len() < 2 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for connect-time auto-subscriptions"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let mut subscribed = peer.subscribe_calls();
    subscribed.sort();
    assert_eq!(
        subscribed,
        vec!["sess-a".to_string(), "sess-b".to_string()],
        "AllSessions subscribes every listed session"
    );

    adapter.dispose().await.expect("dispose succeeds");
}

// ===========================================================================
// Touch-to-subscribe, forkSession trigger, unsubscribe (design 14 D5)
// ===========================================================================

/// Connect an OwnSessionsOnly adapter against a scripted peer — the
/// shared-desktop-server shape every touch-to-subscribe test starts from.
async fn own_sessions_adapter(
    peer: &ScriptedPeer,
) -> Arc<RemoteAdapter> {
    RemoteAdapter::connect_with_policy(&peer.url(), None, &["approval"], SubscribePolicy::OwnSessionsOnly)
        .await
        .expect("connect")
}

#[tokio::test]
async fn should_subscribe_on_first_touch_of_an_unsubscribed_session_under_own_sessions_only() {
    // Given an OwnSessionsOnly adapter and a persisted session "sess-old"
    // (never created through this adapter) that the fake peer knows
    let peer = ScriptedPeer::with_sessions(&["sess-old"]).await;
    let adapter = own_sessions_adapter(&peer).await;

    // When a session-addressed call touches it (session.get)
    let result = adapter
        .call_method("session.get", json!({ "sessionId": "sess-old" }))
        .await
        .expect("session.get succeeds");
    assert_eq!(result["id"], "sess-old");

    // Then session.subscribe for sess-old was sent BEFORE the call itself
    assert_eq!(
        peer.method_calls(),
        vec!["session.subscribe".to_string(), "session.get".to_string()],
        "the touch-subscribe must precede the touched call on the wire"
    );
    assert_eq!(
        peer.subscribe_calls(),
        vec!["sess-old".to_string()],
        "the first touch subscribes the session"
    );

    // And a second session-addressed call does not subscribe again
    let _ = adapter
        .call_method("session.get", json!({ "sessionId": "sess-old" }))
        .await
        .expect("second session.get succeeds");
    assert_eq!(
        peer.method_calls(),
        vec![
            "session.subscribe".to_string(),
            "session.get".to_string(),
            "session.get".to_string(),
        ],
        "the second touch must not re-subscribe"
    );

    adapter.dispose().await.expect("dispose succeeds");
}

#[tokio::test]
async fn should_not_subscribe_on_touch_when_the_call_has_no_session_id() {
    // Given an OwnSessionsOnly adapter against a peer with a persisted session
    let peer = ScriptedPeer::with_sessions(&["sess-old"]).await;
    let adapter = own_sessions_adapter(&peer).await;

    // When a non-session-addressed call runs (session.list)
    let _ = adapter
        .call_method("session.list", json!({}))
        .await
        .expect("session.list succeeds");

    // Then no subscribe was recorded and no touch-subscribe preceded the call
    assert!(
        peer.subscribe_calls().is_empty(),
        "a call without a session id must not trip a subscribe"
    );
    assert_eq!(
        peer.method_calls(),
        vec!["session.list".to_string()],
        "session.list must be the only wire call"
    );

    // And an empty-string session id is treated as no addressing either
    let _ = adapter
        .call_method("session.get", json!({ "sessionId": "" }))
        .await
        .expect("empty-id call still completes");
    assert!(
        peer.subscribe_calls().is_empty(),
        "an empty session id must not trip a subscribe"
    );

    adapter.dispose().await.expect("dispose succeeds");
}

#[tokio::test]
async fn should_send_unsubscribe_and_clear_tracking_when_unsubscribe_is_called() {
    // Given an adapter subscribed to sess-created via session.create
    let peer = ScriptedPeer::start().await;
    let adapter = own_sessions_adapter(&peer).await;
    adapter
        .call_method("session.create", json!({}))
        .await
        .expect("session.create succeeds");
    assert_eq!(
        peer.subscribe_calls(),
        vec!["sess-created".to_string()],
        "the create path subscribed before completing"
    );

    // When unsubscribe("sess-created") is called
    adapter
        .unsubscribe("sess-created")
        .await
        .expect("unsubscribe succeeds");

    // Then the server saw session.unsubscribe for that session
    assert_eq!(
        peer.method_calls(),
        vec![
            "session.create".to_string(),
            "session.subscribe".to_string(),
            "session.unsubscribe".to_string(),
        ],
        "the wire order is create → auto-subscribe → unsubscribe"
    );

    // And a subsequent session-addressed call re-subscribes first
    let _ = adapter
        .call_method("session.get", json!({ "sessionId": "sess-created" }))
        .await
        .expect("post-unsubscribe touch completes");
    assert_eq!(
        peer.subscribe_calls(),
        vec!["sess-created".to_string(), "sess-created".to_string()],
        "the touch after an unsubscribe re-subscribes"
    );

    adapter.dispose().await.expect("dispose succeeds");
}

#[tokio::test]
async fn should_auto_subscribe_a_forked_session_created_via_fork_session_method() {
    // Given an OwnSessionsOnly adapter (any prior subscription state)
    let peer = ScriptedPeer::start().await;
    let adapter = own_sessions_adapter(&peer).await;

    // When call_method("forkSession", {sessionId: ...}) completes
    let forked = adapter
        .call_method(
            "forkSession",
            json!({ "sessionId": "sess-parent", "title": "Forked Copy" }),
        )
        .await
        .expect("forkSession succeeds");
    assert_eq!(forked["id"], "sess-forked", "the peer answered the fork id");

    // Then the forked session id was subscribed before the response was
    // delivered (the subscribe-before-complete arm resolves the returned id)
    assert_eq!(
        peer.subscribe_calls(),
        vec!["sess-forked".to_string()],
        "forkSession must auto-subscribe the forked session"
    );
    assert_eq!(
        peer.method_calls(),
        vec!["forkSession".to_string(), "session.subscribe".to_string()],
        "the subscribe precedes the caller's completion"
    );

    adapter.dispose().await.expect("dispose succeeds");
}

#[tokio::test]
async fn should_hand_over_the_approval_route_when_a_second_subscriber_subscribes_the_same_session() {
    // Design 14 D5 / acceptance 8: the server-side "last subscriber wins"
    // routing must not regress when ACP hosts stop mass-subscribing.
    //
    // Given a REAL server with a mock LLM scripted to call Bash then end the
    // turn, and two OwnSessionsOnly adapters (A and B) with reverse-RPC
    // handlers recording every ask they receive
    let mock = Arc::new(MockLlm::new());
    mock.push_tool_use_response(vec![mirri_core::LlmToolCall {
        id: "call_1".to_string(),
        name: "Bash".to_string(),
        args: json!({ "command": "echo hi" }),
    }]);
    mock.push_text_response("done");
    let server = TestServer::start(
        Some(Arc::new(MockLlmFactory::new(Arc::clone(&mock)))),
        None,
        None,
    )
    .await;
    let adapter_a = RemoteAdapter::connect_with_policy(
        &server.ws_rpc_url(),
        None,
        &["approval", "question"],
        SubscribePolicy::OwnSessionsOnly,
    )
    .await
    .expect("adapter A connects");
    let asks_a = Arc::new(Mutex::new(Vec::<(String, Value)>::new()));
    adapter_a
        .on_reverse_rpc(scripted_reverse_rpc_handler(
            &asks_a,
            json!({ "decision": "approved" }),
        ))
        .await
        .expect("A registers its handler");

    // And a session created through adapter A (A is its only subscriber)
    let session_id = create_session(&adapter_a).await;

    // And adapter B connects (also OwnSessionsOnly — no mass subscribe) and
    // subscribes the same session — the desktop-UI-shaped second subscriber
    let adapter_b = RemoteAdapter::connect_with_policy(
        &server.ws_rpc_url(),
        None,
        &["approval", "question"],
        SubscribePolicy::OwnSessionsOnly,
    )
    .await
    .expect("adapter B connects");
    let asks_b = Arc::new(Mutex::new(Vec::<(String, Value)>::new()));
    adapter_b
        .on_reverse_rpc(scripted_reverse_rpc_handler(
            &asks_b,
            json!({ "decision": "approved" }),
        ))
        .await
        .expect("B registers its handler");
    let _ = adapter_b
        .call_method("session.get", json!({ "sessionId": session_id }))
        .await
        .expect("B touches the session (touch-to-subscribe)");

    // When the engine-side approval.ask fires for the session (a fire-mode
    // tool-using turn driven through adapter A)
    let sent = adapter_a
        .call_method(
            "message.send",
            json!({ "sessionId": session_id, "text": "run the tool", "mode": "fire" }),
        )
        .await
        .expect("fire mode returns immediately");
    assert_eq!(sent["ok"], true);

    // Then the ask routed to adapter B's reverse-RPC channel (the last
    // subscriber), and adapter A's channel received nothing (no double-ask)
    wait_for_reverse_rpc(&asks_b).await;
    let recorded_b = asks_b.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert_eq!(recorded_b.len(), 1, "exactly one ask reached B");
    assert_eq!(recorded_b[0].0, "approval.ask");
    assert_eq!(recorded_b[0].1["tool_name"], "Bash");
    let recorded_a = asks_a.lock().unwrap_or_else(|e| e.into_inner()).clone();
    assert!(
        recorded_a.is_empty(),
        "the former subscriber must not receive the ask: {recorded_a:?}"
    );

    // And B answering it completes the approval round trip (the turn ends
    // with the tool's evidence — the approved Bash call ran)
    wait_for_turn_evidence(&server, &session_id, &mock, 2, "done").await;
    assert_eq!(mock.chat_call_count(), 2, "tool-use then end-turn answers");

    adapter_a.dispose().await.expect("A disposes");
    adapter_b.dispose().await.expect("B disposes");
}


