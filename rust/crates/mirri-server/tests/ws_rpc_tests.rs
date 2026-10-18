//! Integration tests for the unified JSON-RPC 2.0 WebSocket endpoint
//! (`/api/v1/rpc`).
//!
//! Clients connect with `tokio-tungstenite`, complete the `server_hello` /
//! `client_hello` handshake, then exercise forward RPC, event subscription
//! with cursor replay, heartbeat ping/pong, and bearer-subprotocol auth.
//! Naming follows the Given-When-Then convention.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::TestServer;
use futures::{SinkExt, StreamExt};
use mirri_core::{AgentId, Role, SessionId};
use mirri_test_harness::{MockLlm, MockLlmFactory, MockResponse};
use mirri_transport::adapter::{EngineAdapter, EngineError};
use mirri_transport::frame::JsonRpcNotification;
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::{
    Message,
    client::IntoClientRequest,
    http::{HeaderValue, StatusCode},
};

type TestSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Create a session through the HTTP API and return its id.
async fn create_session(server: &TestServer, title: &str) -> String {
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{}/api/v1/sessions", server.base_url()))
        .json(&json!({ "title": title }))
        .send()
        .await
        .expect("create session");
    let body: Value = response.json().await.expect("JSON body");
    body["data"]["id"].as_str().expect("session id").to_string()
}

/// Connect to the unified JSON-RPC WebSocket endpoint.
async fn connect_ws(server: &TestServer) -> TestSocket {
    connect_async(server.ws_rpc_url())
        .await
        .expect("websocket upgrade")
        .0
}

/// Connect offering the given `Sec-WebSocket-Protocol` value.
async fn connect_ws_with_subprotocol(server: &TestServer, subprotocol: &str) -> TestSocket {
    let mut request = server
        .ws_rpc_url()
        .into_client_request()
        .expect("build connect request");
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_str(subprotocol).expect("valid subprotocol header"),
    );
    connect_async(request).await.expect("websocket upgrade").0
}

/// Attempt to connect; assert the upgrade is rejected, and return the HTTP
/// status of the rejection.
async fn connect_expect_rejected(server: &TestServer, subprotocol: Option<&str>) -> StatusCode {
    let mut request = server
        .ws_rpc_url()
        .into_client_request()
        .expect("build connect request");
    if let Some(subprotocol) = subprotocol {
        request.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            HeaderValue::from_str(subprotocol).expect("valid subprotocol header"),
        );
    }
    match connect_async(request).await {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => response.status(),
        other => panic!("expected an HTTP rejection, got: {other:?}"),
    }
}

/// Read the next text frame, parsing it into one or more JSON values
/// (a batched array frame is flattened into its members).
async fn next_frames(socket: &mut TestSocket) -> Vec<Value> {
    let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("message arrives within the timeout")
        .expect("stream stays open")
        .expect("text message");
    let value: Value = serde_json::from_str(message.to_text().expect("UTF-8 text message"))
        .expect("message is valid JSON");
    match value {
        Value::Array(frames) => frames,
        other => vec![other],
    }
}

/// Read the next single frame (panics if the server batched several).
async fn next_frame(socket: &mut TestSocket) -> Value {
    let mut frames = next_frames(socket).await;
    assert_eq!(frames.len(), 1, "expected a single frame, got {frames:?}");
    frames.remove(0)
}

/// Read frames until the response with the given id arrives; returns it,
/// skipping any interleaved notifications (e.g. heartbeats).
async fn next_response(socket: &mut TestSocket, id: i64) -> Value {
    loop {
        for frame in next_frames(socket).await {
            if frame.get("id").and_then(Value::as_i64) == Some(id) {
                return frame;
            }
        }
    }
}

/// Read frames until `count` `event.*` notifications have been collected;
/// returns them in arrival order.
async fn collect_events(socket: &mut TestSocket, count: usize) -> Vec<Value> {
    let mut events = Vec::new();
    while events.len() < count {
        for frame in next_frames(socket).await {
            if frame
                .get("method")
                .and_then(Value::as_str)
                .is_some_and(|method| method.starts_with("event."))
            {
                events.push(frame);
            }
        }
    }
    events
}

