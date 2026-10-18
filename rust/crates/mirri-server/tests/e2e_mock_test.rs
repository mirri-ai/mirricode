//! E2E validation of the complete chat loop against a mock LLM: create
//! session → subscribe → send message → observe events → verify the turn
//! completed. This is the Phase-2 e2e suite (see the design docs
//! `.tmp/rust-core-design/01-protocol-unification.md`).
//!
//! The server runs in-process on an ephemeral port, wired to the shared
//! [`MockLlm`] from `mirri-test-harness` through the
//! [`MockLlmFactory`](mirri_test_harness::MockLlmFactory). The test and the
//! server hold the *same* `Arc<MockLlm>`, so the test can queue responses
//! before a `message.send` and afterwards assert how the engine consumed the
//! scripted event stream (`chat_call_count`, `delta_yield_count`,
//! `delta_spread`, `cancel_seen`).
//!
//! ## What the current server emits on the session event bus
//!
//! The server publishes `event.message.received` for every accepted message,
//! `event.assistant.delta` for streamed text chunks (relayed from the
//! engine's live-event callback), and `event.turn.ended` (reason
//! `"failed"`) when a fire-mode turn dies before completing (timeout,
//! panic, abort). Turn completion is verified through the agent's context
//! memory, which is exactly what the eventual `turn.ended` wire event will
//! summarize.
//!
//! One of the four tests is `#[ignore]`d because it targets server
//! behavior that is not practical to exercise yet:
//!
//! - `should_handle_turn_timeout_when_llm_never_responds` — the engine-turn
//!   timeout (`ENGINE_TURN_TIMEOUT` in `rpc.rs`) is hardcoded to 300 s, so
//!   the test would wait five minutes; enable it once the timeout is
//!   configurable.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::TestServer;
use futures::{SinkExt, StreamExt};
use mirri_core::{AgentId, Role, SessionId};
use mirri_test_harness::{MockLlm, MockLlmFactory, MockResponse};
use serde_json::{Value, json};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

type TestSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

// ===========================================================================
// Wire helpers (handshake, frames, RPC calls)
// ===========================================================================

/// Connect to the unified JSON-RPC WebSocket endpoint (`/api/v1/rpc`).
async fn connect_ws(server: &TestServer) -> TestSocket {
    connect_async(server.ws_rpc_url())
        .await
        .expect("websocket upgrade")
        .0
}