/// Assert the `server_hello` greeting, answer with a `client_hello` request,
/// and assert the accepted response. Returns the greeting frame.
async fn complete_handshake(socket: &mut TestSocket) -> Value {
    let hello = next_frame(socket).await;
    assert_eq!(
        hello["method"], "server_hello",
        "first frame is the server greeting"
    );
    let hello_params = &hello["params"];
    assert!(
        hello_params["serverId"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "server_hello carries a serverId"
    );
    assert!(
        hello_params["version"]
            .as_str()
            .is_some_and(|version| !version.is_empty()),
        "server_hello carries a version"
    );
    assert_eq!(hello_params["capabilities"]["eventBatching"], true);
    assert_eq!(hello_params["maxEventBufferSize"], 256);
    socket
        .send(Message::Text(
            r#"{"id":1,"method":"client_hello","params":{"clientId":"test-client","capabilities":{"reverseRpc":["approval","question","fs","terminal"]}}}"#
                .into(),
        ))
        .await
        .expect("send client_hello");
    let ack = next_response(socket, 1).await;
    assert_eq!(ack["result"]["accepted"], true, "handshake accepted: {ack}");
    hello
}

/// Connect and complete the handshake.
async fn connected_socket(server: &TestServer) -> TestSocket {
    let mut socket = connect_ws(server).await;
    complete_handshake(&mut socket).await;
    socket
}

/// Send a `session.subscribe` request and return the server's response.
async fn subscribe(socket: &mut TestSocket, session_id: &str, id: i64) -> Value {
    socket
        .send(Message::Text(
            json!({ "id": id, "method": "session.subscribe", "params": { "sessionId": session_id } })
                .to_string()
                .into(),
        ))
        .await
        .expect("send subscribe");
    next_response(socket, id).await
}

/// Publish a `message_received` event on the given session's event bus.
fn publish_message(server: &TestServer, session_id: &str, summary: &str) {
    let artifacts = server
        .state
        .session_manager
        .session_artifacts(&mirri_core::SessionId::new(session_id.to_string()))
        .expect("session artifacts");
    artifacts
        .event_bus
        .publish(mirri_core::SessionEvent::MessageReceived {
            agent_id: "main".to_string(),
            role: "user".to_string(),
            content_summary: summary.to_string(),
        });
}

/// Extract `(seq, epoch)` from an `event` notification's params.
fn event_position(event: &Value) -> (u64, &str) {
    let params = &event["params"];
    (
        params["seq"].as_u64().expect("event seq"),
        params["epoch"].as_str().expect("event epoch"),
    )
}

// ===========================================================================
// Handshake & auth
// ===========================================================================

#[tokio::test]
async fn should_complete_handshake_when_client_sends_client_hello() {
    // Given a running server and a connected client
    let server = TestServer::start().await;
    let mut socket = connect_ws(&server).await;

    // When the client receives `server_hello` and answers with `client_hello`
    let hello = complete_handshake(&mut socket).await;

    // Then the greeting advertised server identity, heartbeat, and capabilities
    assert_eq!(hello["params"]["heartbeatMs"], 15000);
    assert_eq!(hello["jsonrpc"], "2.0");
}

#[tokio::test]
async fn should_reject_connection_when_auth_token_missing() {
    // Given a server that requires a bearer token
    let server = TestServer::start_with_auth("test-secret").await;

    // When a client connects without the bearer subprotocol...
    let status = connect_expect_rejected(&server, None).await;

    // ...it is rejected with 401
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // A wrong token is rejected identically (design §13.1: same response,
    // no token-validity oracle).
    let wrong_token = connect_expect_rejected(&server, Some("mirri-code.bearer.wrong")).await;
    assert_eq!(wrong_token, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn should_accept_connection_when_bearer_subprotocol_present() {
    // Given a server that requires a bearer token
    let server = TestServer::start_with_auth("test-secret").await;

    // When the client offers the matching bearer subprotocol
    let mut socket = connect_ws_with_subprotocol(&server, "mirri-code.bearer.test-secret").await;

    // Then the handshake completes (and the echo uses the offered protocol)
    complete_handshake(&mut socket).await;
}

/// Connect offering the given `Sec-WebSocket-Protocol` value and return the
/// upgrade response (whose selected-protocol header the caller asserts).
async fn connect_ws_with_subprotocol_response(
    server: &TestServer,
    subprotocol: &str,
) -> (
    TestSocket,
    tokio_tungstenite::tungstenite::http::Response<Option<Vec<u8>>>,
) {
    let mut request = server
        .ws_rpc_url()
        .into_client_request()
        .expect("build connect request");
    request.headers_mut().insert(
        "Sec-WebSocket-Protocol",
        HeaderValue::from_str(subprotocol).expect("valid subprotocol header"),
    );
    let (socket, response) = connect_async(request).await.expect("websocket upgrade");
    (socket, response)
}

#[tokio::test]
async fn should_echo_the_bearer_subprotocol_when_upgrading_in_bypass_auth_mode() {
    // Given a server running WITHOUT an auth token (open mode — the
    // `--dangerous-bypass-auth` shape) and a browser client that still
    // offered the bearer subprotocol (e.g. a stale credential from an
    // earlier auth-mode run)
    let server = TestServer::start().await;

    // When the client offers the bearer subprotocol, then the upgrade
    // completes and the server echoes the offered subprotocol back — a
    // browser aborts the handshake (1006) when its offer goes unanswered
    let (mut socket, response) =
        connect_ws_with_subprotocol_response(&server, "mirri-code.bearer.stale-credential").await;
    assert_eq!(
        response.headers().get("Sec-WebSocket-Protocol"),
        Some(&HeaderValue::from_static(
            "mirri-code.bearer.stale-credential"
        )),
        "the bypass-mode upgrade echoes the offered bearer subprotocol"
    );

    // And the connection is fully usable (the JSON-RPC handshake completes)
    complete_handshake(&mut socket).await;
}

// ===========================================================================
// Forward RPC dispatch
// ===========================================================================

#[tokio::test]
async fn should_dispatch_rpc_method_when_client_sends_request() {
    // Given a connected client
    let server = TestServer::start().await;
    let mut socket = connected_socket(&server).await;

    // When the client sends a registered method (session.list has no params)
    socket
        .send(Message::Text(
            r#"{"jsonrpc":"2.0","id":7,"method":"session.list","params":{}}"#.into(),
        ))
        .await
        .expect("send request");

    // Then the registry's result comes back under the echoed id
    let response = next_response(&mut socket, 7).await;
    assert!(
        response["result"]["items"].is_array(),
        "session.list returns a page"
    );
    assert_eq!(response["id"], 7);
}

#[tokio::test]
async fn should_return_error_when_method_not_found() {
    // Given a connected client
    let server = TestServer::start().await;
    let mut socket = connected_socket(&server).await;

    // When the client calls an unknown method
    socket
        .send(Message::Text(
            r#"{"id":5,"method":"no.such.method","params":{}}"#.into(),
        ))
        .await
        .expect("send request");

    // Then the server answers -32601 under the echoed id
    let response = next_response(&mut socket, 5).await;
    assert_eq!(response["error"]["code"], -32601);
    assert!(
        response["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("no.such.method")),
        "error names the method: {}",
        response["error"]["message"]
    );
}

#[tokio::test]
async fn should_answer_engine_domain_error_with_code_and_domain_code_when_session_unknown() {
    // Given a connected client and no session carrying the addressed id
    let server = TestServer::start().await;
    let mut socket = connected_socket(&server).await;

    // When the client calls session.close (an engine-domain both-method on
    // the serve wire) with an unknown session id
    socket
        .send(Message::Text(
            r#"{"id":6,"method":"session.close","params":{"sessionId":"no-such-session"}}"#.into(),
        ))
        .await
        .expect("send request");

    // Then the engine's session.not_found domain error crosses the adapter
    // seam unmangled: -32001 (the code clients branch on) plus the D5
    // domainCode payload inside error.data
    let response = next_response(&mut socket, 6).await;
    assert_eq!(response["error"]["code"], -32001);
    assert_eq!(
        response["error"]["data"]["domainCode"], "session.not_found",
        "engine domain error keeps its domainCode on the serve wire: {response}"
    );
    assert!(
        response["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("no-such-session")),
        "error names the missing session: {}",
        response["error"]["message"]
    );
}

#[tokio::test]
async fn should_echo_text_over_the_serve_wire_when_core_echo_is_called() {
    // Given a connected client on a plain server (no session needed —
    // core.echo touches no session state)
    let server = TestServer::start().await;
    let mut socket = connected_socket(&server).await;

    // When the client calls core.echo (a both-method the transport registry
    // does not own: it must reach the embedded engine through the shared
    // dispatch's engine fall-through)
    socket
        .send(Message::Text(
            r#"{"id":8,"method":"core.echo","params":{"text":"drill echo"}}"#.into(),
        ))
        .await
        .expect("send request");

    // Then the engine answers with the text under the `echo` key — the
    // M5 drill's zero-transport-code serve wire
    let response = next_response(&mut socket, 8).await;
    assert_eq!(response["result"]["echo"], "drill echo");
    assert_eq!(response["error"], Value::Null);
}

// ===========================================================================
// Event subscription, cursor replay, and batching
// ===========================================================================

#[tokio::test]
async fn should_receive_events_when_subscribed_to_session() {
    // Given a session and a connected, subscribed client
    let server = TestServer::start().await;
    let session_id = create_session(&server, "Event stream").await;
    let mut socket = connected_socket(&server).await;
    let ack = subscribe(&mut socket, &session_id, 2).await;
    assert_eq!(ack["result"]["seq"], 0, "empty log starts at seq 0");
    let epoch = ack["result"]["epoch"].as_str().expect("epoch").to_string();

    // When a message arrives on the session's event bus
    publish_message(&server, &session_id, "hello through events");

    // Then it arrives as an `event.message.received` notification carrying
    // the cursor envelope and the flattened camelCase payload fields
    let events = collect_events(&mut socket, 1).await;
    let event = &events[0];
    assert_eq!(event["method"], "event.message.received");
    assert_eq!(event["params"]["sessionId"], session_id);
    assert_eq!(event["params"]["volatile"], false);
    assert_eq!(event["params"]["contentSummary"], "hello through events");
    assert_eq!(event_position(event), (1, epoch.as_str()));
}

#[tokio::test]
async fn should_replay_missed_events_when_subscribing_with_cursor() {
    // Given a session with events that arrived while the client was away
    let server = TestServer::start().await;
    let session_id = create_session(&server, "Replay").await;
    let mut socket = connected_socket(&server).await;
    let ack = subscribe(&mut socket, &session_id, 2).await;
    let epoch = ack["result"]["epoch"].as_str().expect("epoch").to_string();
    publish_message(&server, &session_id, "before disconnect");
    publish_message(&server, &session_id, "still before disconnect");
    let live = collect_events(&mut socket, 2).await;
    assert_eq!(
        live.iter().map(event_position).collect::<Vec<_>>(),
        vec![(1, epoch.as_str()), (2, epoch.as_str())],
        "live events are stamped with their cursor positions"
    );

    // When the client disconnects (unsubscribes) and two more events arrive...
    socket
        .send(Message::Text(
            json!({ "id": 3, "method": "session.unsubscribe", "params": { "sessionId": session_id } })
                .to_string()
                .into(),
        ))
        .await
        .expect("send unsubscribe");
    assert_eq!(next_response(&mut socket, 3).await["result"]["ok"], true);
    publish_message(&server, &session_id, "after disconnect 1");
    publish_message(&server, &session_id, "after disconnect 2");

    // ...and it resubscribes with its last cursor
    socket
        .send(Message::Text(
            json!({
                "id": 4,
                "method": "session.subscribe",
                "params": {
                    "sessionId": session_id,
                    "cursor": { "seq": 2, "epoch": epoch },
                },
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("send subscribe with cursor");
    let ack = next_response(&mut socket, 4).await;
    assert_eq!(ack["result"]["seq"], 4, "ack reports the new tail");

    // Then the missed events are replayed in order, stamped with their
    // original seqs (possibly coalesced into one batched array frame).
    let replayed = collect_events(&mut socket, 2).await;
    let positions: Vec<(u64, &str)> = replayed.iter().map(event_position).collect();
    assert_eq!(positions, vec![(3, epoch.as_str()), (4, epoch.as_str())]);
    let summaries: Vec<&str> = replayed
        .iter()
        .map(|event| event["params"]["contentSummary"].as_str().expect("summary"))
        .collect();
    assert_eq!(summaries, vec!["after disconnect 1", "after disconnect 2"]);
}

// ===========================================================================
// Heartbeat
// ===========================================================================

#[tokio::test]
async fn should_handle_ping_pong_when_heartbeat_fires() {
    // Given a server heartbeating every 200 ms and a connected client
    let server = TestServer::start_with_heartbeat_interval(Duration::from_millis(200)).await;
    let mut socket = connected_socket(&server).await;

    // When the heartbeat fires
    let ping = next_frame(&mut socket).await;
    assert_eq!(ping["method"], "ping", "server sends a ping notification");
    let nonce = ping["params"]["nonce"].as_str().expect("nonce").to_string();

    // And the client answers with a pong echoing the nonce
    socket
        .send(Message::Text(
            json!({ "method": "pong", "params": { "nonce": nonce } })
                .to_string()
                .into(),
        ))
        .await
        .expect("send pong");

    // Then the connection stays alive and still serves requests
    socket
        .send(Message::Text(
            r#"{"id":9,"method":"session.list","params":{}}"#.into(),
        ))
        .await
        .expect("send request");
    let response = next_response(&mut socket, 9).await;
    assert!(
        response["result"]["items"].is_array(),
        "connection is still alive: {response}"
    );
}

#[tokio::test]
async fn should_answer_client_ping_with_pong_echoing_nonce() {
    // Given a connected client
    let server = TestServer::start().await;
    let mut socket = connected_socket(&server).await;

    // When the client sends a ping notification with a nonce
    socket
        .send(Message::Text(
            r#"{"method":"ping","params":{"nonce":"client-nonce-42"}}"#.into(),
        ))
        .await
        .expect("send ping");

    // Then the server answers with a pong echoing the nonce
    let pong = next_frame(&mut socket).await;
    assert_eq!(pong["method"], "pong");
    assert_eq!(pong["params"]["nonce"], "client-nonce-42");
}

// ===========================================================================
// message.send: fire & wait modes (design §6.2)
// ===========================================================================

/// Send a `message.send` request over the unified RPC socket and return the
/// response.
async fn send_message(
    socket: &mut TestSocket,
    id: i64,
    session_id: &str,
    text: &str,
    mode: &str,
) -> Value {
    socket
        .send(Message::Text(
            json!({
                "id": id,
                "method": "message.send",
                "params": { "sessionId": session_id, "text": text, "mode": mode },
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("send message");
    next_response(socket, id).await
}

#[tokio::test]
async fn should_return_immediately_with_turn_id_when_mode_is_fire() {
    // Given a session (whose eagerly provisioned main agent) on a server
    // wired to a scripted LLM
    let mock = Arc::new(MockLlm::new());
    mock.push_text_response("Hello from the mock LLM");
    let server =
        TestServer::start_with_llm_factory(Some(Arc::new(MockLlmFactory::new(Arc::clone(&mock)))))
            .await;
    let session_id = create_session(&server, "Fire & wait modes").await;
    let mut socket = connected_socket(&server).await;

    // When the client sends a message in fire-and-forget mode
    let response = send_message(&mut socket, 10, &session_id, "Hello async", "fire").await;

    // Then the server answers immediately with the turn id and no `turn`
    // payload — the turn still runs in the background
    let result = &response["result"];
    assert_eq!(result["ok"], true);
    assert_eq!(result["session_id"], session_id);
    assert_eq!(result["agent_id"], "main");
    let turn_id = result["turn_id"].as_str().expect("turn id");
    assert!(
        turn_id.starts_with("turn-"),
        "fire mode returns a UUID-backed turn id: {turn_id}"
    );
    assert!(
        result.get("turn").is_none(),
        "fire mode does not return the turn result: {result}"
    );
    // And the envelope is exactly the snake_case key set — no camelCase
    // leftovers, no extra keys
    let mut keys: Vec<&str> = result
        .as_object()
        .expect("fire result object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, ["agent_id", "ok", "session_id", "turn_id"]);
}

#[tokio::test]
async fn should_return_full_turn_result_when_mode_is_wait() {
    // Given a session (whose eagerly provisioned main agent) on a server
    // wired to a scripted LLM
    let mock = Arc::new(MockLlm::new());
    mock.push_text_response("Hello from the mock LLM");
    let server =
        TestServer::start_with_llm_factory(Some(Arc::new(MockLlmFactory::new(Arc::clone(&mock)))))
            .await;
    let session_id = create_session(&server, "Fire & wait modes").await;
    let mut socket = connected_socket(&server).await;

    // When the client sends a message in wait mode
    let response = send_message(&mut socket, 11, &session_id, "Hello sync", "wait").await;

    // Then the server blocks until the turn ends and returns the full result
    let result = &response["result"];
    assert_eq!(result["ok"], true);
    assert_eq!(result["turn"]["turnId"], 1);
    assert_eq!(result["turn"]["stopReason"], "end_turn");
    assert_eq!(result["turn"]["steps"], 1);
    assert!(
        result["turn"]["usage"].is_object(),
        "the turn view carries a usage sub-view: {}",
        result["turn"]["usage"]
    );
    assert_eq!(result["turn"]["blockedByPromptHook"], false);
}

#[tokio::test]
async fn should_publish_turn_events_when_fire_mode_turn_completes() {
    // Given a session (whose eagerly provisioned main agent) on a server
    // wired to a scripted LLM, and a client subscribed to the session's
    // event stream
    let mock = Arc::new(MockLlm::new());
    mock.push_text_response("Hello from the mock LLM");
    let server =
        TestServer::start_with_llm_factory(Some(Arc::new(MockLlmFactory::new(Arc::clone(&mock)))))
            .await;
    let session_id = create_session(&server, "Fire & wait modes").await;
    let mut socket = connected_socket(&server).await;
    let ack = subscribe(&mut socket, &session_id, 12).await;
    assert_eq!(ack["result"]["seq"], 0, "empty log starts at seq 0");

    // When a message is sent in fire mode. The engine publishes the
    // triggering `message.received` event before the fire-mode call returns,
    // so the event milestone is read BEFORE the response.
    socket
        .send(Message::Text(
            json!({
                "id": 13,
                "method": "message.send",
                "params": { "sessionId": session_id, "text": "Hello fire", "mode": "fire" },
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("send fire message");

    // Then the event.message.received notification streams to the subscriber
    // while the turn runs in the background
    let events = collect_events(&mut socket, 1).await;
    assert_eq!(events[0]["method"], "event.message.received");
    assert_eq!(events[0]["params"]["sessionId"], session_id);
    assert_eq!(events[0]["params"]["contentSummary"], "Hello fire");

    // And the fire-mode response arrives with the launched turn id
    let response = next_response(&mut socket, 13).await;
    let turn_id = response["result"]["turn_id"]
        .as_str()
        .expect("turn id")
        .to_string();
    assert!(turn_id.starts_with("turn-"), "fire mode returns a turn id");

    // And the turn completes in the background: the assistant reply lands in
    // context memory without anyone polling for it
    let session = server
        .state
        .session_manager
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let agent = session
        .get_agent(&AgentId::new("main"))
        .await
        .expect("the main agent exists");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let messages = agent
            .context_memory()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .messages();
        // The step loop may inject user-role system reminders (e.g. the
        // tools_added announcement) between the prompt and the reply, so the
        // reply is asserted as the LAST message rather than messages[1].
        let reply_recorded = messages.last().is_some_and(|last| {
            last.role == Role::Assistant && last.text_content() == "Hello from the mock LLM"
        });
        if !messages.is_empty()
            && messages[0].role == Role::User
            && messages[0].text_content() == "Hello fire"
            && reply_recorded
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "fire-mode turn did not complete in the background"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn should_stream_forked_session_events_when_a_turn_runs_after_fork() {
    // Given a session with a completed turn (history for the fork to
    // carry) on a server wired to a scripted LLM
    let mock = Arc::new(MockLlm::new());
    mock.push_text_response("First reply");
    mock.push_text_response("Hello from the fork");
    let server =
        TestServer::start_with_llm_factory(Some(Arc::new(MockLlmFactory::new(Arc::clone(&mock)))))
            .await;
    let session_id = create_session(&server, "Fork streaming").await;
    let mut socket = connected_socket(&server).await;
    let response = send_message(&mut socket, 10, &session_id, "hi", "wait").await;
    assert_eq!(response["result"]["ok"], true, "source turn completes");

    // And the session is forked over the wire (the web UI's fork action)
    socket
        .send(Message::Text(
            json!({
                "id": 11,
                "method": "session.fork",
                "params": { "sessionId": session_id },
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("send session.fork");
    let forked = next_response(&mut socket, 11).await;
    let forked_id = forked["result"]["id"]
        .as_str()
        .expect("fork returns the new session view")
        .to_string();
    assert_ne!(forked_id, session_id);

    // And the client subscribes to the forked session (selectSession's
    // subscribe) BEFORE sending a message to it
    let ack = subscribe(&mut socket, &forked_id, 12).await;
    assert!(
        ack["result"].get("seq").is_some(),
        "subscribe ack carries a seq: {ack}"
    );

    // When a message is sent to the forked session in fire mode
    socket
        .send(Message::Text(
            json!({
                "id": 13,
                "method": "message.send",
                "params": { "sessionId": forked_id, "text": "hi from the fork", "mode": "fire" },
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("send fork message");

    // Then the forked session's events stream live to the subscriber — the
    // triggering message.received arrives first, and the assistant reply
    // follows without the client refreshing
    let mut saw_received = false;
    let mut fork_reply = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    'collect: loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "forked session events did not stream within 5s (received={saw_received}, reply={fork_reply:?})"
        );
        for frame in next_frames(&mut socket).await {
            let Some(method) = frame.get("method").and_then(Value::as_str) else {
                continue;
            };
            if method != "event.message.received" && method != "event.assistant.delta" {
                continue;
            }
            assert_eq!(
                frame["params"]["sessionId"], forked_id,
                "the fork's events carry the forked session id: {frame}"
            );
            if method == "event.message.received" {
                saw_received = true;
            } else {
                // The fork's turn runs on the session MAIN agent — the web
                // client streams only main-agent transcript frames.
                assert_eq!(
                    frame["params"]["agentId"], "main",
                    "the fork's turn events must carry the main agent id: {frame}"
                );
                fork_reply.push_str(frame["params"]["delta"].as_str().unwrap_or_default());
                if fork_reply == "Hello from the fork" {
                    break 'collect;
                }
            }
        }
    }
    assert!(
        saw_received,
        "the forked session's message.received streamed live"
    );
}

#[tokio::test]
async fn should_report_running_while_fire_mode_turn_is_in_flight_and_idle_after() {
    // Given a server whose engine streams a long mock reply (200 deltas x
    // 25 ms), so the fire-mode turn is still in flight long after the send
    // returns, and a connected client
    let reply = "y".repeat(200);
    let mock = Arc::new(MockLlm::new());
    mock.push_response(MockResponse::streaming(&reply, Duration::from_millis(25)));
    let server =
        TestServer::start_with_llm_factory(Some(Arc::new(MockLlmFactory::new(Arc::clone(&mock)))))
            .await;
    let session_id = create_session(&server, "Running derivation").await;
    let mut socket = connected_socket(&server).await;

    // When a fire-mode message is sent (the engine-call task's entry is
    // registered in the server's in-flight map, keyed
    // `{session}:{agent}:{turn}`)
    let response = send_message(&mut socket, 20, &session_id, "Long stream", "fire").await;
    assert_eq!(response["result"]["ok"], true);

    // Then the shared `session.status` reports the session as running while
    // the turn is in flight (the dispatch wrapper's in-flight tracking)
    socket
        .send(Message::Text(
            json!({ "id": 21, "method": "session.status", "params": { "sessionId": session_id } })
                .to_string()
                .into(),
        ))
        .await
        .expect("send session.status");
    let status = next_response(&mut socket, 21).await;
    assert_eq!(status["result"]["running"], true);

    // And once the turn has completed, the watcher reaped the entry and the
    // session reports idle again
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    loop {
        socket
            .send(Message::Text(
                json!({ "id": 22, "method": "session.status", "params": { "sessionId": session_id } })
                    .to_string()
                    .into(),
            ))
            .await
            .expect("send session.status");
        let status = next_response(&mut socket, 22).await;
        if status["result"]["running"] == false {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "session.status stayed running after the turn ended: {status}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Given a server whose transport-level config declares the default model
/// alias with a context window, When `session.status` is called on the
/// serve wire, Then the result carries the model's `maxContextTokens`
/// ceiling alongside its id (the web composer's context ring divides the
/// session's token count by exactly this field).
#[tokio::test]
async fn should_report_the_models_context_window_when_session_status_is_called() {
    // Given a state config naming the engine's default model with a 500k
    // window, and a created session + connected WS client
    let state_config = Arc::new(std::sync::RwLock::new(mirri_core::ServerConfig {
        models: Some(std::collections::HashMap::from([(
            "test-default-model".to_string(),
            mirri_core::ModelAlias {
                provider: "test".to_string(),
                model: "GLM-test".to_string(),
                max_context_size: 500_000,
                ..mirri_core::ModelAlias::default()
            },
        )])),
        ..mirri_core::ServerConfig::default()
    }));
    let server = TestServer::start_with_state_config(state_config).await;
    let session_id = create_session(&server, "Context window").await;
    let mut socket = connected_socket(&server).await;

    // When the session status is read over the serve wire
    socket
        .send(Message::Text(
            json!({ "id": 51, "method": "session.status", "params": { "sessionId": session_id } })
                .to_string()
                .into(),
        ))
        .await
        .expect("send session.status");
    let status = next_response(&mut socket, 51).await;

    // Then the model's context window is reported from the config — not
    // the unknown-model 0
    assert_eq!(status["result"]["model"], "test-default-model");
    assert_eq!(
        status["result"]["maxContextTokens"], 500_000,
        "session.status must report the configured model window: {status}"
    );
}

/// Given a created session, When `collaboration.enter` / `collaboration.exit`
/// are called on the serve wire (engine fall-through), Then `session.status`
/// reports the `collaborationMode` flag following each transition.
#[tokio::test]
async fn should_report_collaboration_mode_when_collaboration_methods_are_called() {
    // Given a server with a created session and a connected client
    let server = TestServer::start().await;
    let session_id = create_session(&server, "Collaboration mode").await;
    let mut socket = connected_socket(&server).await;

    // Then the status reports collaboration mode inactive before entering
    socket
        .send(Message::Text(
            json!({ "id": 31, "method": "session.status", "params": { "sessionId": session_id } })
                .to_string()
                .into(),
        ))
        .await
        .expect("send session.status");
    let status = next_response(&mut socket, 31).await;
    assert_eq!(status["result"]["collaborationMode"], false);

    // When collaboration mode is entered
    socket
        .send(Message::Text(
            json!({
                "id": 32,
                "method": "collaboration.enter",
                "params": { "sessionId": session_id }
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("send collaboration.enter");
    let entered = next_response(&mut socket, 32).await;
    assert_eq!(entered["result"]["ok"], true, "enter failed: {entered}");

    // Then the status reports collaboration mode active
    socket
        .send(Message::Text(
            json!({ "id": 33, "method": "session.status", "params": { "sessionId": session_id } })
                .to_string()
                .into(),
        ))
        .await
        .expect("send session.status");
    let active = next_response(&mut socket, 33).await;
    assert_eq!(active["result"]["collaborationMode"], true);

    // When collaboration mode is exited
    socket
        .send(Message::Text(
            json!({
                "id": 34,
                "method": "collaboration.exit",
                "params": { "sessionId": session_id }
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("send collaboration.exit");
    let exited = next_response(&mut socket, 34).await;
    assert_eq!(exited["result"]["ok"], true, "exit failed: {exited}");

    // Then the status reports collaboration mode inactive again
    socket
        .send(Message::Text(
            json!({ "id": 35, "method": "session.status", "params": { "sessionId": session_id } })
                .to_string()
                .into(),
        ))
        .await
        .expect("send session.status");
    let after = next_response(&mut socket, 35).await;
    assert_eq!(after["result"]["collaborationMode"], false);
}

// ---------------------------------------------------------------------------
// Terminal stream bridge (the web terminal panel's terminal_* frames)
// ---------------------------------------------------------------------------

/// Create a terminal through the REST API and return its wire object.
async fn create_terminal(server: &TestServer, session_id: &str) -> Value {
    let client = reqwest::Client::new();
    let response = client
        .post(format!(
            "{}/api/v1/sessions/{session_id}/terminals",
            server.base_url()
        ))
        .json(&json!({}))
        .send()
        .await
        .expect("create terminal");
    let body: Value = response.json().await.expect("JSON body");
    assert_eq!(body["code"], 0, "create terminal envelope: {body}");
    body["data"].clone()
}

/// Send one `terminal_*` control frame and return the server's response.
async fn terminal_call(socket: &mut TestSocket, id: i64, method: &str, params: Value) -> Value {
    socket
        .send(Message::Text(
            json!({ "id": id, "method": method, "params": params })
                .to_string()
                .into(),
        ))
        .await
        .expect("send terminal frame");
    next_response(socket, id).await
}

/// Read frames until a `terminal_output` notification whose `data` contains
/// `needle` arrives; returns that frame (panics past the deadline).
async fn await_terminal_output(socket: &mut TestSocket, needle: &str) -> Value {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "no terminal_output frame carried {needle:?}"
        );
        for frame in next_frames(socket).await {
            if frame["method"] == "terminal_output"
                && frame["params"]["data"]
                    .as_str()
                    .is_some_and(|data| data.contains(needle))
            {
                return frame;
            }
        }
    }
}

/// Given a session with a terminal, when the client attaches and writes a
/// command, then the command's output streams back as `terminal_output`
/// notifications (snake_case payload, the v1 multiplexed frame shape).
#[tokio::test]
async fn should_stream_terminal_output_when_a_terminal_is_attached_and_written() {
    let server = TestServer::start().await;
    let session_id = create_session(&server, "Terminal echo").await;
    let terminal = create_terminal(&server, &session_id).await;
    let terminal_id = terminal["id"].as_str().expect("terminal id").to_string();

    let mut socket = connected_socket(&server).await;
    let ack = terminal_call(
        &mut socket,
        10,
        "terminal_attach",
        json!({ "session_id": session_id, "terminal_id": terminal_id }),
    )
    .await;
    assert_eq!(ack["result"]["ok"], true, "attach acknowledges: {ack}");

    // When a command is written to the terminal's stdin
    terminal_call(
        &mut socket,
        11,
        "terminal_input",
        json!({
            "session_id": session_id,
            "terminal_id": terminal_id,
            "data": "echo hello-terminal\r"
        }),
    )
    .await;

    // Then the command's output streams back over the connection
    let frame = await_terminal_output(&mut socket, "hello-terminal").await;
    assert_eq!(frame["params"]["session_id"], session_id);
    assert_eq!(frame["params"]["terminal_id"], terminal_id);
    assert!(
        frame["params"]["seq"].as_u64().is_some(),
        "output frames carry a seq: {frame}"
    );
}

/// Given a terminal with prior output, when the client re-attaches with the
/// last seen seq, then no frame at or before that seq is replayed — only
/// output produced after the cursor arrives.
#[tokio::test]
async fn should_replay_only_frames_after_the_since_seq_when_reattaching() {
    let server = TestServer::start().await;
    let session_id = create_session(&server, "Terminal resume").await;
    let terminal = create_terminal(&server, &session_id).await;
    let terminal_id = terminal["id"].as_str().expect("terminal id").to_string();

    let mut socket = connected_socket(&server).await;
    terminal_call(
        &mut socket,
        10,
        "terminal_attach",
        json!({ "session_id": session_id, "terminal_id": terminal_id }),
    )
    .await;
    // Produce output and drain it, tracking the highest seq seen.
    terminal_call(
        &mut socket,
        11,
        "terminal_input",
        json!({
            "session_id": session_id,
            "terminal_id": terminal_id,
            "data": "echo one\r"
        }),
    )
    .await;
    let _ = await_terminal_output(&mut socket, "one").await;
    let mut last_seq = 0;
    let drain_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < drain_deadline {
        match tokio::time::timeout(Duration::from_millis(300), socket.next()).await {
            Ok(Some(Ok(message))) => {
                let text = message.to_text().expect("text message");
                for frame in text.split('\n').filter(|line| !line.is_empty()) {
                    let Ok(value) = serde_json::from_str::<Value>(frame) else {
                        continue;
                    };
                    for member in match &value {
                        Value::Array(members) => members.clone(),
                        other => vec![other.clone()],
                    } {
                        if member["method"] == "terminal_output" {
                            last_seq = last_seq.max(member["params"]["seq"].as_u64().unwrap_or(0));
                        }
                    }
                }
            }
            _ => break,
        }
    }

    // When the client re-attaches at the cursor (after detaching)
    terminal_call(
        &mut socket,
        12,
        "terminal_detach",
        json!({ "session_id": session_id, "terminal_id": terminal_id }),
    )
    .await;
    terminal_call(
        &mut socket,
        13,
        "terminal_attach",
        json!({
            "session_id": session_id,
            "terminal_id": terminal_id,
            "since_seq": last_seq
        }),
    )
    .await;

    // Then an idle shell streams nothing immediately: every frame that
    // arrives within the quiet window must be newer than the cursor (a
    // from-zero replay would deliver the pre-cursor history back).
    let quiet_deadline = tokio::time::Instant::now() + Duration::from_millis(400);
    while tokio::time::Instant::now() < quiet_deadline {
        match tokio::time::timeout(Duration::from_millis(100), socket.next()).await {
            Ok(Some(Ok(message))) => {
                let text = message.to_text().expect("text message");
                for frame in text.split('\n').filter(|line| !line.is_empty()) {
                    let Ok(value) = serde_json::from_str::<Value>(frame) else {
                        continue;
                    };
                    for member in match &value {
                        Value::Array(members) => members.clone(),
                        other => vec![other.clone()],
                    } {
                        if member["method"] == "terminal_output" {
                            let seq = member["params"]["seq"].as_u64().unwrap_or(0);
                            assert!(
                                seq > last_seq,
                                "replayed frame at seq {seq} is not after the cursor {last_seq}"
                            );
                        }
                    }
                }
            }
            _ => continue,
        }
    }

    // And output written after the re-attach still streams live.
    terminal_call(
        &mut socket,
        14,
        "terminal_input",
        json!({
            "session_id": session_id,
            "terminal_id": terminal_id,
            "data": "echo two\r"
        }),
    )
    .await;
    let _ = await_terminal_output(&mut socket, "two").await;
}

/// Given an attached terminal, when the client closes it over the stream,
/// then a `terminal_exit` notification fires and later input is refused.
#[tokio::test]
async fn should_emit_terminal_exit_when_the_terminal_is_closed_over_the_stream() {
    let server = TestServer::start().await;
    let session_id = create_session(&server, "Terminal close").await;
    let terminal = create_terminal(&server, &session_id).await;
    let terminal_id = terminal["id"].as_str().expect("terminal id").to_string();

    let mut socket = connected_socket(&server).await;
    terminal_call(
        &mut socket,
        10,
        "terminal_attach",
        json!({ "session_id": session_id, "terminal_id": terminal_id }),
    )
    .await;
    let close = terminal_call(
        &mut socket,
        11,
        "terminal_close",
        json!({ "session_id": session_id, "terminal_id": terminal_id }),
    )
    .await;
    assert_eq!(close["result"]["ok"], true, "close acknowledges: {close}");

    // The exit notification carries the terminal's identity and an exit code
    // (null when the shell was killed by a signal rather than exiting).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "terminal_exit never arrived"
        );
        let mut found = None;
        for frame in next_frames(&mut socket).await {
            if frame["method"] == "terminal_exit" {
                found = Some(frame);
            }
        }
        if let Some(frame) = found {
            assert_eq!(frame["params"]["session_id"], session_id);
            assert_eq!(frame["params"]["terminal_id"], terminal_id);
            assert!(frame["params"].get("exit_code").is_some());
            return;
        }
    }
}

/// Given a terminal producing output while the client attaches, when the
/// attach lands mid-stream, then every `terminal_output` the client
/// receives carries a strictly increasing seq and every written marker
/// arrives — the replay/live splice is gap-free and duplicate-free at the
/// wire level (F-P3-1, gate + atomic attach).
#[tokio::test]
async fn should_stream_strictly_increasing_seqs_when_attaching_mid_output() {
    let server = TestServer::start().await;
    let session_id = create_session(&server, "Terminal mid-stream attach").await;
    let terminal = create_terminal(&server, &session_id).await;
    let terminal_id = terminal["id"].as_str().expect("terminal id").to_string();

    let mut socket = connected_socket(&server).await;

    // Start a continuous output stream: a shell loop echoing 60 markers
    // keeps frames flowing across the attach below (no settling wait — the
    // replay/live splice has to happen mid-stream for the gate to matter).
    terminal_call(
        &mut socket,
        100,
        "terminal_input",
        json!({
            "session_id": session_id,
            "terminal_id": terminal_id,
            "data": "for i in $(seq 1 60); do echo pre-$i; done\r".to_string()
        }),
    )
    .await;

    // Attach immediately with a from-zero cursor: the snapshot lands
    // mid-stream, the replay enqueues while the event loop keeps emitting —
    // the gate must keep live frames behind the replay in the outbox.
    terminal_call(
        &mut socket,
        200,
        "terminal_attach",
        json!({ "session_id": session_id, "terminal_id": terminal_id }),
    )
    .await;

    // Then collect terminal_output notifications until every marker has
    // arrived, asserting seq monotonicity along the way (the replayed
    // prefix streams first, then the live suffix — one strictly increasing
    // sequence). A read that times out is tolerated: quiet periods are
    // normal while the shell works through the loop.
    let mut last_seq: Option<u64> = None;
    let mut combined = String::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let all_markers = |combined: &str| (1..=60).all(|i| combined.contains(&format!("pre-{i}")));
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "never saw all pre markers, combined so far: {combined}"
        );
        let maybe = tokio::time::timeout(Duration::from_millis(500), socket.next()).await;
        let message = match maybe {
            Ok(message) => message.expect("stream stays open").expect("text message"),
            Err(_) => {
                if all_markers(&combined) {
                    break;
                }
                continue;
            }
        };
        let text = message.to_text().expect("UTF-8 text message");
        let value: Value = serde_json::from_str(text).expect("message is valid JSON");
        let frames = match value {
            Value::Array(members) => members,
            other => vec![other],
        };
        for frame in frames {
            if frame["method"] != "terminal_output" {
                continue;
            }
            let seq = frame["params"]["seq"].as_u64().expect("seq on output");
            if let Some(previous) = last_seq {
                assert!(
                    seq > previous,
                    "terminal_output seq must strictly increase at the client, \
                     got {seq} after {previous}"
                );
            }
            last_seq = Some(seq);
            combined.push_str(frame["params"]["data"].as_str().unwrap_or(""));
        }
        if all_markers(&combined) {
            break;
        }
    }

    // And the stream stays live after the splice: one more command's
    // output arrives with a seq beyond everything seen so far.
    terminal_call(
        &mut socket,
        300,
        "terminal_input",
        json!({
            "session_id": session_id,
            "terminal_id": terminal_id,
            "data": "echo post-marker\r"
        }),
    )
    .await;
    let _ = await_terminal_output(&mut socket, "post-marker").await;
}

// ===========================================================================
// Outbox-bounded replay (F-T9-1): oversized replays must not deadlock
// ===========================================================================

/// The connection's outbound channel capacity (`OUTBOX_CAPACITY` in
/// `ws_rpc.rs`). Both tests below deliberately replay strictly more frames
/// than this through a single handler, the exact shape that deadlocked the
/// connection pre-fix (the handler awaited a full outbox whose only consumer
/// was the very select! loop stuck awaiting the handler).
const OUTBOX_CAPACITY_FOR_TEST: usize = 256;

/// Assert the connection is still alive and serving requests by issuing a
/// forward RPC and awaiting its response.
async fn assert_connection_still_alive(socket: &mut TestSocket, id: i64) {
    socket
        .send(Message::Text(
            json!({ "id": id, "method": "session.list", "params": {} })
                .to_string()
                .into(),
        ))
        .await
        .expect("send liveness probe");
    let response = next_response(socket, id).await;
    assert!(
        response["result"]["items"].is_array(),
        "connection must stay alive after the oversized replay: {response}"
    );
}

/// Given a terminal whose buffered output exceeds the connection's outbox
/// capacity, when a fresh client attaches with no `since_seq` (the web
/// panel's full-scrollback attach), then the connection survives: the ack
/// and every buffered frame arrive, the seqs are gap-free and
/// duplicate-free, and the connection still serves requests (F-T9-1).
#[tokio::test]
async fn should_survive_attach_replay_larger_than_the_outbox_capacity() {
    let server = TestServer::start().await;
    let session_id = create_session(&server, "Replay overflow").await;
    let terminal = create_terminal(&server, &session_id).await;
    let terminal_id = terminal["id"].as_str().expect("terminal id").to_string();

    // An observer socket attaches and drives a large burst of output: the
    // PTY reader emits one frame per <=4 KiB read, so 2 MiB of output is
    // guaranteed to produce >2x OUTBOX_CAPACITY frames (>=489).
    let mut observer = connected_socket(&server).await;
    let attach = terminal_call(
        &mut observer,
        10,
        "terminal_attach",
        json!({ "session_id": session_id, "terminal_id": terminal_id }),
    )
    .await;
    assert_eq!(attach["result"]["ok"], true, "observer attach: {attach}");
    let input = terminal_call(
        &mut observer,
        11,
        "terminal_input",
        json!({
            "session_id": session_id,
            "terminal_id": terminal_id,
            "data": "head -c 2000000 /dev/zero | tr '\\0' x; m=REPLAY; printf \"\\n$m-END\\n\"\r"
        }),
    )
    .await;
    assert_eq!(input["result"]["ok"], true, "burst input: {input}");

    // Read live frames until the trailing marker: when it arrives, every
    // earlier byte has already been read by the PTY reader and is therefore
    // in the terminal's bounded buffer. The marker frame's seq is the exact
    // number of buffered output frames.
    let end_seq = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            for frame in next_frames(&mut observer).await {
                if frame["method"] == "terminal_output"
                    && frame["params"]["data"]
                        .as_str()
                        .is_some_and(|data| data.contains("REPLAY-END"))
                {
                    return frame["params"]["seq"].as_u64().expect("marker seq");
                }
            }
        }
    })
    .await
    .expect("burst output completes (observer reads it live)");
    assert!(
        end_seq as usize > 2 * OUTBOX_CAPACITY_FOR_TEST,
        "precondition: the buffered replay must exceed the outbox capacity \
         twice over for a deterministic overflow (got {end_seq} frames)"
    );

    // When a fresh socket attaches with no since_seq: the full scrollback
    // replays through a 256-frame outbox. Pre-fix this deadlocked the
    // connection (no ack, no frames, no teardown); the timeout below turns
    // the hang into a red assertion instead of a stuck test.
    let mut socket = connected_socket(&server).await;
    let replayed = tokio::time::timeout(Duration::from_secs(30), async {
        let ack = terminal_call(
            &mut socket,
            20,
            "terminal_attach",
            json!({ "session_id": session_id, "terminal_id": terminal_id }),
        )
        .await;
        assert_eq!(ack["result"]["ok"], true, "attach acknowledges: {ack}");

        // Then every buffered frame (seq 1..=end_seq) arrives exactly once,
        // in order — a full, gap-free scrollback replay.
        let mut seqs: Vec<u64> = Vec::new();
        loop {
            for frame in next_frames(&mut socket).await {
                if frame["method"] == "terminal_output" {
                    let seq = frame["params"]["seq"].as_u64().expect("seq");
                    assert!(
                        seq <= end_seq,
                        "replayed seq {seq} exceeds the buffered marker seq {end_seq}"
                    );
                    seqs.push(seq);
                    if seq == end_seq {
                        return seqs;
                    }
                }
            }
        }
    })
    .await
    .expect("attach replay larger than the outbox must complete, not deadlock");
    let replay_start = replayed.first().copied().unwrap_or(0);
    assert_eq!(
        replayed.last(),
        Some(&end_seq),
        "the replay reaches the buffered marker frame"
    );
    assert_eq!(
        replayed,
        (replay_start..=end_seq).collect::<Vec<u64>>(),
        "the replayed scrollback is contiguous (no gaps, no duplicates); \
         frames before the replay start were evicted by the bounded buffer"
    );
    assert!(
        replayed.len() > OUTBOX_CAPACITY_FOR_TEST,
        "the replay itself must exceed the outbox capacity for the overflow \
         to have been exercised (got {} frames)",
        replayed.len()
    );

    // And the connection survived the oversized replay.
    assert_connection_still_alive(&mut socket, 21).await;
}

/// Given a session whose durable event log is completely full (256 entries ==
/// the outbox capacity), when a fresh client subscribes with a zero cursor,
/// then the 256-frame replay plus its ack (257 > 256) completes instead of
/// deadlocking the connection — the session.subscribe twin of F-T9-1.
#[tokio::test]
async fn should_survive_cursor_replay_when_the_event_log_is_full() {
    let server = TestServer::start().await;
    let session_id = create_session(&server, "Full log replay").await;

    // A first subscriber creates the session's event log (its stamping task
    // only exists once a channel has been created) and captures the epoch.
    let mut setup = connected_socket(&server).await;
    let ack = subscribe(&mut setup, &session_id, 2).await;
    let epoch = ack["result"]["epoch"].as_str().expect("epoch").to_string();

    // 300 durable events — strictly more than both the event log capacity
    // (256) and the outbox capacity (256). Pacing keeps the log task and
    // the live forwarder from lagging the event bus.
    for i in 0..300 {
        publish_message(&server, &session_id, &format!("bulk event {i}"));
        if i % 16 == 15 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
    let live = collect_events(&mut setup, 300).await;
    let live_tail = live
        .last()
        .map(|event| event_position(event).0)
        .expect("live events observed");
    assert_eq!(live_tail, 300, "all 300 events were stamped into the log");

    // When a fresh socket subscribes with a zero cursor: the full ring
    // buffer (the last 256 entries, seqs 45..=300) replays through a
    // 256-frame outbox, plus the ack — one frame past the boundary that
    // deadlocked the connection pre-fix. The timeout turns the hang into a
    // red assertion instead of a stuck test.
    let mut socket = connected_socket(&server).await;
    socket
        .send(Message::Text(
            json!({
                "id": 5,
                "method": "session.subscribe",
                "params": {
                    "sessionId": session_id,
                    "cursor": { "seq": 0, "epoch": epoch },
                },
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("send subscribe with zero cursor");
    let replayed = tokio::time::timeout(Duration::from_secs(20), async {
        let ack = next_response(&mut socket, 5).await;
        assert_eq!(ack["result"]["seq"], 300, "ack reports the tail: {ack}");
        collect_events(&mut socket, 256).await
    })
    .await
    .expect("full-log cursor replay must complete, not deadlock");

    // Then the whole ring buffer replays contiguously: seqs 45..=300, each
    // exactly once (the first 44 events were evicted by the ring buffer).
    let seqs: Vec<u64> = replayed
        .iter()
        .map(|event| event_position(event).0)
        .collect();
    assert_eq!(
        seqs,
        (45..=300).collect::<Vec<u64>>(),
        "the full event log replays contiguously, exactly once"
    );

    // And the connection survived the oversized replay.
    assert_connection_still_alive(&mut socket, 6).await;
}

/// Given an unknown terminal id, when the client attaches, then the request
/// fails with an explicit application error (not a silent ack).
#[tokio::test]
async fn should_answer_an_application_error_when_attaching_an_unknown_terminal() {
    let server = TestServer::start().await;
    let session_id = create_session(&server, "Terminal unknown").await;
    let mut socket = connected_socket(&server).await;

    let response = terminal_call(
        &mut socket,
        10,
        "terminal_attach",
        json!({ "session_id": session_id, "terminal_id": "term-does-not-exist" }),
    )
    .await;
    assert!(
        response["error"].is_object(),
        "attaching an unknown terminal must fail: {response}"
    );
    assert_eq!(response["error"]["code"], -32000);
}

// ===========================================================================
// mcp.subscribe: the app-level MCP pool event surface (M7, F-9)
// ===========================================================================

/// A stub engine whose app-level MCP pool broadcast the test controls
/// directly: `subscribe_mcp_pool_events` hands out receivers on a channel
/// only the test publishes to, so the serve-surface `mcp.subscribe` wiring
/// (the state-level forwarder + the connection-local relay) is exercised in
/// isolation — no embedded pool has to produce the events.
struct PoolEventEngine {
    pool_tx: broadcast::Sender<Arc<Value>>,
}

impl PoolEventEngine {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            pool_tx: broadcast::channel(64).0,
        })
    }

    /// Publish one pool-status payload (`{"server": {...}}`).
    fn emit(&self, payload: Value) {
        let _ = self.pool_tx.send(Arc::new(payload));
    }

    /// How many receivers the engine's pool channel carries. The serve
    /// binary keeps exactly one — the single state-level forwarder
    /// ([`ServerState::mcp_event_rx`]'s `OnceLock` spawn) — no matter how
    /// many connections subscribe.
    fn engine_receivers(&self) -> usize {
        self.pool_tx.receiver_count()
    }
}

#[async_trait::async_trait]
impl EngineAdapter for PoolEventEngine {
    async fn call_method(&self, method: &str, _params: Value) -> Result<Value, EngineError> {
        Err(EngineError::MethodNotFound(method.to_string()))
    }

    async fn subscribe_events(&self) -> Result<mpsc::Receiver<JsonRpcNotification>, EngineError> {
        let (_tx, rx) = mpsc::channel(8);
        Ok(rx)
    }

    fn subscribe_mcp_pool_events(&self) -> Result<broadcast::Receiver<Arc<Value>>, EngineError> {
        Ok(self.pool_tx.subscribe())
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

/// One pool-status payload shaped exactly like the engine's app-level
/// broadcast carries (`publish_pool_status_event`'s wire shape).
fn pool_status_payload(name: &str, status: &str) -> Value {
    json!({
        "server": {
            "name": name,
            "transport": "http",
            "status": status,
            "toolCount": 3,
            "pool": "user",
        }
    })
}

/// Send an `mcp.subscribe` request and return the server's response.
async fn mcp_subscribe(socket: &mut TestSocket, id: i64) -> Value {
    socket
        .send(Message::Text(
            json!({ "id": id, "method": "mcp.subscribe", "params": {} })
                .to_string()
                .into(),
        ))
        .await
        .expect("send mcp.subscribe");
    next_response(socket, id).await
}

/// Assert no frame arrives within the quiet window (a duplicate forwarder
/// would deliver a second copy of the last event here).
async fn assert_socket_stays_quiet(socket: &mut TestSocket, window: Duration) {
    if let Ok(frame) = tokio::time::timeout(window, socket.next()).await {
        panic!("expected no frames, got: {frame:?}");
    }
}

/// Given a subscribed connection on the app-level MCP pool surface, when the
/// engine publishes a pool transition and the connection re-subscribes, then
/// every event is forwarded exactly once as an `event.mcp.server.status`
/// notification, the re-subscribe acknowledges again, and a single
/// engine-level receiver serves any number of subscriptions (the state-level
/// forwarder is spawned once).
#[tokio::test]
async fn should_ack_and_forward_pool_events_when_mcp_subscribe_is_called_and_repeated() {
    // Given a server whose engine's pool broadcast the test controls, and a
    // connected client
    let engine = PoolEventEngine::new();
    let server = TestServer::start_with_engine(Arc::clone(&engine) as Arc<dyn EngineAdapter>).await;
    let mut socket = connected_socket(&server).await;

    // When the client subscribes to the app-level pool surface
    let ack = mcp_subscribe(&mut socket, 30).await;

    // Then the acknowledgment is the honest `{ok}` result under the echoed id
    assert_eq!(ack["id"], 30);
    assert_eq!(ack["result"], json!({ "ok": true }), "ack: {ack}");

    // When the engine publishes a pool transition
    let payload = pool_status_payload("mock", "connected");
    engine.emit(payload.clone());

    // Then it is forwarded to the subscribing connection as an
    // `event.mcp.server.status` notification carrying the payload as-is
    let events = collect_events(&mut socket, 1).await;
    let event = &events[0];
    assert_eq!(event["method"], "event.mcp.server.status");
    assert_eq!(event["jsonrpc"], "2.0");
    assert_eq!(
        event["params"], payload,
        "payload is forwarded unwrapped: {event}"
    );

    // When the client re-subscribes (idempotent) and the engine publishes
    // one more transition
    let re_ack = mcp_subscribe(&mut socket, 31).await;
    assert_eq!(re_ack["id"], 31, "re-subscribe acknowledges: {re_ack}");
    assert_eq!(re_ack["result"], json!({ "ok": true }));
    let mut second = pool_status_payload("mock", "failed");
    second["server"]["error"] = json!("connect refused");
    engine.emit(second.clone());

    // Then exactly one notification arrives — the re-subscribe spawned no
    // second forwarder (a duplicate delivery would surface inside the quiet
    // window below)
    let events = collect_events(&mut socket, 1).await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["params"], second, "second event: {:?}", events[0]);
    assert_socket_stays_quiet(&mut socket, Duration::from_millis(300)).await;

    // And the engine's pool channel carries exactly one receiver — the
    // single state-level forwarder both subscriptions share (no
    // per-connection engine receiver is ever spawned)
    assert_eq!(
        engine.engine_receivers(),
        1,
        "the state-level forwarder is the only engine-channel receiver"
    );
}

/// Given a subscribed connection, when it disconnects and a new connection
/// subscribes, then the new connection receives each subsequently published
/// event exactly once — the dead connection's forwarder is aborted at
/// teardown, so events published after the disconnect reach the new
/// subscriber unduplicated and nothing leaks across connections.
#[tokio::test]
async fn should_deliver_pool_events_to_the_next_connection_when_a_subscribed_socket_disconnects() {
    // Given a subscribed connection that already received one event
    let engine = PoolEventEngine::new();
    let server = TestServer::start_with_engine(Arc::clone(&engine) as Arc<dyn EngineAdapter>).await;
    let mut first = connected_socket(&server).await;
    assert_eq!(mcp_subscribe(&mut first, 30).await["result"]["ok"], true);
    engine.emit(pool_status_payload("mock", "connected"));
    let _ = collect_events(&mut first, 1).await;

    // When that connection goes away and events are published in the gap
    drop(first);
    tokio::time::sleep(Duration::from_millis(200)).await;
    engine.emit(pool_status_payload("mock", "pending"));

    // Then a fresh connection subscribing receives the events published
    // after its own subscribe, exactly once — the disconnected connection's
    // forwarder was aborted at its teardown, not kept delivering or
    // duplicated onto the new connection
    let mut second = connected_socket(&server).await;
    assert_eq!(mcp_subscribe(&mut second, 31).await["result"]["ok"], true);
    let after = pool_status_payload("mock", "connected");
    engine.emit(after.clone());
    let events = collect_events(&mut second, 1).await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["params"], after, "event: {:?}", events[0]);
    assert_socket_stays_quiet(&mut second, Duration::from_millis(300)).await;
}

// ===========================================================================
// resync_required: the subscribe-path resync signal (protocol §9.3)
// ===========================================================================

/// Given a client whose cursor was evicted from the ring buffer, when it
/// resubscribes with that stale cursor, then the server sends a
/// `resync_required` notification after the ack so the client rebuilds from
/// a snapshot instead of silently skipping the missed events.
#[tokio::test]
async fn should_send_resync_required_when_the_cursor_was_evicted() {
    let server = TestServer::start().await;
    let session_id = create_session(&server, "Evicted cursor").await;

    // Create the channel and capture the epoch.
    let mut setup = connected_socket(&server).await;
    let ack = subscribe(&mut setup, &session_id, 2).await;
    let epoch = ack["result"]["epoch"].as_str().expect("epoch").to_string();

    // 300 durable events — the first 44 get evicted from the 256-entry ring.
    for i in 0..300 {
        publish_message(&server, &session_id, &format!("bulk {i}"));
        if i % 16 == 15 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }
    let _ = collect_events(&mut setup, 300).await;
    drop(setup);

    // When a fresh socket subscribes with an evicted cursor (seq 1)
    let mut socket = connected_socket(&server).await;
    socket
        .send(Message::Text(
            json!({
                "id": 5,
                "method": "session.subscribe",
                "params": {
                    "sessionId": session_id,
                    "cursor": { "seq": 1, "epoch": epoch },
                },
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("send subscribe with evicted cursor");
    let ack = next_response(&mut socket, 5).await;
    assert_eq!(ack["result"]["seq"], 300, "ack reports the tail");

    // Then a resync_required notification arrives after the ack, carrying
    // the protocol §9.3 payload.
    let resync = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            for frame in next_frames(&mut socket).await {
                if frame.get("method").and_then(Value::as_str) == Some("resync_required") {
                    return frame;
                }
            }
        }
    })
    .await
    .expect("resync_required must be sent when the cursor was evicted");
    assert_eq!(resync["params"]["sessionId"], session_id);
    assert_eq!(resync["params"]["reason"], "buffer_overflow");
    assert_eq!(resync["params"]["currentSeq"], 300);
    assert_eq!(resync["params"]["seq"], 300);
    assert_eq!(resync["params"]["epoch"], epoch);
}

/// Given a client whose cursor carries a foreign epoch, when it
/// subscribes, then the server sends `resync_required` with reason
/// `epoch_changed` instead of replaying nothing silently.
#[tokio::test]
async fn should_send_resync_required_when_the_cursor_epoch_mismatches() {
    let server = TestServer::start().await;
    let session_id = create_session(&server, "Epoch mismatch").await;

    let mut socket = connected_socket(&server).await;
    let ack = subscribe(&mut socket, &session_id, 2).await;
    let _ = ack;
    publish_message(&server, &session_id, "one event");
    let _ = collect_events(&mut socket, 1).await;
    let tail = {
        let events = collect_events(&mut socket, 0).await;
        let _ = events;
        1u64
    };

    // When resubscribing with a mismatched epoch and an old seq
    socket
        .send(Message::Text(
            json!({
                "id": 5,
                "method": "session.subscribe",
                "params": {
                    "sessionId": session_id,
                    "cursor": { "seq": 0, "epoch": "foreign-epoch" },
                },
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("send subscribe with foreign epoch");
    let ack = next_response(&mut socket, 5).await;
    assert_eq!(ack["result"]["seq"], tail);

    // Then resync_required arrives with reason epoch_changed.
    let resync = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            for frame in next_frames(&mut socket).await {
                if frame.get("method").and_then(Value::as_str) == Some("resync_required") {
                    return frame;
                }
            }
        }
    })
    .await
    .expect("resync_required must be sent on epoch mismatch");
    assert_eq!(resync["params"]["reason"], "epoch_changed");
    assert_eq!(resync["params"]["sessionId"], session_id);
}

/// Given a healthy resubscribe (cursor at tail, nothing missed), when the
/// client subscribes, then NO resync_required is sent — the signal must stay
/// quiet on the happy path.
#[tokio::test]
async fn should_not_send_resync_required_when_the_cursor_replays_normally() {
    let server = TestServer::start().await;
    let session_id = create_session(&server, "Happy path").await;

    let mut socket = connected_socket(&server).await;
    let ack = subscribe(&mut socket, &session_id, 2).await;
    let epoch = ack["result"]["epoch"].as_str().expect("epoch").to_string();
    publish_message(&server, &session_id, "event 1");
    publish_message(&server, &session_id, "event 2");
    let _ = collect_events(&mut socket, 2).await;

    // When resubscribing with cursor seq 1 (event 2 still replayable)
    socket
        .send(Message::Text(
            json!({
                "id": 5,
                "method": "session.subscribe",
                "params": {
                    "sessionId": session_id,
                    "cursor": { "seq": 1, "epoch": epoch },
                },
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("send subscribe with replayable cursor");
    let _ack = next_response(&mut socket, 5).await;
    let replayed = collect_events(&mut socket, 1).await;
    assert_eq!(event_position(&replayed[0]).0, 2, "the missed event replays");

    // Then no resync_required arrives: assert the connection stays quiet
    // (only heartbeats, no resync frame) for a generous window.
    let saw_resync = tokio::time::timeout(Duration::from_millis(1500), async {
        loop {
            for frame in next_frames(&mut socket).await {
                if frame.get("method").and_then(Value::as_str) == Some("resync_required") {
                    return true;
                }
            }
        }
    })
    .await;
    assert!(saw_resync.is_err(), "no resync_required on the happy path");
}

/// Given a subscribed client whose forwarder cannot keep up, when the
/// live broadcast channel drops events (Lagged), then the server sends a
/// `resync_required` notification instead of silently skipping.
#[tokio::test]
async fn should_send_resync_required_when_the_live_stream_lags() {
    let server = TestServer::start().await;
    let session_id = create_session(&server, "Lagged forwarder").await;
    let mut socket = connected_socket(&server).await;
    let ack = subscribe(&mut socket, &session_id, 2).await;
    let epoch = ack["result"]["epoch"].as_str().expect("epoch").to_string();

    // Do NOT read from the socket: the forwarder's outbox backs up until
    // the live broadcast channel (capacity 256) overflows and the forwarder
    // observes RecvError::Lagged. The 8 KiB payloads saturate the loopback
    // socket buffering the writer drains into (kernel autotune is far
    // larger than 800 small frames), so the writer stalls and the forwarder
    // *must* observe Lagged; the ring buffer keeps the last 256 for replay.
    let big = "X".repeat(8 * 1024);
    for i in 0..800 {
        publish_message(&server, &session_id, &format!("{big} flood {i}"));
        if i % 16 == 15 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    }

    // Then resync_required arrives once the client drains its socket.
    let resync = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            for frame in next_frames(&mut socket).await {
                if frame.get("method").and_then(Value::as_str) == Some("resync_required") {
                    return frame;
                }
            }
        }
    })
    .await
    .expect("resync_required must be sent when the live stream lags");
    assert_eq!(resync["params"]["sessionId"], session_id);
    assert_eq!(resync["params"]["reason"], "buffer_overflow");
    assert_eq!(resync["params"]["epoch"], epoch);
    // currentSeq must be at least the number of published events (the log
    // stamped all 800) — the client adopts it as its new cursor.
    assert!(
        resync["params"]["currentSeq"].as_u64().expect("currentSeq") >= 800,
        "currentSeq reports the durable tail"
    );
}

/// Given the session's own event bus overflows the log task's receiver,
/// when events are dropped before being journaled, then the durable seq
/// sequence jumps by the dropped count (a detectable gap) instead of
/// continuing contiguously (an undetectable hole).
#[tokio::test]
async fn should_produce_a_seq_gap_when_the_event_bus_lags_the_log_task() {
    let server = TestServer::start().await;
    let session_id = create_session(&server, "Bus lag").await;

    let mut socket = connected_socket(&server).await;
    let ack = subscribe(&mut socket, &session_id, 2).await;
    let epoch = ack["result"]["epoch"].as_str().expect("epoch").to_string();

    // Baseline events first: the server (journal task included) runs on this
    // test's single-threaded runtime, so a synchronous flood below journals
    // *nothing* until this test yields again. A detectable gap therefore
    // needs a pre-lag seq — publish a small baseline and yield so the
    // journal task stamps and forwards it before the flood.
    for i in 0..8 {
        publish_message(&server, &session_id, &format!("baseline {i}"));
    }
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Flood the session's event bus faster than the log task can drain,
    // without pacing: the synchronous burst (2000 > bus capacity 256) forces
    // the log task's bus receiver to observe RecvError::Lagged the moment it
    // next runs, dropping every event except the newest 256. The observed
    // seqs must then jump from the baseline tail by the dropped count.
    for i in 0..2000 {
        publish_message(&server, &session_id, &format!("flood {i}"));
    }
    // One event after the flood: its seq must NOT be tail+1 if anything
    // was dropped.
    publish_message(&server, &session_id, "post-lag marker");
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Read everything that arrived and check for a seq jump > 1.
    let mut events = Vec::new();
    while let Ok(frame_set) =
        tokio::time::timeout(Duration::from_millis(300), next_frames(&mut socket)).await
    {
        for frame in frame_set {
            if frame.get("method").and_then(Value::as_str) == Some("event.message.received") {
                events.push(frame);
            }
        }
    }
    let seqs: Vec<u64> = events.iter().map(|e| event_position(e).0).collect();
    assert!(!seqs.is_empty(), "some events must have been journaled");
    // The gap assertion: there is at least one jump of more than 1 between
    // consecutive observed seqs (or the first observed seq is far beyond
    // what could have been delivered pre-lag), proving the skip was
    // recorded in the seq sequence.
    let has_gap = seqs.windows(2).any(|w| w[1] > w[0] + 1);
    assert!(
        has_gap,
        "a bus lag must leave a detectable seq gap, observed seqs: {seqs:?}"
    );
    // All observed seqs must still be within one epoch.
    assert!(events.iter().all(|e| event_position(e).1 == epoch));
}