/// Read the next text frame, parsing it into one or more JSON values
/// (a batched array frame is flattened into its members). Waits up to
/// `frame_timeout` for the frame.
async fn next_frames_with_timeout(socket: &mut TestSocket, frame_timeout: Duration) -> Vec<Value> {
    let message = tokio::time::timeout(frame_timeout, socket.next())
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

/// Read the next frame(s) with the usual 5 s patience.
async fn next_frames(socket: &mut TestSocket) -> Vec<Value> {
    next_frames_with_timeout(socket, Duration::from_secs(5)).await
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

/// Collect `count` `event.*` notifications, in arrival order.
async fn collect_events(socket: &mut TestSocket, count: usize) -> Vec<Value> {
    collect_events_with_timeout(socket, count, Duration::from_secs(5)).await
}

/// Like [`collect_events`], but each frame may take up to `frame_timeout` to
/// arrive — needed by the (ignored) timeout test, where the interesting event
/// only arrives after the server's 300 s engine-turn deadline.
async fn collect_events_with_timeout(
    socket: &mut TestSocket,
    count: usize,
    frame_timeout: Duration,
) -> Vec<Value> {
    let mut events = Vec::new();
    while events.len() < count {
        for frame in next_frames_with_timeout(socket, frame_timeout).await {
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

/// Read frames until one `event.*` notification whose method starts with
/// `method_prefix` arrives, skipping unrelated events (e.g. streaming
/// `event.assistant.delta` frames while waiting for `event.turn.ended`).
async fn next_event_with_method(socket: &mut TestSocket, method_prefix: &str) -> Value {
    loop {
        for frame in next_frames(socket).await {
            if frame
                .get("method")
                .and_then(Value::as_str)
                .is_some_and(|method| method.starts_with(method_prefix))
            {
                return frame;
            }
        }
    }
}

/// Assert the `server_hello` greeting, answer with a `client_hello` request,
/// and assert the accepted response.
async fn complete_handshake(socket: &mut TestSocket) {
    let hello = next_frame(socket).await;
    assert_eq!(
        hello["method"], "server_hello",
        "first frame is the server greeting"
    );
    assert!(
        hello["params"]["capabilities"]["eventBatching"] == true,
        "server advertises event batching"
    );
    socket
        .send(Message::Text(
            r#"{"id":1,"method":"client_hello","params":{"clientId":"e2e-mock-client","capabilities":{"reverseRpc":["approval","question","fs","terminal"]}}}"#
                .into(),
        ))
        .await
        .expect("send client_hello");
    let ack = next_response(socket, 1).await;
    assert_eq!(ack["result"]["accepted"], true, "handshake accepted: {ack}");
}

/// Connect and complete the handshake.
async fn connected_socket(server: &TestServer) -> TestSocket {
    let mut socket = connect_ws(server).await;
    complete_handshake(&mut socket).await;
    socket
}

/// Send one JSON-RPC request and return the matching response.
async fn rpc_call(socket: &mut TestSocket, id: i64, method: &str, params: Value) -> Value {
    socket
        .send(Message::Text(
            json!({ "id": id, "method": method, "params": params })
                .to_string()
                .into(),
        ))
        .await
        .expect("send rpc request");
    next_response(socket, id).await
}

/// Send a `session.create` request and return the created session id.
async fn create_session_via_rpc(socket: &mut TestSocket, title: &str, id: i64) -> String {
    let response = rpc_call(socket, id, "session.create", json!({ "title": title })).await;
    response["result"]["id"]
        .as_str()
        .expect("session.create returns the session id")
        .to_string()
}

/// Send a `session.subscribe` request and return the server's response.
async fn subscribe(socket: &mut TestSocket, session_id: &str, id: i64) -> Value {
    rpc_call(
        socket,
        id,
        "session.subscribe",
        json!({ "sessionId": session_id }),
    )
    .await
}

/// Send a fire-mode `message.send` and return the response.
async fn send_fire(socket: &mut TestSocket, id: i64, session_id: &str, text: &str) -> Value {
    rpc_call(
        socket,
        id,
        "message.send",
        json!({ "sessionId": session_id, "text": text, "mode": "fire" }),
    )
    .await
}

// ===========================================================================
// Server-side helpers
// ===========================================================================

/// Poll the main agent's context memory until the completed turn recorded
/// the expected user message and canned assistant reply — the fire-mode
/// completion signal, since no `turn.ended` event is on the bus yet.
///
/// The step loop may inject user-role system reminders (e.g. the
/// `tools_added` announcement) between the prompt and the reply, so the
/// reply is asserted as the LAST message rather than `messages[1]`.
async fn wait_for_assistant_reply(
    server: &TestServer,
    session_id: &str,
    user_text: &str,
    assistant_text: &str,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let session = server
            .state
            .session_manager
            .get_session(&SessionId::new(session_id))
            .await
            .expect("session stays live");
        let agent = session
            .get_agent(&AgentId::new("main"))
            .await
            .expect("the main agent stays live");
        let messages = agent
            .context_memory()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .messages();
        let reply_recorded = messages.last().is_some_and(|last| {
            last.role == Role::Assistant && last.text_content() == assistant_text
        });
        if messages.len() >= 2
            && messages[0].role == Role::User
            && messages[0].text_content() == user_text
            && reply_recorded
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "fire-mode turn did not complete with the canned reply in context memory"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Poll a synchronous condition until it holds or the deadline passes.
async fn poll_until(deadline: tokio::time::Instant, what: &str, condition: impl Fn() -> bool) {
    loop {
        if condition() {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

// ===========================================================================
// The complete chat loop
// ===========================================================================

/// E2E: create session → subscribe → send message → receive events → verify
/// the turn completed with the mock's canned reply.
#[tokio::test]
async fn should_complete_full_chat_loop_when_using_mock_llm() {
    // Given a server whose agents are served by a shared mock LLM with a
    // canned response
    let factory = Arc::new(MockLlmFactory::with_text("Hello! I'm a mock LLM."));
    let mock = factory.shared();
    let server = TestServer::start_with_llm_factory(Some(factory)).await;
    let mut socket = connected_socket(&server).await;

    // When the client creates a session over the RPC socket (which eagerly
    // provisions the "main" agent)
    let session_id = create_session_via_rpc(&mut socket, "E2E Test", 2).await;
    assert!(!session_id.is_empty(), "session.create returns an id");

    // And subscribes to the session's event stream (empty log: seq 0)
    let ack = subscribe(&mut socket, &session_id, 3).await;
    assert_eq!(ack["result"]["seq"], 0, "empty log starts at seq 0");
    let epoch = ack["result"]["epoch"].as_str().expect("epoch").to_string();

    // And sends a message in fire mode. The engine publishes the triggering
    // `message.received` event on the session bus before the fire-mode call
    // returns, so the event milestone is read BEFORE the response (the wire
    // convention: the event-collector must not miss the underlying event
    // while waiting for the response frame).
    socket
        .send(Message::Text(
            json!({
                "id": 4,
                "method": "message.send",
                "params": { "sessionId": session_id, "text": "Hello", "mode": "fire" },
            })
            .to_string()
            .into(),
        ))
        .await
        .expect("send fire message");

    // Then the event.message.received notification streams to the subscriber
    // with the cursor envelope and the flattened camelCase payload
    let events = collect_events(&mut socket, 1).await;
    let event = &events[0];
    assert_eq!(event["method"], "event.message.received");
    assert_eq!(event["params"]["sessionId"], session_id);
    assert_eq!(event["params"]["volatile"], false);
    assert_eq!(event["params"]["seq"], 1, "first event on the log");
    assert_eq!(event["params"]["epoch"], epoch);
    assert_eq!(event["params"]["contentSummary"], "Hello");
    assert!(
        event["params"]["timestamp"]
            .as_str()
            .is_some_and(|timestamp| !timestamp.is_empty()),
        "durable events carry an RFC 3339 timestamp"
    );

    // And the fire-mode response arrives with the launched turn id
    let response = next_response(&mut socket, 4).await;
    let result = &response["result"];
    assert_eq!(result["ok"], true);
    assert_eq!(result["session_id"], session_id);
    assert_eq!(result["agent_id"], "main");
    let turn_id = result["turn_id"].as_str().expect("turn id");
    assert!(
        turn_id.starts_with("turn-"),
        "fire mode returns a UUID-backed turn id: {turn_id}"
    );

    // And the turn completed in the background: the mock was consulted once
    // and its canned reply landed in context memory
    wait_for_assistant_reply(&server, &session_id, "Hello", "Hello! I'm a mock LLM.").await;
    assert_eq!(
        mock.chat_call_count(),
        1,
        "the mock was consulted exactly once"
    );
}

// ===========================================================================
// Token streaming
// ===========================================================================

/// E2E: a streaming mock response reaches the engine as one delta per
/// character, paced by the configured delay — verified end-to-end through a
/// real fire-mode turn.
#[tokio::test]
async fn should_receive_assistant_delta_events_when_llm_streams_tokens() {
    // Given a server whose mock streams its reply one character at a time,
    // 10 ms apart
    let text = "Streaming works!";
    let mock = Arc::new(MockLlm::new());
    mock.push_response(MockResponse::streaming(text, Duration::from_millis(10)));
    let factory = Arc::new(MockLlmFactory::new(Arc::clone(&mock)));
    let server = TestServer::start_with_llm_factory(Some(factory)).await;
    let mut socket = connected_socket(&server).await;
    let session_id = create_session_via_rpc(&mut socket, "Streaming", 2).await;
    subscribe(&mut socket, &session_id, 3).await;

    // When a message is sent in fire mode
    let response = send_fire(&mut socket, 4, &session_id, "Stream this").await;
    assert_eq!(response["result"]["ok"], true);

    // Then the turn completes with the full text in memory...
    wait_for_assistant_reply(&server, &session_id, "Stream this", text).await;
    assert_eq!(mock.chat_call_count(), 1);

    // ...and the engine consumed the response incrementally: one delta per
    // character, spaced by the configured delay (the block-level `TextPart`
    // — and therefore the memory record — only arrives after the last delta,
    // so the counters below are final).
    let char_count = text.chars().count() as u32;
    assert_eq!(
        mock.delta_yield_count(),
        char_count as u64,
        "every character streamed as its own delta"
    );
    let expected_min_spread = Duration::from_millis(10) * char_count.saturating_sub(1);
    let spread = mock
        .delta_spread()
        .expect("at least two deltas yield a measurable spread");
    assert!(
        spread >= expected_min_spread,
        "deltas arrived paced by the token delay: {spread:?} < {expected_min_spread:?}"
    );
}

// ===========================================================================
// Turn timeout (engine never hears back from the LLM)
// ===========================================================================

/// E2E: a mock that never responds drives the server's engine-turn timeout —
/// the fire-mode turn must end with a `turn_failed` event instead of hanging
/// the session forever.
///
/// Ignored: the server hardcodes `ENGINE_TURN_TIMEOUT = 300 s` in `rpc.rs`,
/// so enabling this test waits the full five minutes. Enable it once the
/// timeout is configurable (or lower); the flow itself is what the eventual
/// configurable-timeout CI run will exercise.
#[ignore = "rpc.rs hardcodes ENGINE_TURN_TIMEOUT = 300 s; the test would wait five minutes. Enable once the engine-turn timeout is configurable."]
#[tokio::test]
async fn should_handle_turn_timeout_when_llm_never_responds() {
    // Given a server whose mock never responds, and a subscribed client
    let mock = Arc::new(MockLlm::new());
    mock.hang_forever();
    let factory = Arc::new(MockLlmFactory::new(Arc::clone(&mock)));
    let server = TestServer::start_with_llm_factory(Some(factory)).await;
    let mut socket = connected_socket(&server).await;
    let session_id = create_session_via_rpc(&mut socket, "Timeout", 2).await;
    subscribe(&mut socket, &session_id, 3).await;

    // When a message is sent in fire mode against the never-responding LLM
    let response = send_fire(&mut socket, 4, &session_id, "Are you there?").await;
    assert_eq!(response["result"]["ok"], true);
    assert_eq!(mock.chat_call_count(), 1, "the mock was consulted once");

    // Then the server's engine-turn deadline fires and an event.turn.ended
    // notification (reason "failed") tells subscribers the turn will never
    // complete — the client must not wait for a successful turn end forever.
    let events = collect_events_with_timeout(&mut socket, 1, Duration::from_secs(310)).await;
    let event = &events[0];
    assert_eq!(event["method"], "event.turn.ended");
    assert_eq!(event["params"]["sessionId"], session_id);
    assert_eq!(event["params"]["reason"], "failed");
    assert_eq!(event["params"]["error"]["code"], "internal");
    let error = event["params"]["error"]["message"]
        .as_str()
        .expect("event.turn.ended carries the failure description");
    assert!(
        error.contains("timed out"),
        "the failure names the timeout: {error}"
    );
}

// ===========================================================================
// Aborting an in-flight turn
// ===========================================================================

/// E2E: a fire-mode turn that is still streaming can be aborted. The server
/// acknowledges the abort, publishes a `turn_failed` event (an
/// `event.turn.ended` notification with reason "failed") so subscribers
/// stop waiting for the turn, and drops the in-flight registration.
///
/// The engine turn itself runs on the blocking pool and cannot be unwound
/// mid-LLM-call (documented limitation of `message.abort` in `rpc.rs`), so
/// the mock's stream keeps being consumed and the full reply still lands in
/// context memory — bounded by the streaming duration, not by the 300 s
/// `ENGINE_TURN_TIMEOUT`. If the engine is later taught to propagate the
/// abort into the LLM call, this test's final assertion flips (and the
/// mock's `cancel_seen` becomes the signal to assert).
#[tokio::test]
async fn should_allow_abort_when_turn_is_in_flight() {
    // Given a server whose mock streams a long reply (200 deltas × 25 ms ≈
    // 5 s), so the turn is still in flight long after message.send returns
    let reply = "x".repeat(200);
    let mock = Arc::new(MockLlm::new());
    mock.push_response(MockResponse::streaming(&reply, Duration::from_millis(25)));
    let factory = Arc::new(MockLlmFactory::new(Arc::clone(&mock)));
    let server = TestServer::start_with_llm_factory(Some(factory)).await;
    let mut socket = connected_socket(&server).await;
    let session_id = create_session_via_rpc(&mut socket, "Abort", 2).await;

    // When the client sends a message in fire mode
    let response = send_fire(&mut socket, 3, &session_id, "Long stream").await;
    let turn_id = response["result"]["turn_id"]
        .as_str()
        .expect("turn id")
        .to_string();

    // And waits until the streaming turn is demonstrably in flight (deltas
    // are flowing)
    poll_until(
        tokio::time::Instant::now() + Duration::from_secs(5),
        "the streaming turn to be in flight",
        || mock.delta_yield_count() >= 3,
    )
    .await;

    // And subscribes only now, so the abort fallout is the first event the
    // client observes (the earlier `message_received` is not replayed)
    subscribe(&mut socket, &session_id, 4).await;

    // Then aborting the turn by id is acknowledged
    let abort = rpc_call(
        &mut socket,
        5,
        "message.abort",
        json!({ "sessionId": session_id, "turnId": turn_id }),
    )
    .await;
    assert_eq!(abort["result"]["ok"], true, "abort acknowledged: {abort}");

    // And an event.turn.ended notification (reason "failed") tells
    // subscribers the client-visible turn is gone, so they can stop waiting
    // for it. The blocking turn keeps streaming deltas until it finishes,
    // so skip `event.assistant.delta` frames and wait for the ended
    // method.
    let event = next_event_with_method(&mut socket, "event.turn.ended").await;
    assert_eq!(event["method"], "event.turn.ended");
    assert_eq!(event["params"]["sessionId"], session_id);
    assert_eq!(event["params"]["reason"], "failed");
    // The payload's agentId must be the agent's wire identity — clients route
    // turn.ended frames by it, and an id other than "main" makes the web
    // projector treat the frame as a subagent's (leaving the main turn's
    // "thinking" UI stuck until a page refresh).
    assert_eq!(event["params"]["agentId"], "main");
    // The structured error object reports the real domain code of the
    // failure — a user cancellation, not a generic internal — with its
    // retryability verdict (aborts are terminal, never retried)
    assert_eq!(event["params"]["error"]["code"], "abort.user_cancelled");
    assert_eq!(event["params"]["error"]["retryable"], false);
    let error = event["params"]["error"]["message"]
        .as_str()
        .expect("event.turn.ended carries the failure description");
    assert!(
        error.contains("aborted"),
        "the failure names the abort: {error}"
    );

    // And the abort reached the engine's LLM stream: the mock observed the
    // cancel token between deltas (the embedded engine's cooperative
    // cancellation, unlike the pre-thinning server handler which could only
    // drop the task handle). The stream stops mid-way, so the full reply is
    // never recorded.
    poll_until(
        tokio::time::Instant::now() + Duration::from_secs(5),
        "the mock stream to observe the cancel token",
        || mock.cancel_seen(),
    )
    .await;
    let yielded = mock.delta_yield_count();
    assert!(
        yielded < reply.chars().count() as u64,
        "the cancelled stream stops before the full reply: yielded={yielded} of {}",
        reply.chars().count()
    );
}
