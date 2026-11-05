// layer: T2
// Domain group binary consolidated from per-file test binaries
// per 16-test-execution-strategy.md §4.4 (pure file moves).
mod common;

mod e2e_mock_test {
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
//! engine's live-event callback), and `event.turn.ended` (reason `"failed"`
//! for genuine failures, `"cancelled"` for aborts per ADR-007) when a
//! fire-mode turn dies before completing (timeout, panic, abort). Turn
//! completion is verified through the agent's context memory, which is
//! exactly what the eventual `turn.ended` wire event will summarize.
//!
//! One of the four tests is `#[ignore]`d because it targets server
//! behavior that is not practical to exercise yet:
//!
//! - `should_handle_turn_timeout_when_llm_never_responds` — the engine-turn
//!   timeout (`ENGINE_TURN_TIMEOUT` in `rpc.rs`) is hardcoded to 300 s, so
//!   the test would wait five minutes; enable it once the timeout is
//!   configurable.


use std::sync::Arc;
use std::time::Duration;

use crate::common::TestServer;
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
/// `event.turn.ended` notification with reason "cancelled" per ADR-007 —
/// the user-cancellation code projects into the cancelled vocabulary) so
/// subscribers stop waiting for the turn, and drops the in-flight
/// registration.
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

    // And an event.turn.ended notification (reason "cancelled" per
    // ADR-007 — the user's abort projects into the cancelled vocabulary)
    // tells subscribers the client-visible turn is gone, so they can stop
    // waiting for it. The blocking turn keeps streaming deltas until it
    // finishes, so skip `event.assistant.delta` frames and wait for the
    // ended method.
    let event = next_event_with_method(&mut socket, "event.turn.ended").await;
    assert_eq!(event["method"], "event.turn.ended");
    assert_eq!(event["params"]["sessionId"], session_id);
    assert_eq!(event["params"]["reason"], "cancelled");
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

}

mod files_tests {
//! Integration tests for the files REST surface (P4): multipart upload,
//! binary download, expiry metadata, and the error paths.


use std::time::Duration;

use crate::common::TestServer;
use serde_json::Value;

/// HTTP client with a generous timeout so slow CI machines do not flake.
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("build http client")
}

/// Build a `multipart/form-data` body by hand: one `file` part (with
/// filename and content type) plus the extra text fields, in order.
fn multipart_body(
    boundary: &str,
    filename: &str,
    mime_type: &str,
    data: &[u8],
    fields: &[(&str, &str)],
) -> Vec<u8> {
    let mut body = Vec::new();
    for (name, value) in fields {
        body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        body.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{name}\"\r\n\r\n").as_bytes(),
        );
        body.extend_from_slice(value.as_bytes());
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!("Content-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(format!("Content-Type: {mime_type}\r\n\r\n").as_bytes());
    body.extend_from_slice(data);
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    body
}

/// POST a multipart upload and return `(status, envelope json)`.
async fn upload(
    client: &reqwest::Client,
    server: &TestServer,
    boundary: &str,
    body: Vec<u8>,
) -> (reqwest::StatusCode, Value) {
    let response = client
        .post(format!("{}/api/v1/files", server.base_url()))
        .header(
            reqwest::header::CONTENT_TYPE,
            format!("multipart/form-data; boundary={boundary}"),
        )
        .body(body)
        .send()
        .await
        .expect("POST upload");
    let status = response.status();
    let envelope: Value = response.json().await.expect("JSON envelope");
    (status, envelope)
}

#[tokio::test]
async fn should_return_file_meta_when_uploading_multipart_file() {
    // Given an open server with a files dir
    let server = TestServer::start_with_files_dir().await;
    let client = client();

    // When a file is uploaded as multipart/form-data
    let body = multipart_body(
        "testboundary",
        "photo.png",
        "image/png",
        b"png-bytes-here",
        &[],
    );
    let (status, envelope) = upload(&client, &server, "testboundary", body).await;

    // Then the answer is the success envelope with the file metadata (the
    // TS `FileMeta` wire shape)
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(envelope["code"], 0, "envelope: {envelope}");
    let data = &envelope["data"];
    let id = data["id"].as_str().expect("file id");
    assert!(id.starts_with("f_"), "id must be f_-prefixed: {id}");
    assert_eq!(data["name"], "photo.png");
    assert_eq!(data["media_type"], "image/png");
    assert_eq!(data["size"], 14);
    assert!(
        data.get("expires_at").is_none() || data["expires_at"].is_null(),
        "no expiry without expires_in_sec"
    );
}

#[tokio::test]
async fn should_return_uploaded_bytes_when_getting_file_by_id() {
    // Given an uploaded file
    let server = TestServer::start_with_files_dir().await;
    let client = client();
    let payload = b"the quick brown fox jumps over the lazy dog";
    let body = multipart_body(
        "testboundary",
        "fox.txt",
        "text/plain",
        payload,
        &[("name", "renamed.txt")],
    );
    let (_, envelope) = upload(&client, &server, "testboundary", body).await;
    let id = envelope["data"]["id"]
        .as_str()
        .expect("file id")
        .to_string();

    // When the file is downloaded by id
    let response = client
        .get(format!("{}/api/v1/files/{id}", server.base_url()))
        .send()
        .await
        .expect("GET file");

    // Then the exact bytes come back with the metadata-driven headers
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let headers = response.headers().clone();
    let bytes = response.bytes().await.expect("file bytes");
    assert_eq!(&bytes[..], payload);
    assert_eq!(
        headers
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/plain")
    );
    let disposition = headers
        .get(reqwest::header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .expect("content-disposition header");
    assert!(
        disposition.starts_with("attachment; filename=\"renamed.txt\""),
        "disposition: {disposition}"
    );
    assert_eq!(
        headers
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok()),
        Some("43")
    );
}

#[tokio::test]
async fn should_serve_inline_disposition_when_getting_image_file() {
    // Given an uploaded image
    let server = TestServer::start_with_files_dir().await;
    let client = client();
    let body = multipart_body("b", "cat.png", "image/png", b"not-really-png", &[]);
    let (_, envelope) = upload(&client, &server, "b", body).await;
    let id = envelope["data"]["id"]
        .as_str()
        .expect("file id")
        .to_string();

    // When it is downloaded
    let response = client
        .get(format!("{}/api/v1/files/{id}", server.base_url()))
        .send()
        .await
        .expect("GET file");

    // Then the disposition is inline (image/* renders in the browser)
    let disposition = response
        .headers()
        .get(reqwest::header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .expect("content-disposition header");
    assert!(
        disposition.starts_with("inline"),
        "disposition: {disposition}"
    );
}

#[tokio::test]
async fn should_record_expires_at_when_uploading_with_expires_in_sec() {
    // Given an upload carrying the expiry field
    let server = TestServer::start_with_files_dir().await;
    let client = client();
    let body = multipart_body(
        "testboundary",
        "temp.bin",
        "application/octet-stream",
        b"xyz",
        &[("expires_in_sec", "3600")],
    );
    let (status, envelope) = upload(&client, &server, "testboundary", body).await;

    // Then the metadata records the future expiry timestamp
    assert_eq!(status, reqwest::StatusCode::OK);
    let expires_at = envelope["data"]["expires_at"].as_str().expect("expires_at");
    assert!(!expires_at.is_empty(), "expiry must be an ISO timestamp");
}

#[tokio::test]
async fn should_answer_404_when_getting_unknown_file() {
    // Given an empty files store
    let server = TestServer::start_with_files_dir().await;
    let client = client();

    // When an unknown file id is fetched
    let response = client
        .get(format!("{}/api/v1/files/f_nope", server.base_url()))
        .send()
        .await
        .expect("GET file");

    // Then the answer is the 404 error envelope (the binary route's error
    // shape — matching the TS server's FILE_NOT_FOUND mapping)
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
    let envelope: Value = response.json().await.expect("JSON envelope");
    assert_ne!(envelope["code"], 0, "envelope: {envelope}");
}

#[tokio::test]
async fn should_answer_204_and_remove_file_when_deleting_by_id() {
    // Given an uploaded file
    let server = TestServer::start_with_files_dir().await;
    let client = client();
    let body = multipart_body("b", "doomed.txt", "text/plain", b"delete me", &[]);
    let (_, envelope) = upload(&client, &server, "b", body).await;
    let id = envelope["data"]["id"]
        .as_str()
        .expect("file id")
        .to_string();

    // When the file is deleted by id
    let response = client
        .delete(format!("{}/api/v1/files/{id}", server.base_url()))
        .send()
        .await
        .expect("DELETE file");

    // Then the answer is 204 No Content with an empty body
    assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
    let body = response.bytes().await.expect("empty body");
    assert!(body.is_empty(), "204 must carry no body: {body:?}");

    // And the file is gone: a follow-up GET answers the 404 error envelope
    let response = client
        .get(format!("{}/api/v1/files/{id}", server.base_url()))
        .send()
        .await
        .expect("GET file");
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
    let envelope: Value = response.json().await.expect("JSON envelope");
    assert_ne!(envelope["code"], 0, "envelope: {envelope}");

    // And the blob itself was removed from the store root
    let files_dir = server.state.files_dir.clone().expect("files dir injected");
    assert!(
        !files_dir.join(&id).exists(),
        "the blob file must be removed from the files root"
    );
}

#[tokio::test]
async fn should_answer_404_when_deleting_unknown_file() {
    // Given an empty files store
    let server = TestServer::start_with_files_dir().await;
    let client = client();

    // When an unknown file id is deleted
    let response = client
        .delete(format!("{}/api/v1/files/f_nope", server.base_url()))
        .send()
        .await
        .expect("DELETE file");

    // Then the answer is the 404 error envelope
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
    let envelope: Value = response.json().await.expect("JSON envelope");
    assert_ne!(envelope["code"], 0, "envelope: {envelope}");
}

#[tokio::test]
async fn should_answer_401_when_deleting_file_without_token() {
    // Given a token-protected server
    let server = TestServer::start_with_auth("secret-token").await;
    let client = client();

    // When a file delete is requested without the credential
    let response = client
        .delete(format!("{}/api/v1/files/f_anything", server.base_url()))
        .send()
        .await
        .expect("DELETE file");

    // Then the request is rejected with the standard 401 error envelope
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    let envelope: Value = response.json().await.expect("JSON envelope");
    assert_ne!(envelope["code"], 0, "envelope: {envelope}");
}

#[tokio::test]
async fn should_answer_error_envelope_when_uploading_without_file_part() {
    // Given a multipart body that carries no file part at all — only text
    // fields (fastify's `req.file()` treats the first filename-carrying part
    // as the upload regardless of its field name, so a body without one has
    // nothing to store)
    let server = TestServer::start_with_files_dir().await;
    let client = client();
    let body = b"--testboundary\r\n\
Content-Disposition: form-data; name=\"name\"\r\n\
\r\n\
just-a-text-field\r\n\
--testboundary--\r\n"
        .to_vec();

    // When it is uploaded
    let (status, envelope) = upload(&client, &server, "testboundary", body).await;

    // Then the answer is the error envelope (validation failed)
    assert_eq!(status, reqwest::StatusCode::OK, "TS serves 200 + envelope");
    assert_ne!(envelope["code"], 0, "envelope: {envelope}");
}

#[tokio::test]
async fn should_serve_partial_content_when_range_requested() {
    // Given an uploaded file
    let server = TestServer::start_with_files_dir().await;
    let client = client();
    let payload = b"0123456789abcdef";
    let body = multipart_body("b", "hex.bin", "application/octet-stream", payload, &[]);
    let (_, envelope) = upload(&client, &server, "b", body).await;
    let id = envelope["data"]["id"]
        .as_str()
        .expect("file id")
        .to_string();

    // When a byte range is requested
    let response = client
        .get(format!("{}/api/v1/files/{id}", server.base_url()))
        .header(reqwest::header::RANGE, "bytes=2-5")
        .send()
        .await
        .expect("GET file range");

    // Then the slice comes back as 206 Partial Content
    assert_eq!(response.status(), reqwest::StatusCode::PARTIAL_CONTENT);
    let headers = response.headers().clone();
    let bytes = response.bytes().await.expect("file bytes");
    assert_eq!(&bytes[..], b"2345");
    assert_eq!(
        headers
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok()),
        Some("bytes 2-5/16")
    );
}

/// A deterministic 100 MiB byte pattern (a splitmix-style hash of the
/// offset): regenerable on both the upload and the download side without
/// ever buffering the whole payload.
#[cfg(test)]
mod peak {
    use super::*;

    const TOTAL: usize = 100 * 1024 * 1024;
    const CHUNK: usize = 64 * 1024;
    const BOUNDARY: &str = "biguploadboundary";

    fn pattern_at(offset: usize, buf: &mut [u8]) {
        for (i, byte) in buf.iter_mut().enumerate() {
            let position = (offset + i) as u64;
            *byte = (position.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 56) as u8;
        }
    }

    /// Stream the full multipart body (headers, then the 100 MiB pattern in
    /// 64 KiB chunks, then the closing delimiter) without ever materializing
    /// it client-side — the upload memory profile of the *server* under test
    /// is what the surrounding `/usr/bin/time -l` measurement observes.
    fn upload_body() -> impl futures::Stream<Item = Result<Vec<u8>, std::io::Error>> {
        use futures::StreamExt as _;
        let prefix = format!(
            "--{BOUNDARY}\r\n\
             Content-Disposition: form-data; name=\"file\"; filename=\"big.bin\"\r\n\
             Content-Type: application/octet-stream\r\n\
             \r\n"
        );
        let data = futures::stream::unfold(0usize, |offset| async move {
            if offset >= TOTAL {
                return None;
            }
            let len = CHUNK.min(TOTAL - offset);
            let mut chunk = vec![0u8; len];
            pattern_at(offset, &mut chunk);
            Some((Ok(chunk), offset + len))
        });
        let suffix = Ok(format!("\r\n--{BOUNDARY}--\r\n").into_bytes());
        futures::stream::iter(vec![Ok(prefix.into_bytes())])
            .chain(data)
            .chain(futures::stream::iter(vec![suffix]))
    }

    /// Manual peak-RSS measurement (plans/07 T10 F-P4-4): boot the test binary
    /// under `/usr/bin/time -l` and record the maximum resident set size —
    /// with a streaming upload the peak must stay far below the 100 MiB
    /// payload. Not part of the regular gate (it uploads 100 MiB through
    /// loopback on every run).
    #[tokio::test]
    #[ignore = "manual peak-RSS measurement: run under /usr/bin/time -l (plans/07 T10 F-P4-4)"]
    async fn should_stream_a_100mb_upload_with_bounded_memory() {
        // Given an open server with a files dir
        let server = TestServer::start_with_files_dir().await;
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .expect("build http client");

        // When the 100 MiB pattern is streamed up as one multipart upload
        let response = client
            .post(format!("{}/api/v1/files", server.base_url()))
            .header(
                reqwest::header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={BOUNDARY}"),
            )
            .body(reqwest::Body::wrap_stream(upload_body()))
            .send()
            .await
            .expect("POST upload");

        // Then the upload succeeds with the full size recorded
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let envelope: Value = response.json().await.expect("JSON envelope");
        assert_eq!(envelope["code"], 0, "envelope: {envelope}");
        assert_eq!(envelope["data"]["size"], TOTAL as u64);
        let id = envelope["data"]["id"]
            .as_str()
            .expect("file id")
            .to_string();

        // And the blob on disk is byte-exact, verified through a windowed
        // read (never a whole-file load — the GET route buffers its whole
        // body, which would pollute the memory-peak measurement under test)
        let files_dir = server.state.files_dir.clone().expect("files dir injected");
        let mut blob = std::fs::File::open(files_dir.join(&id)).expect("blob file exists");
        let mut offset = 0usize;
        let mut window = vec![0u8; CHUNK];
        loop {
            let read = std::io::Read::read(&mut blob, &mut window).expect("blob read");
            if read == 0 {
                break;
            }
            let mut expected = vec![0u8; read];
            pattern_at(offset, &mut expected);
            assert!(
                window[..read] == expected[..],
                "stored bytes diverge from the pattern at offset {offset}"
            );
            offset += read;
        }
        assert_eq!(offset, TOTAL, "the whole payload was stored");
    }
}

#[tokio::test]
async fn should_upload_file_larger_than_default_body_limit() {
    // Given a payload an order of magnitude above axum's default 2 MB body
    // limit (the TS server caps uploads at MAX_SAFE_INTEGER — effectively
    // uncapped — and the rust route must not truncate at 2 MB)
    let server = TestServer::start_with_files_dir().await;
    let client = client();
    let payload = vec![0x5Au8; 3 * 1024 * 1024];
    let body = multipart_body("b", "big.bin", "application/octet-stream", &payload, &[]);

    // When it is uploaded
    let (status, envelope) = upload(&client, &server, "b", body).await;

    // Then the upload succeeds with the full size recorded
    assert_eq!(status, reqwest::StatusCode::OK, "envelope: {envelope}");
    assert_eq!(envelope["data"]["size"], 3 * 1024 * 1024);
}

#[tokio::test]
async fn should_store_the_full_payload_when_the_close_delimiter_ends_at_eof() {
    // Given a hand-rolled multipart body that ends with the close delimiter
    // and no trailing CRLF (RFC 2046 legal; browsers/curl always append the
    // CRLF, a hand-written client need not) — the review's F-T10-1 repro
    // shape: the pending payload must be flushed, not silently dropped
    let server = TestServer::start_with_files_dir().await;
    let client = client();
    let body = b"--b\r\n\
Content-Disposition: form-data; name=\"file\"; filename=\"eof.bin\"\r\n\
Content-Type: application/octet-stream\r\n\
\r\n\
8-bytes!\r\n--b--"
        .to_vec();

    // When it is uploaded and downloaded back
    let (status, envelope) = upload(&client, &server, "b", body).await;
    assert_eq!(status, reqwest::StatusCode::OK, "envelope: {envelope}");
    assert_eq!(envelope["code"], 0, "envelope: {envelope}");
    assert_eq!(envelope["data"]["size"], 8);
    let id = envelope["data"]["id"]
        .as_str()
        .expect("file id")
        .to_string();
    let response = client
        .get(format!("{}/api/v1/files/{id}", server.base_url()))
        .send()
        .await
        .expect("GET file");

    // Then the exact payload survives — no silent truncation at EOF
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let bytes = response.bytes().await.expect("file bytes");
    assert_eq!(&bytes[..], b"8-bytes!", "bytes: {bytes:?}");
}

#[tokio::test]
async fn should_reject_upload_with_excessive_transport_padding() {
    // Given a multipart body padding its close delimiter with far more LWSP
    // than the cap (an unbounded padding run would pin the parser buffer —
    // the review's F-T10-2 DoS shape)
    let server = TestServer::start_with_files_dir().await;
    let client = client();
    let mut body = b"--b\r\n\
Content-Disposition: form-data; name=\"file\"; filename=\"pad.bin\"\r\n\
\r\n\
payload\r\n--b--"
        .to_vec();
    body.extend(std::iter::repeat_n(b' ', 8 * 1024));

    // When it is uploaded
    let (status, envelope) = upload(&client, &server, "b", body).await;

    // Then the request is rejected (the route's error convention: 200 + the
    // error envelope, as for any malformed multipart body)
    assert_eq!(status, reqwest::StatusCode::OK, "TS serves 200 + envelope");
    assert_ne!(envelope["code"], 0, "envelope: {envelope}");
    let message = envelope["msg"].as_str().unwrap_or_default();
    assert!(message.contains("padding"), "envelope: {envelope}");
}

/// A multipart body whose file payload deliberately embeds byte sequences
/// that collide with the boundary delimiters — the naive-scan parser
/// (F-P4-6) treated any `--boundary` byte run as a delimiter and truncated
/// the file there.
fn adversarial_body(boundary: &str) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(b"AA--bZZ"); // bare boundary inline
    payload.extend_from_slice(b"\r\n--bXYZ\r\n"); // CRLF boundary prefix + junk suffix
    payload.extend_from_slice(b"\r\n--b--junk"); // close-delimiter variant + junk
    payload.extend_from_slice(b"\r\n--b-- \ttail\r\n"); // close + LWSP + junk
    payload.extend_from_slice(b"\r\n--b-"); // boundary prefix fragment
    payload.push(b'\r');
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        b"Content-Disposition: form-data; name=\"file\"; filename=\"evil.bin\"\r\n\
          Content-Type: application/octet-stream\r\n\
          \r\n",
    );
    body.extend_from_slice(&payload);
    // A trailing text field after the file part proves the collision did not
    // desynchronize the parser either (no cross-part bleed).
    body.extend_from_slice(format!("\r\n--{boundary}\r\n").as_bytes());
    body.extend_from_slice(b"Content-Disposition: form-data; name=\"label\"\r\n\r\ncollides\r\n");
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

#[tokio::test]
async fn should_roundtrip_file_content_containing_boundary_collisions() {
    // Given an upload whose file content embeds boundary-shaped byte runs
    let server = TestServer::start_with_files_dir().await;
    let client = client();
    let body = adversarial_body("b");

    // When it is uploaded and downloaded back
    let (status, envelope) = upload(&client, &server, "b", body).await;
    assert_eq!(status, reqwest::StatusCode::OK, "envelope: {envelope}");
    assert_eq!(envelope["code"], 0, "envelope: {envelope}");
    let id = envelope["data"]["id"]
        .as_str()
        .expect("file id")
        .to_string();

    let response = client
        .get(format!("{}/api/v1/files/{id}", server.base_url()))
        .send()
        .await
        .expect("GET file");

    // Then the exact adversarial payload round-trips — no truncation at any
    // of the collision byte runs, and no bleed from the trailing field
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let bytes = response.bytes().await.expect("file bytes");
    let expected: &[u8] = b"AA--bZZ\r\n--bXYZ\r\n\r\n--b--junk\r\n--b-- \ttail\r\n\r\n--b-\r";
    assert_eq!(&bytes[..], expected, "bytes: {bytes:?}");
}

}

mod http_tests {
//! Integration tests for the HTTP transport layer (Phase 4).
//!
//! Tests boot a real server on an ephemeral port and exercise it with
//! `reqwest`, asserting the `{code, msg, data, request_id}` envelope
//! (`code: 0` = success). Naming follows the Given-When-Then convention.


use std::sync::Arc;
use std::time::Duration;

use crate::common::TestServer;
use mirri_core::{AgentId, AgentScopeConfig, PermissionMode, Role, SessionId};
use mirri_test_harness::{MockLlm, MockLlmFactory};
use serde_json::{Value, json};

/// HTTP client with a generous timeout so slow CI machines do not flake.
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("build http client")
}

async fn get(client: &reqwest::Client, url: &str) -> (reqwest::StatusCode, Value) {
    let response = client.get(url).send().await.expect("GET request");
    let status = response.status();
    let body: Value = response.json().await.expect("JSON body");
    (status, body)
}

async fn post(client: &reqwest::Client, url: &str, body: Value) -> (reqwest::StatusCode, Value) {
    let response = client
        .post(url)
        .json(&body)
        .send()
        .await
        .expect("POST request");
    let status = response.status();
    let body: Value = response.json().await.expect("JSON body");
    (status, body)
}

async fn delete(client: &reqwest::Client, url: &str) -> (reqwest::StatusCode, Value) {
    let response = client.delete(url).send().await.expect("DELETE request");
    let status = response.status();
    let body: Value = response.json().await.expect("JSON body");
    (status, body)
}

/// Create a session through the HTTP API and return its id.
async fn create_session(server: &TestServer, client: &reqwest::Client, title: &str) -> String {
    let (status, body) = post(
        client,
        &format!("{}/api/v1/sessions", server.base_url()),
        json!({ "title": title }),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "session create succeeds");
    body["data"]["id"].as_str().expect("session id").to_string()
}

/// Create a session through the HTTP API and add one fresh `agent-test`
/// agent directly through the session manager (there is no RPC endpoint for
/// agent creation yet). Returns the session id.
async fn create_session_with_agent(server: &TestServer, client: &reqwest::Client) -> String {
    create_session_with_agent_mode(server, client, PermissionMode::default()).await
}

/// Create a session through the HTTP API and add one fresh `agent-test`
/// agent with the given permission mode directly through the session manager.
async fn create_session_with_agent_mode(
    server: &TestServer,
    client: &reqwest::Client,
    permission_mode: PermissionMode,
) -> String {
    let session_id = create_session(server, client, "Engine wiring").await;
    let session = server
        .state
        .session_manager
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("created session is live");
    session
        .create_agent(
            AgentId::new("agent-test"),
            AgentScopeConfig {
                permission_mode,
                ..AgentScopeConfig::default()
            },
        )
        .await
        .expect("create agent");
    session_id
}

// ===========================================================================
// Infra routes
// ===========================================================================

#[tokio::test]
async fn should_return_ok_and_ready_when_getting_healthz() {
    // Given a running server
    let server = TestServer::start().await;
    let client = client();

    // When requesting the health endpoint
    let (status, body) = get(&client, &format!("{}/api/v1/healthz", server.base_url())).await;

    // Then it reports healthy and ready in the standard envelope
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);
    assert_eq!(body["msg"], "success");
    assert_eq!(body["data"]["ok"], true);
    assert_eq!(body["data"]["ready"], true);
}

#[tokio::test]
async fn should_return_version_and_started_at_when_getting_meta() {
    // Given a running server
    let server = TestServer::start().await;
    let client = client();

    // When requesting the metadata endpoint
    let (status, body) = get(&client, &format!("{}/api/v1/meta", server.base_url())).await;

    // Then it reports the server version, id, and start time in the v1 wire
    // contract's snake_case shape (the protocol schema and the live e2e
    // suites pin these spellings)
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["server_version"], "0.1.0-test");
    assert!(
        body["data"]["server_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "server_id is a non-empty string"
    );
    let started_at = body["data"]["started_at"]
        .as_str()
        .expect("started_at is a string");
    chrono::DateTime::parse_from_rfc3339(started_at).expect("started_at is RFC3339");
    assert!(
        body["data"]["open_in_apps"].as_array().is_some(),
        "open_in_apps is always present (may be empty)"
    );
    assert!(
        body["data"]["dangerous_bypass_auth"].as_bool().is_some(),
        "dangerous_bypass_auth is always present"
    );
}

#[tokio::test]
async fn should_echo_envelope_when_calling_api() {
    // Given a running server and a client that sends x-request-id
    let server = TestServer::start().await;
    let client = client();

    // When calling an endpoint with a request id header
    let response = client
        .get(format!("{}/api/v1/healthz", server.base_url()))
        .header("x-request-id", "req-123")
        .send()
        .await
        .expect("GET request");
    let body: Value = response.json().await.expect("JSON body");

    // Then the envelope echoes the request id back
    assert_eq!(body["code"], 0);
    assert_eq!(body["request_id"], "req-123");
}

// ===========================================================================
// Authentication
// ===========================================================================

/// Shared secret enforced by auth-enabled test servers.
const TEST_AUTH_TOKEN: &str = "test-secret-token";

#[tokio::test]
async fn should_return_401_when_requesting_without_token() {
    // Given a server that requires a Bearer token on protected routes
    let server = TestServer::start_with_auth(TEST_AUTH_TOKEN).await;
    let client = client();

    // When requesting a protected route without an Authorization header
    let (status, body) = get(&client, &format!("{}/api/v1/config", server.base_url())).await;

    // Then the request is rejected with the standard 401 error envelope
    assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], -32002);
    assert_eq!(body["msg"], "unauthorized");
    assert_eq!(body["data"], Value::Null);
    assert!(
        body["request_id"].as_str().is_some_and(|id| !id.is_empty()),
        "401 envelope carries a request id"
    );
}

#[tokio::test]
async fn should_return_401_when_requesting_with_wrong_token() {
    // Given a server that requires a Bearer token on protected routes
    let server = TestServer::start_with_auth(TEST_AUTH_TOKEN).await;
    let client = client();

    // When requesting a protected route with a token that does not match
    let response = client
        .get(format!("{}/api/v1/config", server.base_url()))
        .bearer_auth("wrong-token")
        .send()
        .await
        .expect("GET request");
    let status = response.status();
    let body: Value = response.json().await.expect("JSON body");

    // Then the request is rejected with the standard 401 error envelope
    assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], -32002);
    assert_eq!(body["msg"], "unauthorized");
}

#[tokio::test]
async fn should_return_200_when_requesting_with_valid_token() {
    // Given a server that requires a Bearer token on protected routes
    let server = TestServer::start_with_auth(TEST_AUTH_TOKEN).await;
    let client = client();

    // When requesting a protected route with the correct token
    let response = client
        .get(format!("{}/api/v1/config", server.base_url()))
        .bearer_auth(TEST_AUTH_TOKEN)
        .send()
        .await
        .expect("GET request");
    let status = response.status();
    let body: Value = response.json().await.expect("JSON body");

    // Then the request is served normally
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);
}

#[tokio::test]
async fn should_return_200_when_requesting_healthz_without_token() {
    // Given a server that requires a Bearer token on protected routes
    let server = TestServer::start_with_auth(TEST_AUTH_TOKEN).await;
    let client = client();

    // When requesting the public health endpoint without a token
    let (status, body) = get(&client, &format!("{}/api/v1/healthz", server.base_url())).await;

    // Then the probe stays reachable — it is exempt from authentication
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["ok"], true);
    assert_eq!(body["data"]["ready"], true);
}

#[tokio::test]
async fn should_return_401_when_requesting_auth_summary_without_token() {
    // Given a server that requires a Bearer token on protected routes
    let server = TestServer::start_with_auth(TEST_AUTH_TOKEN).await;
    let client = client();

    // When requesting the auth summary without a credential — the exact
    // request the web UI makes on first paint
    let (status, body) = get(&client, &format!("{}/api/v1/auth", server.base_url())).await;

    // Then it is rejected like every protected route: the 401 is the signal
    // the web client uses to show its server-token dialog
    assert_eq!(status, reqwest::StatusCode::UNAUTHORIZED);
    assert_eq!(body["code"], -32002);
    assert_eq!(body["msg"], "unauthorized");
}

#[tokio::test]
async fn should_report_ready_when_a_default_model_and_custom_provider_are_configured() {
    // Given an auth-enabled server with a custom provider and a default
    // model alias
    let server = TestServer::start_with_auth(TEST_AUTH_TOKEN).await;
    let config = mirri_core::ServerConfig {
        providers: Some(std::collections::HashMap::from([(
            "custom".to_string(),
            mirri_core::ProviderEntry {
                api_key: Some("test-key".to_string()),
                ..Default::default()
            },
        )])),
        default_model: Some("ultra".to_string()),
        ..Default::default()
    };
    *server
        .state
        .config
        .write()
        .unwrap_or_else(|e| e.into_inner()) = config;
    let client = client();

    // When requesting the auth summary with a valid credential
    let response = client
        .get(format!("{}/api/v1/auth", server.base_url()))
        .bearer_auth(TEST_AUTH_TOKEN)
        .send()
        .await
        .expect("GET request");
    let status = response.status();
    let body: Value = response.json().await.expect("JSON body");

    // Then the v1 AuthSummary shape reports a ready setup
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["ready"], true);
    // The configured custom provider.
    assert_eq!(body["data"]["providers_count"], 1);
    assert_eq!(body["data"]["default_model"], "ultra");
}

#[tokio::test]
async fn should_report_not_ready_when_no_default_model_is_configured() {
    // Given an auth-enabled server with no providers and no default model
    let server = TestServer::start_with_auth(TEST_AUTH_TOKEN).await;
    let client = client();

    // When requesting the auth summary with a valid credential
    let response = client
        .get(format!("{}/api/v1/auth", server.base_url()))
        .bearer_auth(TEST_AUTH_TOKEN)
        .send()
        .await
        .expect("GET request");
    let status = response.status();
    let body: Value = response.json().await.expect("JSON body");

    // Then the summary reports the not-ready setup
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["ready"], false);
    assert_eq!(body["data"]["providers_count"], 0);
    assert_eq!(body["data"]["default_model"], Value::Null);
}

#[tokio::test]
async fn should_return_200_when_requesting_protected_route_without_token_on_open_server() {
    // Given a server in open (no-token) mode
    let server = TestServer::start().await;
    let client = client();

    // When requesting a protected route without a token
    let (status, body) = get(&client, &format!("{}/api/v1/config", server.base_url())).await;

    // Then the request is served normally — open mode allows everyone
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);
}

#[tokio::test]
async fn should_return_200_when_shutting_down_without_token_on_open_server() {
    // Given a server in open (no-token) mode listening on loopback
    let server = TestServer::start().await;
    let client = client();

    // When requesting a graceful shutdown from the loopback peer
    let (status, body) = post(
        &client,
        &format!("{}/api/v1/shutdown", server.base_url()),
        json!({}),
    )
    .await;

    // Then the shutdown is acknowledged — loopback origins are trusted
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["ok"], true);
    assert!(
        *server.state.shutdown_tx.borrow(),
        "shutdown flag is raised on the server state"
    );
}

// ===========================================================================
// Session lifecycle routes
// ===========================================================================

#[tokio::test]
async fn should_return_session_id_when_creating_session() {
    // Given a running server
    let server = TestServer::start().await;
    let client = client();

    // When creating a session with a title and tags
    let (status, body) = post(
        &client,
        &format!("{}/api/v1/sessions", server.base_url()),
        json!({ "title": "E2E Session", "tags": ["transport"] }),
    )
    .await;

    // Then the envelope carries the new session's wire view (session.create
    // eagerly provisions the "main" agent, mirroring the embedded engine)
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["title"], "E2E Session");
    assert_eq!(body["data"]["agentCount"], 1);
    let session_id = body["data"]["id"].as_str().expect("session id");
    assert!(
        session_id.starts_with("session-"),
        "session id has the scope prefix: {session_id}"
    );
    assert!(
        body["data"]["tags"]
            .as_array()
            .is_some_and(|tags| tags.len() == 1)
    );
}

#[tokio::test]
async fn should_return_session_when_listing_sessions() {
    // Given a running server with one created session
    let server = TestServer::start().await;
    let client = client();
    let session_id = create_session(&server, &client, "List me").await;
    let base = server.base_url();

    // When listing sessions
    let (status, body) = get(&client, &format!("{base}/api/v1/sessions")).await;

    // Then the created session appears exactly once with its summary (the
    // v2 page envelope — `data: {items, has_more}` — matching the TS route)
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["has_more"], false);
    let sessions = body["data"]["items"].as_array().expect("session list");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["id"], session_id);
    assert_eq!(sessions[0]["title"], "List me");
}

#[tokio::test]
async fn should_return_details_when_getting_session() {
    // Given a running server with one created session
    let server = TestServer::start().await;
    let client = client();
    let session_id = create_session(&server, &client, "Details please").await;
    let base = server.base_url();

    // When fetching the session by id
    let (status, body) = get(&client, &format!("{base}/api/v1/sessions/{session_id}")).await;

    // Then the envelope carries the session details with the eagerly
    // provisioned main agent as its only agent
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["id"], session_id);
    assert_eq!(body["data"]["title"], "Details please");
    assert_eq!(body["data"]["agentCount"], 1);
    let agents = body["data"]["agents"].as_array().expect("agent list");
    assert_eq!(agents, &vec![json!({ "id": "main" })]);
}

#[tokio::test]
async fn should_remove_session_when_closing_session() {
    // Given a running server with one created session
    let server = TestServer::start().await;
    let client = client();
    let session_id = create_session(&server, &client, "Close me").await;
    let base = server.base_url();

    // When closing the session through the HTTP API
    let (status, body) = delete(&client, &format!("{base}/api/v1/sessions/{session_id}")).await;

    // Then the close reports success
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["ok"], true);

    // And the session is gone: fetching it returns 200 with a NotFound code
    // (TS server convention: HTTP status is always 200, error in envelope)
    let (status, body) = get(&client, &format!("{base}/api/v1/sessions/{session_id}")).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], -32001);
}

#[tokio::test]
async fn should_return_404_when_getting_missing_session() {
    // Given a running server without sessions
    let server = TestServer::start().await;
    let client = client();

    // When fetching a session id that was never created
    let (status, body) = get(
        &client,
        &format!(
            "{}/api/v1/sessions/session-00000000-0000-0000-0000-000000000000",
            server.base_url()
        ),
    )
    .await;

    // Then it returns 200 with a NotFound error code in the envelope
    // (TS server convention: HTTP status is always 200, error in envelope)
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], -32001);
}

// ===========================================================================
// Tool & route fallback
// ===========================================================================

#[tokio::test]
async fn should_return_the_main_agents_builtin_tools_when_listing_tools() {
    // Given a running server with one created session (session.create
    // eagerly provisions the "main" agent with its builtin tools enabled)
    let server = TestServer::start().await;
    let client = client();
    let session_id = create_session(&server, &client, "Toolbox").await;
    let base = server.base_url();

    // When listing the session's tools
    let (status, body) = get(
        &client,
        &format!("{base}/api/v1/sessions/{session_id}/tools"),
    )
    .await;

    // Then the envelope carries the main agent's builtin tool list (no user
    // or MCP tools are registered)
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);
    let tools = body["data"].as_array().expect("tool list");
    let names: Vec<&str> = tools
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name"))
        .collect();
    assert!(
        names.contains(&"Read") && names.contains(&"Bash") && names.contains(&"Grep"),
        "the main agent's builtin tools are listed: {names:?}"
    );
    assert!(
        !names.contains(&"Agent") && !names.contains(&"AgentSwarm"),
        "no subagent host is wired on the server side: {names:?}"
    );
}

#[tokio::test]
async fn should_return_404_when_requesting_unknown_route() {
    // Given a running server
    let server = TestServer::start().await;
    let client = client();

    // When requesting a route that is not registered
    let response = client
        .get(format!(
            "{}/api/v1/definitely/not/a/route",
            server.base_url()
        ))
        .send()
        .await
        .expect("GET request");

    // Then it returns 404
    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn should_respond_ok_when_posting_shutdown() {
    // Given a running server
    let server = TestServer::start().await;
    let client = client();

    // When requesting a graceful shutdown
    let (status, body) = post(
        &client,
        &format!("{}/api/v1/shutdown", server.base_url()),
        json!({}),
    )
    .await;

    // Then the request is acknowledged and the shutdown flag is raised
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["ok"], true);
    assert!(
        *server.state.shutdown_tx.borrow(),
        "shutdown flag is raised on the server state"
    );
}

// ===========================================================================
// Engine wiring: message.send
// ===========================================================================

#[tokio::test]
async fn should_return_internal_error_when_sending_message_without_llm_provider() {
    // Given a session with an agent on a server without an LLM provider
    let server = TestServer::start().await;
    let client = client();
    let session_id = create_session_with_agent(&server, &client).await;
    let base = server.base_url();

    // When sending a message (addressing agent-test explicitly — the session
    // now also holds the eagerly provisioned main agent, and the first-agent
    // fallback order is unspecified)
    let (status, body) = post(
        &client,
        &format!("{base}/api/v1/sessions/{session_id}/messages"),
        json!({ "text": "Hello without an LLM", "agentId": "agent-test" }),
    )
    .await;

    // Then the send fails with a clear internal error
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], -32603);
    assert!(
        body["msg"]
            .as_str()
            .is_some_and(|msg| { msg.contains("no LLM provider configured for this agent") }),
        "envelope explains the missing provider: {}",
        body["msg"]
    );

    // And — the engine-backed wire semantics (message.send is an engine
    // method now) — the failed send leaves the session state untouched: the
    // provider check runs before any memory mutation, so history stays
    // consistent with the (empty) wire log.
    let session = server
        .state
        .session_manager
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let agent = session
        .get_agent(&AgentId::new("agent-test"))
        .await
        .expect("agent-test exists");
    let messages = agent
        .context_memory()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .messages();
    assert!(
        messages.is_empty(),
        "a no-provider send records nothing: {}",
        messages.len()
    );
}

#[tokio::test]
async fn should_run_turn_and_return_turn_result_when_sending_message_with_scripted_llm() {
    // Given a session with an agent on a server whose engine runs a shared
    // mock LLM
    let mock = Arc::new(MockLlm::new());
    mock.push_text_response("Hello from the mock LLM");
    let server =
        TestServer::start_with_llm_factory(Some(Arc::new(MockLlmFactory::new(Arc::clone(&mock)))))
            .await;
    let client = client();
    let session_id = create_session_with_agent(&server, &client).await;
    let base = server.base_url();

    // When sending a message (addressing agent-test explicitly — the session
    // now also holds the eagerly provisioned main agent, and the first-agent
    // fallback order is unspecified)
    let (status, body) = post(
        &client,
        &format!("{base}/api/v1/sessions/{session_id}/messages"),
        json!({ "text": "Hello engine", "agentId": "agent-test" }),
    )
    .await;

    // Then the envelope carries the successful turn end result
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["agent_id"], "agent-test");
    assert_eq!(body["data"]["turn"]["turnId"], 1);
    assert_eq!(body["data"]["turn"]["stopReason"], "end_turn");
    assert_eq!(body["data"]["turn"]["steps"], 1);
    assert_eq!(body["data"]["turn"]["blockedByPromptHook"], false);

    // And memory holds the user message plus the assistant reply. The step
    // loop may inject user-role system reminders (e.g. the tools_added
    // announcement) between them, so the reply is asserted as the LAST
    // message rather than messages[1].
    let session = server
        .state
        .session_manager
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let agent = session
        .get_agent(&AgentId::new("agent-test"))
        .await
        .expect("agent-test exists");
    let messages = agent
        .context_memory()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .messages();
    assert!(
        messages.len() >= 2,
        "user prompt and assistant reply are both recorded"
    );
    assert_eq!(messages[0].role, Role::User);
    assert_eq!(messages[0].text_content(), "Hello engine");
    let reply = messages.last().expect("assistant reply recorded");
    assert_eq!(reply.role, Role::Assistant);
    assert_eq!(reply.text_content(), "Hello from the mock LLM");
}

// ===========================================================================
// Terminal routes (web terminal panel)
// ===========================================================================

/// Given a session, when a terminal is created through the REST API, then
/// the wire object follows the protocol's terminal schema and the terminal
/// is listed and fetchable.
#[tokio::test]
async fn should_create_list_and_get_a_terminal_when_the_rest_api_is_used() {
    let server = TestServer::start().await;
    let client = client();
    let session_id = create_session(&server, &client, "Terminals").await;

    // Create answers the protocol's `Terminal` wire shape (snake_case).
    let (status, body) = post(
        &client,
        &format!(
            "{}/api/v1/sessions/{session_id}/terminals",
            server.base_url()
        ),
        json!({ "cols": 120, "rows": 30 }),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "terminal create succeeds");
    assert_eq!(body["code"], 0, "terminal create envelope: {body}");
    let terminal = &body["data"];
    let terminal_id = terminal["id"].as_str().expect("terminal id").to_string();
    assert!(terminal_id.starts_with("term_"), "id: {terminal_id}");
    assert_eq!(terminal["session_id"], session_id);
    assert_eq!(terminal["cols"], 120);
    assert_eq!(terminal["rows"], 30);
    assert_eq!(terminal["status"], "running");
    assert!(
        terminal["cwd"]
            .as_str()
            .is_some_and(|cwd| !cwd.starts_with('/')),
        "cwd stays relative to the workspace: {}",
        terminal["cwd"]
    );
    assert!(
        terminal["shell"]
            .as_str()
            .is_some_and(|shell| !shell.is_empty()),
        "shell resolves to the detected default"
    );
    assert!(
        terminal["created_at"]
            .as_str()
            .is_some_and(|at| !at.is_empty()),
        "created_at is present"
    );
    assert!(
        terminal.get("exited_at").is_none(),
        "a running terminal carries no exited_at"
    );

    // The terminal appears in the list…
    let (status, body) = get(
        &client,
        &format!(
            "{}/api/v1/sessions/{session_id}/terminals",
            server.base_url()
        ),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    let items = body["data"]["items"].as_array().expect("items array");
    assert!(
        items.iter().any(|item| item["id"] == terminal_id),
        "the created terminal is listed: {items:?}"
    );

    // …and is fetchable on its own.
    let (status, body) = get(
        &client,
        &format!(
            "{}/api/v1/sessions/{session_id}/terminals/{terminal_id}",
            server.base_url()
        ),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["data"]["id"], terminal_id);
    assert_eq!(body["data"]["status"], "running");
}

/// Given a created terminal, when it is closed through the REST API, then
/// the close acknowledges and the terminal reports exited with an exit code.
#[tokio::test]
async fn should_close_a_terminal_and_report_it_as_exited() {
    let server = TestServer::start().await;
    let client = client();
    let session_id = create_session(&server, &client, "Terminal close").await;
    let (_, body) = post(
        &client,
        &format!(
            "{}/api/v1/sessions/{session_id}/terminals",
            server.base_url()
        ),
        json!({}),
    )
    .await;
    let terminal_id = body["data"]["id"]
        .as_str()
        .expect("terminal id")
        .to_string();

    let (status, body) = post(
        &client,
        &format!(
            "{}/api/v1/sessions/{session_id}/terminals/{terminal_id}/close",
            server.base_url()
        ),
        json!({}),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK, "terminal close succeeds");
    assert_eq!(body["data"], json!({ "closed": true }));

    // The exited spelling carries status/exit_code/exited_at. The pool
    // observes the child's exit asynchronously after the kill, so the
    // status is polled for briefly instead of asserted immediately (the
    // close→read race otherwise flakes under a fully parallel workspace
    // run).
    let mut body;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let (status, fetched) = get(
            &client,
            &format!(
                "{}/api/v1/sessions/{session_id}/terminals/{terminal_id}",
                server.base_url()
            ),
        )
        .await;
        assert_eq!(status, reqwest::StatusCode::OK);
        body = fetched;
        if body["data"]["status"] == "exited" || std::time::Instant::now() > deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(body["data"]["status"], "exited");
    assert!(
        body["data"].get("exit_code").is_some(),
        "exit_code rides the exited spelling"
    );
    assert!(
        body["data"]["exited_at"]
            .as_str()
            .is_some_and(|at| !at.is_empty()),
        "exited_at is present after the close"
    );
}

/// Given a terminal create body with an absolute cwd, when the terminal is
/// created, then the request fails the schema's relative-path rule instead
/// of spawning outside the session workspace.
#[tokio::test]
async fn should_reject_an_absolute_cwd_when_creating_a_terminal() {
    let server = TestServer::start().await;
    let client = client();
    let session_id = create_session(&server, &client, "Terminal validation").await;

    let (status, body) = post(
        &client,
        &format!(
            "{}/api/v1/sessions/{session_id}/terminals",
            server.base_url()
        ),
        json!({ "cwd": "/etc" }),
    )
    .await;
    assert_eq!(
        status,
        reqwest::StatusCode::OK,
        "errors answer in the envelope"
    );
    assert_eq!(body["code"], -32602, "invalid params: {body}");
}

/// Given an unknown session or terminal, when the terminal routes are
/// called, then the envelope answers the not-found application code.
#[tokio::test]
async fn should_answer_not_found_for_unknown_session_or_terminal() {
    let server = TestServer::start().await;
    let client = client();

    let (status, body) = get(
        &client,
        &format!(
            "{}/api/v1/sessions/session-nope/terminals",
            server.base_url()
        ),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], -32001, "unknown session: {body}");

    let session_id = create_session(&server, &client, "Terminal unknown").await;
    let (status, body) = get(
        &client,
        &format!(
            "{}/api/v1/sessions/{session_id}/terminals/term-nope",
            server.base_url()
        ),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], -32001, "unknown terminal: {body}");
}

/// Given a session that lives on a non-default workspace's routing, when the
/// terminal panel lists terminals, then the session resolves through the
/// routing seam instead of the default manager (which cannot see it).
#[tokio::test]
async fn should_list_terminals_when_the_session_lives_on_a_foreign_workspace() {
    // Given: a routing that owns a live session on an out-of-band manager
    let (routing, session_id) = crate::common::ForeignWorkspaceRouting::with_session("Foreign").await;
    let server = TestServer::start_with_session_routing(routing).await;
    let client = client();

    // When: the terminal panel lists the foreign session's terminals
    let (status, body) = get(
        &client,
        &format!("{}/api/v1/sessions/{session_id}/terminals", server.base_url()),
    )
    .await;

    // Then: the session resolves — an empty list, not a not-found error
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0, "terminal list resolves: {body}");
    assert_eq!(body["data"]["items"].as_array().map(Vec::len), Some(0));
}

// ===========================================================================
// Agent profiles REST surface (P6 — the web `/agents` contract)
// ===========================================================================

/// `PUT` helper (the profile update route is the first PUT on the wire).
async fn put(client: &reqwest::Client, url: &str, body: Value) -> (reqwest::StatusCode, Value) {
    let response = client
        .put(url)
        .json(&body)
        .send()
        .await
        .expect("PUT request");
    let status = response.status();
    let json_body = response.json().await.expect("JSON body");
    (status, json_body)
}

#[tokio::test]
async fn should_create_list_get_update_and_delete_profiles_when_the_rest_api_is_used() {
    // Given a server whose engine roots its Mirri home in a tempdir
    let home = tempfile::tempdir().expect("tempdir should succeed");
    let server = TestServer::start_with_home_dir(home.path().to_path_buf()).await;
    let client = client();
    let base = format!("{}/api/v1/agents", server.base_url());

    // When a profile is created over the REST contract (the full TS
    // `createProfileRequestSchema` surface — T19 m5 cleared)
    let (status, body) = post(
        &client,
        &base,
        json!({
            "name": "reviewer",
            "description": "Reviews code.",
            "default_model": "claude-sonnet-4-20250514",
            "tools": ["Read", "Grep"],
            "disallowed_tools": ["Bash"],
            "subagents": ["coder", "explore"],
            "capabilities_required": ["code.read"],
            "system_prompt_template": "You review code.",
            "when_to_use": "When a change needs review.",
        }),
    )
    .await;

    // Then the created row answers with the WireAgentProfile shape and
    // the profile is persisted under <home>/agents/ (the unified .md
    // format the discovery pass reads — the R4-1 write channel)
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0, "create succeeds: {body}");
    let data = &body["data"];
    assert_eq!(data["name"], "reviewer");
    assert_eq!(data["source"], "user");
    assert_eq!(data["builtin"], false);
    assert_eq!(data["essential"], false);
    assert_eq!(data["enabled"], true);
    assert_eq!(data["has_override"], false);
    assert_eq!(data["default_model"], "claude-sonnet-4-20250514");
    assert_eq!(data["tools"], json!(["Read", "Grep"]));
    assert_eq!(data["disallowed_tools"], json!(["Bash"]));
    assert_eq!(data["subagents"], json!(["coder", "explore"]));
    assert_eq!(data["capabilities_required"], json!(["code.read"]));
    assert_eq!(data["system_prompt_template"], "You review code.");
    assert!(
        home.path().join("agents").join("reviewer.md").is_file(),
        "the profile file must be persisted"
    );

    // And the list answers the {items, skipped} envelope with the built-ins
    // plus the new profile
    let (status, body) = get(&client, &base).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);
    let items = body["data"]["items"].as_array().expect("items array");
    let mut builtin_names: Vec<&str> = items
        .iter()
        .filter(|item| item["source"] == "builtin")
        .map(|item| item["name"].as_str().expect("a builtin row carries a name"))
        .collect();
    builtin_names.sort();
    assert_eq!(
        builtin_names,
        vec!["agent", "coder", "explore", "plan"],
        "the four built-in profiles are listed"
    );
    let reviewer = items
        .iter()
        .find(|item| item["name"] == "reviewer")
        .expect("the created profile is listed");
    assert!(reviewer["file_path"].as_str().is_some());
    assert_eq!(
        body["data"]["skipped"],
        json!([]),
        "a clean home skips nothing"
    );

    // And the single-profile read answers the same row
    let (status, body) = get(&client, &format!("{base}/reviewer")).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["name"], "reviewer");
    // SDR-014: a REST-created profile's prompt is composed onto the shared
    // base — the effective prompt is the base with the role text riding it,
    // never the bare role text alone.
    let effective = body["data"]["effective_system_prompt"]
        .as_str()
        .expect("effective_system_prompt is set");
    assert!(effective.contains("You review code."));
    assert!(
        effective.len() > "You review code.".len(),
        "the effective prompt carries the shared base, not just the role text"
    );

    // And a partial update merges (the description moves, the new-field
    // surface keeps its values, the prompt stays)
    let (status, body) = put(
        &client,
        &format!("{base}/reviewer"),
        json!({ "description": "Reviews code carefully." }),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["description"], "Reviews code carefully.");
    assert_eq!(body["data"]["disallowed_tools"], json!(["Bash"]));
    assert_eq!(
        body["data"]["system_prompt_template"], "You review code.",
        "an omitted field keeps its previous value"
    );

    // And the delete removes the row and the file
    let (status, body) = delete(&client, &format!("{base}/reviewer")).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);
    assert_eq!(body["data"]["deleted"], true);
    assert!(
        !home.path().join("agents").join("reviewer.md").exists(),
        "the profile file must be removed"
    );
    let (status, body) = get(&client, &format!("{base}/reviewer")).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_ne!(body["code"], 0, "the deleted profile is not found anymore");
}

/// Given a home whose profiles root holds a malformed `.md` (a parse
/// failure) and a file declaring the essential name, When the agents list
/// is fetched over the REST contract, Then the essential file LOADS as the
/// content override (D16) while the `skipped` diagnostics carry only the
/// genuine parse failure (D13 — load failures only).
#[tokio::test]
async fn should_report_skipped_files_when_the_agents_list_is_fetched() {
    // Given a home whose profiles directory holds a broken .md and an
    // essential-name declaration
    let home = tempfile::tempdir().expect("tempdir should succeed");
    let profiles_dir = home.path().join("profiles");
    std::fs::create_dir_all(&profiles_dir).expect("profiles dir must be creatable");
    std::fs::write(
        profiles_dir.join("broken.md"),
        "---\nname: broken\n---\nNo description here.",
    )
    .expect("broken .md must be writable");
    std::fs::write(
        profiles_dir.join("agent.md"),
        "---\nname: agent\ndescription: Imposter.\n---\nYou are not the default.",
    )
    .expect("essential .md must be writable");
    let server = TestServer::start_with_home_dir(home.path().to_path_buf()).await;
    let client = client();

    // When the agents list is fetched
    let (status, body) = get(&client, &format!("{}/api/v1/agents", server.base_url())).await;

    // Then the essential row is the user override (the covered-builtin
    // wire shape) and only the broken file is skipped
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);
    let items = body["data"]["items"]
        .as_array()
        .expect("items ride alongside skipped");
    let essential = items
        .iter()
        .find(|item| item["name"] == "agent")
        .expect("the essential override is listed");
    assert_eq!(essential["source"], "user");
    assert_eq!(essential["builtin"], false);
    assert_eq!(essential["has_override"], true);
    assert_eq!(essential["essential"], true);
    let skipped = body["data"]["skipped"]
        .as_array()
        .expect("skipped rides alongside items");
    assert_eq!(
        skipped.len(),
        1,
        "only the malformed file is skipped (D13): {skipped:?}"
    );
    let broken = skipped
        .iter()
        .find(|entry| {
            entry["path"]
                .as_str()
                .is_some_and(|path| path.ends_with("broken.md"))
        })
        .expect("the malformed file is reported");
    assert!(
        broken["reason"].as_str().is_some_and(
            |reason| reason.contains("Missing required frontmatter field \"description\"")
        ),
        "the parse failure is reported readably: {broken:?}"
    );
}

#[tokio::test]
async fn should_keep_created_profiles_when_a_new_server_shares_the_home() {
    // Given a server with one REST-created profile
    let home = tempfile::tempdir().expect("tempdir should succeed");
    let server = TestServer::start_with_home_dir(home.path().to_path_buf()).await;
    let client = client();
    let (status, body) = post(
        &client,
        &format!("{}/api/v1/agents", server.base_url()),
        json!({ "name": "survivor", "description": "persists" }),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);

    // When a new server instance boots over the same home (the restart
    // persistence path)
    let restarted = TestServer::start_with_home_dir(home.path().to_path_buf()).await;

    // Then the profile is still listed
    let (status, body) = get(&client, &format!("{}/api/v1/agents", restarted.base_url())).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(body["code"], 0);
    let items = body["data"]["items"].as_array().expect("items array");
    assert!(
        items.iter().any(|item| item["name"] == "survivor"),
        "the profile survives the restart"
    );
}

#[tokio::test]
async fn should_reject_essential_and_builtin_profile_writes_over_the_rest_api() {
    // Given a home-rooted server
    let home = tempfile::tempdir().expect("tempdir should succeed");
    let server = TestServer::start_with_home_dir(home.path().to_path_buf()).await;
    let client = client();
    let base = format!("{}/api/v1/agents", server.base_url());

    // When the essential default profile name is created over, Then the
    // envelope carries the TS-exact refusal (`agents.ts:700`; the
    // transport prefixes every invalid-params error with its RPC label)
    let (status, body) = post(&client, &base, json!({ "name": "agent" })).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_ne!(body["code"], 0, "the essential name is refused: {body}");
    assert_eq!(
        body["msg"], "invalid params: Cannot create a profile with the reserved name \"agent\"",
        "the TS-exact essential refusal (transport-prefixed): {body}"
    );

    // And a non-kebab name is refused with the TS-exact contract message
    // before any filesystem access (`agents.ts:149-166`)
    let (status, body) = post(
        &client,
        &base,
        json!({ "name": "../evil", "description": "x" }),
    )
    .await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(
        body["msg"],
        "invalid params: name must be kebab-case (lowercase alphanumeric segments separated by \
         single hyphens, e.g. \"code-reviewer\")",
        "the TS-exact kebab refusal: {body}"
    );

    // And a create without a description is refused (the invisible-profile
    // guard, `agents.ts:705-722`)
    let (status, body) = post(&client, &base, json!({ "name": "blank" })).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(
        body["msg"], "invalid params: description is required and must be a non-empty string",
        "the TS-exact description guard: {body}"
    );

    // And deleting a built-in without an override is refused (the TS-exact
    // builtin refusal, `agents.ts:997`)
    let (status, body) = delete(&client, &format!("{base}/coder")).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert_eq!(
        body["msg"], "invalid params: Cannot delete built-in profile \"coder\"",
        "the TS-exact builtin refusal: {body}"
    );

    // And an unknown profile read answers not-found with the TS-exact
    // message (`agents.ts:648`; the not-found transport label prefixes it)
    let (status, body) = get(&client, &format!("{base}/ghost")).await;
    assert_eq!(status, reqwest::StatusCode::OK);
    assert!(
        body["msg"]
            .as_str()
            .is_some_and(|msg| msg.contains("Agent profile \"ghost\" was not found")),
        "the TS-exact not-found message: {body}"
    );
}

}

mod ipc_tests {
//! Integration tests for the Unix-socket IPC transport: line-delimited
//! JSON-RPC 2.0 over a Unix domain socket. Naming follows the
//! Given-When-Then convention.
#![cfg(unix)]


use std::time::Duration;

use crate::common::TestServer;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

type ResponseLines = tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>;
type WriteHalf = tokio::net::unix::OwnedWriteHalf;

/// Connect to the test server's IPC socket (retrying briefly while the
/// async server task binds it) and return the response line reader plus
/// the write half.
async fn connect_ipc(server: &TestServer) -> (ResponseLines, WriteHalf) {
    let path = server
        .ipc_path
        .as_ref()
        .expect("test server booted with IPC");

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let stream = loop {
        match tokio::net::UnixStream::connect(path).await {
            Ok(stream) => break stream,
            Err(_) if std::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(error) => panic!("connect to the IPC socket: {error}"),
        }
    };
    let (read_half, write_half) = stream.into_split();
    (BufReader::new(read_half).lines(), write_half)
}

/// Send one JSON-RPC line (the transport's request unit).
async fn send(writer: &mut WriteHalf, line: &str) {
    writer.write_all(line.as_bytes()).await.expect("write line");
    writer.write_all(b"\n").await.expect("write newline");
    writer.flush().await.expect("flush");
}

/// Read the next response line as JSON, failing fast instead of hanging.
async fn next_response(lines: &mut ResponseLines) -> serde_json::Value {
    let line = tokio::time::timeout(Duration::from_secs(5), lines.next_line())
        .await
        .expect("response arrives within the timeout")
        .expect("stream stays open")
        .expect("a response line");
    serde_json::from_str(&line).expect("response line is JSON")
}

/// Assert that no further response arrives within a short window.
async fn assert_no_response(lines: &mut ResponseLines) {
    let stray = tokio::time::timeout(Duration::from_millis(300), lines.next_line()).await;
    assert!(
        stray.is_err(),
        "no response should arrive; got {:?}",
        stray.ok().and_then(|line| line.ok().flatten())
    );
}

/// Given a running IPC server, when a client sends a line-delimited
/// JSON-RPC request, then exactly one response line comes back echoing the
/// request id with a `result` payload.
#[tokio::test]
async fn should_answer_line_delimited_requests_over_the_unix_socket() {
    let server = TestServer::start_with_ipc().await;
    let (mut lines, mut writer) = connect_ipc(&server).await;

    send(
        &mut writer,
        r#"{"jsonrpc":"2.0","id":1,"method":"session.list","params":{}}"#,
    )
    .await;

    let response = next_response(&mut lines).await;
    assert_eq!(response["jsonrpc"], serde_json::json!("2.0"));
    assert_eq!(response["id"], serde_json::json!(1));
    assert!(
        response.get("result").is_some(),
        "response carries a result: {response}"
    );
}

/// Given a connected IPC client, when it sends a notification (no id) and
/// a stray response frame before a valid request, then only the valid
/// request is answered — the other frames stay silent.
#[tokio::test]
async fn should_stay_silent_for_notifications_and_response_frames() {
    let server = TestServer::start_with_ipc().await;
    let (mut lines, mut writer) = connect_ipc(&server).await;

    send(
        &mut writer,
        r#"{"jsonrpc":"2.0","method":"session.list","params":{}}"#,
    )
    .await;
    send(
        &mut writer,
        r#"{"jsonrpc":"2.0","result":{"ignored":true},"id":99}"#,
    )
    .await;
    send(
        &mut writer,
        r#"{"jsonrpc":"2.0","id":7,"method":"session.list","params":{}}"#,
    )
    .await;

    let response = next_response(&mut lines).await;
    assert_eq!(
        response["id"],
        serde_json::json!(7),
        "the first and only response serves the valid request: {response}"
    );

    assert_no_response(&mut lines).await;
}

/// Given a connected IPC client, when it sends a line that is not valid
/// JSON, then the server answers with a parse-error frame whose id is
/// null (parse errors are always answered).
#[tokio::test]
async fn should_answer_a_garbage_line_with_a_null_id_error_frame() {
    let server = TestServer::start_with_ipc().await;
    let (mut lines, mut writer) = connect_ipc(&server).await;

    send(&mut writer, "this is not json").await;

    let response = next_response(&mut lines).await;
    assert_eq!(response["id"], serde_json::Value::Null);
    assert_eq!(response["error"]["code"], serde_json::json!(-32700));
}

/// Given a connected IPC client, when it requests an unknown method with
/// an id, then the server answers with a method-not-found error that
/// echoes the id and names the method.
#[tokio::test]
async fn should_answer_unknown_methods_with_an_echoed_id_error() {
    let server = TestServer::start_with_ipc().await;
    let (mut lines, mut writer) = connect_ipc(&server).await;

    send(
        &mut writer,
        r#"{"jsonrpc":"2.0","id":42,"method":"no.such.method","params":{}}"#,
    )
    .await;

    let response = next_response(&mut lines).await;
    assert_eq!(response["id"], serde_json::json!(42));
    assert_eq!(response["error"]["code"], serde_json::json!(-32601));
    assert!(
        response["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("no.such.method")),
        "error names the method: {}",
        response["error"]["message"]
    );
}

}

mod skill_e2e_tests {
//! E2E closed loop for the builtin skill bundle (design 13, batch M3 —
//! the Done-when ③ "real e2e" leg).
//!
//! The server is a real `mirri serve`-shaped boot: the full assembly path
//! (`mirri_embedded::bootstrap::assemble` — config load, plugin manager,
//! the complete skill-discovery sequence that seeds the builtin bundle)
//! behind the serve transport, listening on an ephemeral loopback port.
//! Every step drives the wire protocol (JSON-RPC 2.0 over the
//! `/api/v1/rpc` WebSocket) — no in-process engine calls.
//!
//! The boot mirrors `mirri-cli`'s `serve` composition: the transport's
//! `ServerState` is built with the ENGINE's session manager (the same
//! `parts.session_manager`), so sessions created through the engine's RPC
//! surface resolve on the transport's session routing — the shared-manager
//! wiring the real binary uses (minus auth, web SPA, and the multi-root
//! watchers, none of which this loop touches).
//!
//! The loop, in one test (design 13 §6):
//!
//! 1. `skill.list` → the eight builtin entries with the optional gate
//!    flag projection;
//! 2. `agent.activateSkill` of `write-goal` → the embedded body injected
//!    into the turn (the builtin content table supply);
//! 3. a model-side `Skill` call of the user-only `mcp-config` → hard
//!    gate, error text "can only be triggered by the user";
//! 4. `skill.setEnabled` disable → the activation refused → restore →
//!    the activation succeeds again.
//!
//! The engine's LLM factory is swapped for a scripted one (the assembled
//! `EngineParts.llm_factory` seam) so the model side is observable: the
//! mock records every chat call's messages, which is how the test proves
//! the embedded skill body actually reached the model.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use mirri_core::llm::{Llm, LlmChatParams, LlmEvent, LlmEventStream, LlmToolCall, ModelCapability};
use mirri_core::{AgentScopeConfig, LlmFactory, WorkspaceId};
use mirri_embedded::EmbeddedServer;
use mirri_server::ServerState;
use mirri_transport::adapter::EngineAdapter;
use serde_json::{Value, json};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

type TestSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

// ===========================================================================
// Scripted, message-recording LLM (the e2e-local counterpart of the
// embedded suite's ScriptedLlm — no recording mock exists in
// mirri-test-harness, and the body-injection assertion needs the model's
// view of the turn)
// ===========================================================================

/// A scripted response: either a plain text end-turn reply or tool calls
/// that keep the loop running.
#[derive(Debug)]
enum ScriptedResponse {
    /// Plain text reply that ends the turn.
    Text(String),
    /// Tool calls that keep the loop running.
    ToolUse(Vec<LlmToolCall>),
}

/// A scripted [`Llm`] that records every chat call's serialized messages —
/// the observation point for "the embedded skill body entered the turn".
#[derive(Debug, Default)]
struct ScriptedLlm {
    responses: Mutex<VecDeque<ScriptedResponse>>,
    recorded_messages: Mutex<Vec<serde_json::Value>>,
}

impl ScriptedLlm {
    fn new() -> Self {
        Self::default()
    }

    /// Queue a plain text reply.
    fn push_text(&self, text: &str) {
        self.responses
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push_back(ScriptedResponse::Text(text.to_string()));
    }

    /// Queue a tool-use reply.
    fn push_tool_use(&self, tool_calls: Vec<LlmToolCall>) {
        self.responses
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push_back(ScriptedResponse::ToolUse(tool_calls));
    }

    /// The serialized messages of the n-th chat call (0-based).
    fn messages_of_call(&self, call: usize) -> serde_json::Value {
        self.recorded_messages
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(call)
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    }
}

impl Llm for ScriptedLlm {
    fn system_prompt(&self) -> &str {
        "You are a scripted e2e assistant."
    }

    fn model_name(&self) -> &str {
        "scripted-e2e-llm"
    }

    fn capability(&self) -> &ModelCapability {
        static CAPABILITY: ModelCapability = ModelCapability {
            image_in: false,
            video_in: false,
            audio_in: false,
            dynamically_loaded_tools: false,
        };
        &CAPABILITY
    }

    fn chat(&self, params: LlmChatParams) -> mirri_core::EngineResult<LlmEventStream> {
        self.recorded_messages
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(serde_json::to_value(&params.messages).unwrap_or_default());
        // Exhausted queue: a placeholder reply, never an empty response
        // (the empty-response guard fails empty-but-completed turns).
        let response = self
            .responses
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop_front()
            .unwrap_or_else(|| ScriptedResponse::Text("(no more scripted replies)".to_string()));
        let mut events = VecDeque::new();
        match response {
            ScriptedResponse::Text(text) => {
                for ch in text.chars() {
                    events.push_back(LlmEvent::TextDelta {
                        text: ch.to_string(),
                    });
                }
                if !text.is_empty() {
                    events.push_back(LlmEvent::TextPart { text });
                }
                events.push_back(LlmEvent::Usage {
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                });
                events.push_back(LlmEvent::Done {
                    stop_reason: "completed".to_string(),
                });
            }
            ScriptedResponse::ToolUse(tool_calls) => {
                for tool_call in &tool_calls {
                    events.push_back(LlmEvent::ToolCall {
                        tool: tool_call.name.clone(),
                        args: tool_call.args.clone(),
                        call_id: tool_call.id.clone(),
                    });
                }
                events.push_back(LlmEvent::Usage {
                    input_tokens: 0,
                    output_tokens: 0,
                    cache_read_tokens: 0,
                    cache_creation_tokens: 0,
                });
                events.push_back(LlmEvent::Done {
                    stop_reason: "tool_calls".to_string(),
                });
            }
        }
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

/// An [`LlmFactory`] serving one shared [`ScriptedLlm`].
struct ScriptedLlmFactory {
    llm: std::sync::Arc<ScriptedLlm>,
}

impl LlmFactory for ScriptedLlmFactory {
    fn create_llm(&self, _config: &AgentScopeConfig) -> Result<std::sync::Arc<dyn Llm>, String> {
        Ok(self.llm.clone() as std::sync::Arc<dyn Llm>)
    }
}

// ===========================================================================
// Wire helpers (handshake, frames, RPC calls)
// ===========================================================================

/// Connect to the unified JSON-RPC WebSocket endpoint (`/api/v1/rpc`).
async fn connect_ws(ws_rpc_url: &str) -> TestSocket {
    connect_async(ws_rpc_url)
        .await
        .expect("websocket upgrade")
        .0
}

/// Read the next text frame, parsing it into one or more JSON values
/// (a batched array frame is flattened into its members).
async fn next_frames(socket: &mut TestSocket) -> Vec<Value> {
    let message = tokio::time::timeout(Duration::from_secs(15), socket.next())
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

/// The client's wire demultiplexer: one socket carries BOTH the responses
/// and the subscribed event notifications, and an engine turn's events are
/// published BEFORE the fire-mode call's response arrives — so a response
/// hunt must PRESERVE the interleaved event frames instead of skipping
/// them (they land in a backlog the event waits drain first).
struct Wire {
    socket: TestSocket,
    /// Event notifications read while hunting a response (arrival order).
    events: VecDeque<Value>,
}

impl Wire {
    /// Connect to the endpoint and complete the handshake.
    async fn connect(ws_rpc_url: &str) -> Self {
        let mut socket = connect_ws(ws_rpc_url).await;
        let hello = {
            let mut frames = next_frames(&mut socket).await;
            assert_eq!(
                frames.len(),
                1,
                "expected a single greeting frame, got {frames:?}"
            );
            frames.remove(0)
        };
        assert_eq!(
            hello["method"], "server_hello",
            "first frame is the server greeting"
        );
        socket
            .send(Message::Text(
                r#"{"id":1,"method":"client_hello","params":{"clientId":"skill-e2e-client","capabilities":{"reverseRpc":["approval","question","fs","terminal"]}}}"#
                    .into(),
            ))
            .await
            .expect("send client_hello");
        let mut handshake_events = VecDeque::new();
        let ack = Self::response(&mut socket, 1, &mut handshake_events).await;
        assert_eq!(ack["result"]["accepted"], true, "handshake accepted: {ack}");
        Self {
            socket,
            events: VecDeque::new(),
        }
    }

    /// The raw response hunt: read frames until the response with the given
    /// id arrives, preserving interleaved event notifications in `events`.
    async fn response(socket: &mut TestSocket, id: i64, events: &mut VecDeque<Value>) -> Value {
        loop {
            for frame in next_frames(socket).await {
                if frame.get("id").and_then(Value::as_i64) == Some(id) {
                    return frame;
                }
                if frame
                    .get("method")
                    .and_then(Value::as_str)
                    .is_some_and(|method| method.starts_with("event."))
                {
                    events.push_back(frame);
                }
            }
        }
    }

    /// Send one JSON-RPC request and return the matching response.
    async fn call(&mut self, id: i64, method: &str, params: Value) -> Value {
        self.socket
            .send(Message::Text(
                json!({ "id": id, "method": method, "params": params })
                    .to_string()
                    .into(),
            ))
            .await
            .expect("send rpc request");
        Self::response(&mut self.socket, id, &mut self.events).await
    }

    /// Read until an `event.*` notification matching the predicate arrives —
    /// the backlog (events preserved by the response hunts) is drained first.
    async fn event_where(&mut self, predicate: impl Fn(&Value) -> bool) -> Value {
        if let Some(index) = self.events.iter().position(&predicate) {
            return self.events.remove(index).expect("the position found above");
        }
        loop {
            for frame in next_frames(&mut self.socket).await {
                let is_event = frame
                    .get("method")
                    .and_then(Value::as_str)
                    .is_some_and(|method| method.starts_with("event."));
                if is_event {
                    if predicate(&frame) {
                        return frame;
                    }
                    self.events.push_back(frame);
                }
            }
        }
    }
}

// ===========================================================================
// Server boot — the `mirri serve` composition on an ephemeral port
// ===========================================================================

/// Boot a real server on an ephemeral loopback port: the engine takes the
/// FULL assembly path (config load + the complete skill-discovery sequence
/// that seeds the builtin bundle) with the test's scripted LLM factory
/// swapped into the assembled parts, and the transport's `ServerState` is
/// built with the ENGINE's session manager (the shared-manager wiring
/// `mirri serve` uses, minus auth / web SPA / watchers). Returns the
/// `/api/v1/rpc` WebSocket URL; the temp roots outlive the test.
async fn builtin_skill_server(
    llm: std::sync::Arc<ScriptedLlm>,
) -> (String, tempfile::TempDir, tempfile::TempDir) {
    let workspace = tempfile::tempdir().expect("workspace tempdir");
    let home = tempfile::tempdir().expect("home tempdir");
    // The create chain requires a resolvable model (spec 11 R4 — no
    // built-in fallback), so the injected home carries a config.toml seed,
    // exactly like a real user's home would.
    std::fs::write(
        home.path().join("config.toml"),
        "default_model = \"test-default-model\"\n",
    )
    .expect("seed config.toml");
    // Hermetic discovery: a non-empty explicit skillDirs list replaces the
    // user/project default roots (which would otherwise read the ambient
    // HOME's `~/.agents/skills` of the machine running the test) — the
    // builtin bundle rides the explicit branch unchanged (design 13 §3.3),
    // so the wire list is exactly the eight builtin entries.
    let explicit_skill_dir = workspace.path().join("explicit-skills");
    std::fs::create_dir_all(&explicit_skill_dir).expect("explicit skill dir");
    let mut parts = mirri_embedded::bootstrap::assemble(
        WorkspaceId::new("ws-skill-e2e"),
        workspace.path().to_path_buf(),
        Some(home.path().to_path_buf()),
        None,
        mirri_embedded::bootstrap::WorkspacePolicy::UntrustedDefault,
        vec![explicit_skill_dir],
    );
    parts.llm_factory = std::sync::Arc::new(ScriptedLlmFactory { llm });
    // The shared session manager: cloned BEFORE `from_parts` consumes the
    // parts, then handed to the transport so the engine-created sessions
    // resolve on the transport's session routing.
    let session_manager = std::sync::Arc::clone(&parts.session_manager);
    let engine: std::sync::Arc<dyn EngineAdapter> =
        std::sync::Arc::new(EmbeddedServer::from_parts(parts));
    let state = ServerState::with_engine(engine, session_manager, "0.1.0-test".to_string(), None);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        if let Err(error) = mirri_server::http::serve(listener, state, None).await {
            std::eprintln!("test server exited with an error: {error}");
        }
    });
    (format!("ws://{addr}/api/v1/rpc"), workspace, home)
}

// ===========================================================================
// The closed loop
// ===========================================================================

/// The `(name, pseudo path, gate)` wire expectations for the eight builtin
/// entries — the TS literals verbatim (design 13 §3.1) and the registration
/// layer's gate flags (six user-slash-only entries carry
/// `disable_model_invocation`; write-goal / update-config carry neither).
const BUILTIN_WIRE_EXPECTATIONS: [(&str, &str, bool); 8] = [
    ("mcp-config", "builtin://mcp-config", true),
    (
        "import-from-cc-codex",
        "builtin://import-from-cc-codex",
        true,
    ),
    ("update-config", "builtin://update-config", false),
    ("custom-theme", "builtin://custom-theme", true),
    ("write-goal", "builtin://write-goal", false),
    ("sub-skill", "builtin://sub-skill", true),
    ("sub-skill.review", "builtin://sub-skill/review", true),
    (
        "sub-skill.consolidate",
        "builtin://sub-skill/consolidate",
        true,
    ),
];

/// E2E: serve the builtin skill bundle over a real port and close the loop
/// — list (with the gate projection) → activate (embedded body in the turn)
/// → model-side call gated → setEnabled disable/refuse/restore.
#[tokio::test]
async fn should_close_the_builtin_skill_loop_when_a_real_server_serves_the_wire() {
    // Given a real server on an ephemeral port: the full assembly path
    // (the builtin bundle seeded through the real discovery sequence)
    // behind the serve transport, with a scripted recording LLM
    let mock = std::sync::Arc::new(ScriptedLlm::new());
    let (ws_rpc_url, _workspace, _home) = builtin_skill_server(std::sync::Arc::clone(&mock)).await;
    let mut wire = Wire::connect(&ws_rpc_url).await;

    // When the workspace skill list is read over the wire
    let listed = wire.call(2, "skill.list", json!({})).await;

    // Then the eight builtin entries are listed with their TS-literal
    // pseudo paths, and the optional gate flag is emitted only when set
    let entries = listed["result"].as_array().expect("the bare array");
    assert_eq!(entries.len(), 8, "entries: {entries:?}");
    for (name, path, gated) in BUILTIN_WIRE_EXPECTATIONS {
        let entry = entries
            .iter()
            .find(|entry| entry["name"] == name)
            .unwrap_or_else(|| panic!("the builtin entry {name} must be listed"));
        assert_eq!(entry["source"], "builtin", "{name} source");
        assert_eq!(entry["path"], path, "{name} pseudo path");
        if gated {
            assert_eq!(
                entry["disable_model_invocation"], true,
                "{name} must carry the gate flag"
            );
        } else {
            assert!(
                entry.get("disable_model_invocation").is_none(),
                "{name} must NOT carry the gate flag: {entry}"
            );
        }
    }

    // And a titled session subscribed to events (the explicit title keeps
    // the side-title LLM call out of the scripted queue) with yolo
    // permissions so the model's Skill call runs unattended
    let created = wire
        .call(3, "session.create", json!({ "title": "builtin e2e" }))
        .await;
    let session_id = created["result"]["id"]
        .as_str()
        .expect("session.create returns the id")
        .to_string();
    let subscribe = wire
        .call(4, "session.subscribe", json!({ "sessionId": session_id }))
        .await;
    assert!(
        subscribe["result"]["seq"].as_u64().is_some(),
        "subscribed: {subscribe}"
    );
    wire.call(
        5,
        "agent.setPermission",
        json!({ "sessionId": session_id, "mode": "yolo" }),
    )
    .await;

    // And the user-slash activation of the builtin write-goal succeeds —
    // the embedded body enters the turn through the builtin content table
    mock.push_text("Goal drafted.");
    let activated = wire
        .call(
            6,
            "agent.activateSkill",
            json!({
                "sessionId": session_id,
                "name": "write-goal",
                "args": "ship the beta"
            }),
        )
        .await;
    assert_eq!(activated["result"]["ok"], true, "activation: {activated}");
    assert_eq!(activated["result"]["skill"], "write-goal");

    // Then the activation is announced with the builtin provenance and the
    // turn settles
    let event = wire
        .event_where(|frame| {
            frame["method"] == "event.skill.activated"
                && frame["params"]["skillName"] == "write-goal"
        })
        .await;
    assert_eq!(event["params"]["trigger"], "user-slash");
    assert_eq!(event["params"]["skillSource"], "builtin");
    assert_eq!(event["params"]["skillPath"], "builtin://write-goal");
    assert_eq!(event["params"]["args"], "ship the beta");
    let _ = wire
        .event_where(|frame| frame["method"] == "event.turn.ended")
        .await;

    // And the model's first call saw the embedded body injected into the
    // turn (the content-table supply behind the rendered skill prompt)
    let first_call = serde_json::to_string(&mock.messages_of_call(0)).unwrap_or_default();
    assert!(
        first_call.contains(r#"<kimi-skill-loaded name=\"write-goal\" trigger=\"user-slash\""#),
        "the model must see the rendered builtin skill prompt: {first_call}"
    );
    assert!(
        first_call.contains("completion contract"),
        "the embedded write-goal body must be inside the turn: {first_call}"
    );

    // And the model-side Skill call of the user-only mcp-config is hard
    // gated — the tool result carries the user-trigger-only error
    mock.push_tool_use(vec![LlmToolCall {
        id: "call_mcp".to_string(),
        name: "Skill".to_string(),
        args: json!({ "skill": "mcp-config" }),
    }]);
    mock.push_text("That skill is user-only; I told the user.");
    wire.call(
        7,
        "message.send",
        json!({ "sessionId": session_id, "text": "configure mcp", "mode": "fire" }),
    )
    .await;
    let tool_result = wire
        .event_where(|frame| {
            frame["method"] == "event.tool.result" && frame["params"]["name"] == "Skill"
        })
        .await;
    assert_eq!(
        tool_result["params"]["isError"], true,
        "frame: {tool_result}"
    );
    assert!(
        tool_result["params"]["output"]
            .as_str()
            .is_some_and(|output| output.contains("can only be triggered by the user")),
        "the gate error must reach the model: {tool_result}"
    );
    let _ = wire
        .event_where(|frame| frame["method"] == "event.turn.ended")
        .await;

    // And the management face closes the loop: disable write-goal → the
    // activation is refused → restore → the activation succeeds again
    let disabled = wire
        .call(
            8,
            "skill.setEnabled",
            json!({ "name": "write-goal", "enabled": false }),
        )
        .await;
    assert_eq!(disabled["result"]["registered"], true, "{disabled}");
    assert_eq!(disabled["result"]["disabled_skills"], json!(["write-goal"]));

    let refused = wire
        .call(
            9,
            "agent.activateSkill",
            json!({ "sessionId": session_id, "name": "write-goal" }),
        )
        .await;
    assert!(
        refused.get("error").is_some(),
        "the disabled builtin must not activate: {refused}"
    );
    assert!(
        refused["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("disabled")),
        "the refusal must carry the disabled error: {refused}"
    );

    let restored_flag = wire
        .call(
            10,
            "skill.setEnabled",
            json!({ "name": "write-goal", "enabled": true }),
        )
        .await;
    assert_eq!(
        restored_flag["result"]["registered"], true,
        "{restored_flag}"
    );

    mock.push_text("Goal drafted after restore.");
    let restored = wire
        .call(
            11,
            "agent.activateSkill",
            json!({ "sessionId": session_id, "name": "write-goal" }),
        )
        .await;
    assert_eq!(restored["result"]["ok"], true, "restored: {restored}");
    let _ = wire
        .event_where(|frame| {
            frame["method"] == "event.skill.activated"
                && frame["params"]["skillName"] == "write-goal"
        })
        .await;
    let _ = wire
        .event_where(|frame| frame["method"] == "event.turn.ended")
        .await;
}

// ===========================================================================
// The session-routed skill.list face (the composer `/` menu path)
// ===========================================================================

/// A two-workspace skill assembly: the bound default root (no skills) plus
/// a second workspace whose root carries `.agents/skills/proj-skill`.
/// Discovery rides the DEFAULT roots (no explicit skillDirs — unlike
/// [`builtin_skill_server`], whose hermetic replacement kills project
/// discovery), mirroring the desktop `mirri web` path. The second workspace
/// is registered over the wire (`workspace.createOrTouch`), not by
/// pre-seeding `workspaces.json`.
///
/// Returns `(ws_rpc_url, second_root, second_workspace_id, keep_alive)`
/// where `keep_alive` keeps every harness tempdir alive for the whole
/// test — the spawned server holds all three roots, and dropping them
/// early would delete directories the registration still references.
async fn two_workspace_skill_server(
    llm: std::sync::Arc<ScriptedLlm>,
) -> (String, String, String, TwoWorkspaceKeepAlive) {
    let bound_root = tempfile::tempdir().expect("bound workspace tempdir");
    let second_root = tempfile::tempdir().expect("second workspace tempdir");
    let home = tempfile::tempdir().expect("home tempdir");
    // The create chain requires a resolvable model (spec 11 R4).
    std::fs::write(
        home.path().join("config.toml"),
        "default_model = \"test-default-model\"\n",
    )
    .expect("seed config.toml");
    // The second workspace's project-level skill.
    let skill_dir = second_root.path().join(".agents/skills/proj-skill");
    std::fs::create_dir_all(&skill_dir).expect("create project skill dir");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\ndescription: A second-workspace project skill.\n---\n# proj-skill\n",
    )
    .expect("write SKILL.md");
    let mut parts = mirri_embedded::bootstrap::assemble(
        WorkspaceId::new("ws-skill-session-e2e"),
        bound_root.path().to_path_buf(),
        Some(home.path().to_path_buf()),
        None,
        mirri_embedded::bootstrap::WorkspacePolicy::UntrustedDefault,
        Vec::new(),
    );
    parts.llm_factory = std::sync::Arc::new(ScriptedLlmFactory { llm });
    let session_manager = std::sync::Arc::clone(&parts.session_manager);
    let engine: std::sync::Arc<dyn EngineAdapter> =
        std::sync::Arc::new(EmbeddedServer::from_parts(parts));
    let state = ServerState::with_engine(engine, session_manager, "0.1.0-test".to_string(), None);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    tokio::spawn(async move {
        if let Err(error) = mirri_server::http::serve(listener, state, None).await {
            std::eprintln!("test server exited with an error: {error}");
        }
    });
    (
        format!("ws://{addr}/api/v1/rpc"),
        second_root.path().to_string_lossy().into_owned(),
        mirri_core::encode_work_dir_key(second_root.path()),
        TwoWorkspaceKeepAlive {
            _bound_root: bound_root,
            _second_root: second_root,
            _home: home,
        },
    )
}

/// The harness tempdirs the spawned server still references — returned so
/// the caller's binding keeps them alive for the whole test.
struct TwoWorkspaceKeepAlive {
    _bound_root: tempfile::TempDir,
    _second_root: tempfile::TempDir,
    _home: tempfile::TempDir,
}

/// Extract the `result` array of a `skill.list` response into owned names.
fn skill_names_of(listed: &Value) -> Vec<String> {
    listed["result"]
        .as_array()
        .expect("the bare array")
        .iter()
        .filter_map(|entry| entry["name"].as_str().map(str::to_string))
        .collect()
}

/// E2E: the session-routed `skill.list { sessionId }` must answer with the
/// session's OWN workspace catalog — the composer `/` menu path. A session
/// of a second (non-default) workspace sees that workspace's project
/// skills; a session of the default workspace does not.
#[tokio::test]
async fn should_list_project_skills_when_skill_list_is_routed_by_session_id() {
    // Given a two-workspace server whose second root carries a project skill
    let mock = std::sync::Arc::new(ScriptedLlm::new());
    let (ws_rpc_url, second_root, second_id, _keep_alive) =
        two_workspace_skill_server(std::sync::Arc::clone(&mock)).await;
    let mut wire = Wire::connect(&ws_rpc_url).await;

    // And the second workspace registered over the wire (the desktop path)
    let touched = wire
        .call(2, "workspace.createOrTouch", json!({ "root": second_root }))
        .await;
    assert!(
        touched["result"]["id"].as_str().is_some(),
        "createOrTouch returns the id: {touched}"
    );

    // And a session created IN the second workspace
    let created = wire
        .call(
            3,
            "session.create",
            json!({ "title": "proj skills", "workspaceId": second_id }),
        )
        .await;
    let session_id = created["result"]["id"]
        .as_str()
        .expect("session.create returns the id")
        .to_string();

    // When skill.list is routed by sessionId (the composer `/` menu path)
    let listed = wire
        .call(4, "skill.list", json!({ "sessionId": session_id }))
        .await;

    // Then the second workspace's project skill is listed
    let names = skill_names_of(&listed);
    assert!(
        names.iter().any(|name| name == "proj-skill"),
        "the session's workspace project skill must be listed, got: {names:?}"
    );

    // And the workspace-routed face agrees (the existing correct path)
    let ws_listed = wire
        .call(5, "skill.list", json!({ "workspaceId": second_id }))
        .await;
    let ws_names = skill_names_of(&ws_listed);
    assert!(
        ws_names.iter().any(|name| name == "proj-skill"),
        "the workspace-routed list must carry the project skill, got: {ws_names:?}"
    );

    // And a session of the DEFAULT workspace does not see it
    let default_created = wire
        .call(6, "session.create", json!({ "title": "default ws" }))
        .await;
    let default_sid = default_created["result"]["id"]
        .as_str()
        .expect("session.create returns the id")
        .to_string();
    let default_listed = wire
        .call(7, "skill.list", json!({ "sessionId": default_sid }))
        .await;
    let default_names = skill_names_of(&default_listed);
    assert!(
        !default_names.iter().any(|name| name == "proj-skill"),
        "the default workspace's session must NOT see the second workspace's \
         project skill, got: {default_names:?}"
    );
}

}

mod ws_rpc_tests {
//! Integration tests for the unified JSON-RPC 2.0 WebSocket endpoint
//! (`/api/v1/rpc`).
//!
//! Clients connect with `tokio-tungstenite`, complete the `server_hello` /
//! `client_hello` handshake, then exercise forward RPC, event subscription
//! with cursor replay, heartbeat ping/pong, and bearer-subprotocol auth.
//! Naming follows the Given-When-Then convention.


use std::sync::Arc;
use std::time::Duration;

use crate::common::TestServer;
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
// Provider credentials on the serve wire (ADR-010)
// ===========================================================================

#[tokio::test]
async fn should_store_and_list_a_provider_credential_over_the_serve_wire_when_credential_set_is_called() {
    // Given a connected client on a plain server (the credential store lives
    // in the engine's own temp home — no session needed)
    let server = TestServer::start().await;
    let mut socket = connected_socket(&server).await;

    // When the client sets a provider key (the both-method the web settings
    // page's key dialog calls — ADR-010: the serve wire reaches the embedded
    // handler through the shared dispatch's engine fall-through)
    socket
        .send(Message::Text(
            r#"{"id":9,"method":"credential.set","params":{"providerId":"test-provider","value":{"type":"key","key":"test-key-123"}}}"#.into(),
        ))
        .await
        .expect("send request");

    // Then the write succeeds with a noResult (JSON null) — not the
    // -32601 method-not-found the embedded-only availability used to answer
    let response = next_response(&mut socket, 9).await;
    assert_eq!(response["error"], Value::Null, "credential.set must not be gated off the serve wire: {response}");
    assert_eq!(response["result"], Value::Null);

    // And the store's list projection carries the provider's row with its
    // type and timestamps — but never the key material itself (F.1: no
    // plaintext crosses the wire)
    socket
        .send(Message::Text(
            r#"{"id":10,"method":"credential.list","params":{}}"#.into(),
        ))
        .await
        .expect("send request");
    let response = next_response(&mut socket, 10).await;
    assert_eq!(response["error"], Value::Null);
    let entries = response["result"].as_array().expect("credential.list answers a bare array");
    let row = entries
        .iter()
        .find(|entry| entry["provider_id"] == "test-provider")
        .expect("the set credential is listed");
    assert_eq!(row["type"], "key");
    assert!(row["time_created"].as_u64().is_some(), "row carries time_created: {row}");
    assert!(row["status"].is_string(), "row carries a status: {row}");
    assert!(
        row.get("key").is_none(),
        "no key material in the list projection: {row}"
    );
    assert!(
        !response.to_string().contains("test-key-123"),
        "the raw list response never carries the key material: {response}"
    );
}

#[tokio::test]
async fn should_report_status_and_remove_credential_over_the_serve_wire() {
    // Given a connected client with one stored credential
    let server = TestServer::start().await;
    let mut socket = connected_socket(&server).await;
    socket
        .send(Message::Text(
            r#"{"id":11,"method":"credential.set","params":{"providerId":"status-provider","value":{"type":"key","key":"test-key-456"}}}"#.into(),
        ))
        .await
        .expect("send request");
    assert_eq!(next_response(&mut socket, 11).await["error"], Value::Null);

    // When the client asks for the provider's credential health, the answer
    // is the honest no-evidence state (D.2: no recorded failure)
    socket
        .send(Message::Text(
            r#"{"id":12,"method":"credential.status","params":{"providerId":"status-provider"}}"#.into(),
        ))
        .await
        .expect("send request");
    let response = next_response(&mut socket, 12).await;
    assert_eq!(response["error"], Value::Null);
    assert_eq!(response["result"]["status"], "normal");

    // And removing the credential succeeds, while a second remove of the
    // now-missing entry is still a successful no-op (store semantics)
    socket
        .send(Message::Text(
            r#"{"id":13,"method":"credential.remove","params":{"providerId":"status-provider"}}"#.into(),
        ))
        .await
        .expect("send request");
    let response = next_response(&mut socket, 13).await;
    assert_eq!(response["error"], Value::Null, "credential.remove over the serve wire: {response}");
    socket
        .send(Message::Text(
            r#"{"id":14,"method":"credential.remove","params":{"providerId":"status-provider"}}"#.into(),
        ))
        .await
        .expect("send request");
    let response = next_response(&mut socket, 14).await;
    assert_eq!(
        response["error"], Value::Null,
        "removing a missing entry is a successful no-op: {response}"
    );
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

/// The shell the terminal tests spawn: an rc-free POSIX shell so the suite is
/// hermetic. Left to the default, the PTY runs the developer's interactive
/// shell, whose rc files (nvm, oh-my-zsh, …) can take seconds to load — the
/// burst test below then times out waiting for output that has not started
/// yet, while the same code passes on a runner with a bare rc.
#[cfg(not(windows))]
const TEST_SHELL: Option<&str> = Some("/bin/sh");
#[cfg(windows)]
const TEST_SHELL: Option<&str> = None;

/// Create a terminal through the REST API and return its wire object.
async fn create_terminal(server: &TestServer, session_id: &str) -> Value {
    let client = reqwest::Client::new();
    let response = client
        .post(format!(
            "{}/api/v1/sessions/{session_id}/terminals",
            server.base_url()
        ))
        .json(&json!({ "shell": TEST_SHELL }))
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

    // Do NOT read from the socket: the forwarder's outbox backs up once the
    // writer can no longer drain into the loopback socket, and only then
    // does the live broadcast channel (capacity 256) overflow so the
    // forwarder observes `RecvError::Lagged`; the ring keeps the last 256
    // for replay.
    //
    // The flood must exceed what the kernel can absorb without the peer
    // reading: the server's send buffer and the client's receive buffer
    // both autotune, and their ceilings differ per platform. 4000 × 8 KiB
    // (≈32 MiB) is several times above the largest default ceilings
    // (≈10 MiB combined), so the stall is a property of the test, not of
    // the runner's buffer configuration — a flood sized at the boundary
    // lets a roomy runner keep up and the lag never materialises. The
    // periodic yield keeps the log task scheduled so the events actually
    // reach the live channel.
    let big = "X".repeat(8 * 1024);
    for i in 0..4000 {
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
    // stamped every one) — the client adopts it as its new cursor.
    assert!(
        resync["params"]["currentSeq"].as_u64().expect("currentSeq") >= 4000,
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

/// Given a session that lives on a non-default workspace's routing, when the
/// terminal panel attaches a terminal over the WS RPC connection, then the
/// session's terminal pool resolves through the routing seam and the attach
/// is acknowledged.
#[tokio::test]
async fn should_attach_a_terminal_when_the_session_lives_on_a_foreign_workspace() {
    // Given: a routing that owns a live foreign session with one terminal
    let (routing, session_id) = crate::common::ForeignWorkspaceRouting::with_session("Foreign").await;
    let pool = routing
        .manager
        .get_session(&SessionId::new(session_id.clone()))
        .await
        .expect("session live on foreign manager")
        .terminal_pool();
    let terminal_id = pool
        .create_terminal(std::env::temp_dir(), None, 80, 24)
        .await
        .expect("create terminal on foreign session");
    let server = TestServer::start_with_session_routing(routing).await;
    let mut socket = connected_socket(&server).await;

    // When: the client attaches to that terminal
    let ack = terminal_call(
        &mut socket,
        901,
        "terminal_attach",
        json!({ "session_id": session_id, "terminal_id": terminal_id }),
    )
    .await;

    // Then: the attach is acknowledged (not session-not-found)
    assert_eq!(ack["result"], json!({ "ok": true }), "attach ack: {ack}");
}

}

mod ws_tests {
//! Integration tests for the multiplexed WebSocket endpoint (`/api/v1/ws`).
//!
//! Clients connect with `tokio-tungstenite`, complete the `server_hello` /
//! `client_hello` handshake, then subscribe to session event streams.
//! Session events arrive wrapped in `session_event` frames that carry the
//! id of the session they belong to.


use std::time::Duration;

use crate::common::TestServer;
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

type TestSocket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// Create a session through the HTTP API and return its id.
async fn create_session(server: &TestServer, client: &reqwest::Client, title: &str) -> String {
    let response = client
        .post(format!("{}/api/v1/sessions", server.base_url()))
        .json(&json!({ "title": title }))
        .send()
        .await
        .expect("create session");
    let body: Value = response.json().await.expect("JSON body");
    body["data"]["id"].as_str().expect("session id").to_string()
}

/// Connect to the multiplexed WebSocket endpoint.
async fn connect_ws(server: &TestServer) -> TestSocket {
    connect_async(server.ws_url())
        .await
        .expect("websocket upgrade")
        .0
}

/// Wait for the next text frame and parse it as JSON.
async fn next_message(socket: &mut TestSocket) -> Value {
    let message = tokio::time::timeout(Duration::from_secs(5), socket.next())
        .await
        .expect("message arrives within timeout")
        .expect("stream stays open")
        .expect("text message");
    serde_json::from_str(message.to_text().expect("UTF-8 text message"))
        .expect("message is valid JSON")
}

/// Assert the `server_hello` greeting and answer it with a `client_hello`.
async fn complete_handshake(socket: &mut TestSocket) {
    let hello = next_message(socket).await;
    assert_eq!(
        hello["type"], "server_hello",
        "first frame is the server greeting"
    );
    assert!(
        hello["serverId"].as_str().is_some_and(|id| !id.is_empty()),
        "server_hello carries a serverId"
    );
    assert!(
        hello["version"]
            .as_str()
            .is_some_and(|version| !version.is_empty()),
        "server_hello carries a version"
    );
    socket
        .send(Message::Text(
            r#"{"type":"client_hello","clientId":"test-client"}"#.into(),
        ))
        .await
        .expect("send client_hello");
}

/// Connect and complete the handshake.
async fn connected_socket(server: &TestServer) -> TestSocket {
    let mut socket = connect_ws(server).await;
    complete_handshake(&mut socket).await;
    socket
}

/// Send a `subscribe` frame and return the server's response.
async fn subscribe(socket: &mut TestSocket, session_id: &str) -> Value {
    socket
        .send(Message::Text(
            json!({ "type": "subscribe", "sessionId": session_id })
                .to_string()
                .into(),
        ))
        .await
        .expect("send subscribe");
    next_message(socket).await
}

/// Publish a `message_received` event on the given session's event bus.
fn publish_message(server: &TestServer, session_id: &str, role: &str, summary: &str) {
    let artifacts = server
        .state
        .session_manager
        .session_artifacts(&mirri_core::SessionId::new(session_id.to_string()))
        .expect("session artifacts");
    artifacts
        .event_bus
        .publish(mirri_core::SessionEvent::MessageReceived {
            agent_id: "main".to_string(),
            role: role.to_string(),
            content_summary: summary.to_string(),
        });
}

/// Assert that no text frame arrives within a short window.
async fn assert_silent(socket: &mut TestSocket) {
    let silence = tokio::time::timeout(Duration::from_millis(300), socket.next()).await;
    assert!(
        matches!(silence, Ok(None)) || silence.is_err(),
        "expected no message, got: {silence:?}"
    );
}

#[tokio::test]
async fn should_receive_server_hello_when_connected() {
    // Given a running server
    let server = TestServer::start().await;
    let mut socket = connect_ws(&server).await;

    // When a client connects to the WebSocket endpoint
    let hello = next_message(&mut socket).await;

    // Then the server greets it with server_hello carrying its identity
    assert_eq!(hello["type"], "server_hello");
    assert_eq!(hello["serverId"], server.state.server_id);
    assert_eq!(hello["version"], "0.1.0-test");
}

#[tokio::test]
async fn should_receive_subscribed_ack_when_subscribing_to_session() {
    // Given a running server with a session and a handshaken connection
    let server = TestServer::start().await;
    let client = reqwest::Client::new();
    let session_id = create_session(&server, &client, "Subscribe").await;
    let mut socket = connected_socket(&server).await;

    // When the client subscribes to the session
    let ack = subscribe(&mut socket, &session_id).await;

    // Then the server confirms the subscription
    assert_eq!(ack["type"], "subscribed");
    assert_eq!(ack["sessionId"], session_id);
}

#[tokio::test]
async fn should_receive_session_event_when_event_published() {
    // Given a subscribed connection to a session
    let server = TestServer::start().await;
    let client = reqwest::Client::new();
    let session_id = create_session(&server, &client, "Events").await;
    let mut socket = connected_socket(&server).await;
    let ack = subscribe(&mut socket, &session_id).await;
    assert_eq!(ack["type"], "subscribed");

    // When an event is published on the session's bus
    publish_message(&server, &session_id, "assistant", "hello from the agent");

    // Then the connection receives it wrapped in a session_event frame
    let frame = next_message(&mut socket).await;
    assert_eq!(frame["type"], "session_event");
    assert_eq!(frame["sessionId"], session_id);
    assert_eq!(frame["event"]["type"], "message_received");
    assert_eq!(frame["event"]["content_summary"], "hello from the agent");
}

#[tokio::test]
async fn should_receive_pong_when_pinging() {
    // Given a handshaken connection
    let server = TestServer::start().await;
    let mut socket = connected_socket(&server).await;

    // When the client sends a plain ping
    socket
        .send(Message::Text(r#"{"type":"ping"}"#.into()))
        .await
        .expect("send ping");

    // Then the server answers with a pong
    let pong = next_message(&mut socket).await;
    assert_eq!(pong["type"], "pong");

    // And a ping carrying a nonce gets it echoed back
    socket
        .send(Message::Text(r#"{"type":"ping","nonce":"n-42"}"#.into()))
        .await
        .expect("send ping with nonce");
    let pong = next_message(&mut socket).await;
    assert_eq!(pong["type"], "pong");
    assert_eq!(pong["nonce"], "n-42");
}

#[tokio::test]
async fn should_stop_receiving_events_when_unsubscribing() {
    // Given a subscribed connection to a session
    let server = TestServer::start().await;
    let client = reqwest::Client::new();
    let session_id = create_session(&server, &client, "Unsubscribe").await;
    let mut socket = connected_socket(&server).await;
    let ack = subscribe(&mut socket, &session_id).await;
    assert_eq!(ack["type"], "subscribed");

    // When the client unsubscribes from the session
    socket
        .send(Message::Text(
            json!({ "type": "unsubscribe", "sessionId": session_id })
                .to_string()
                .into(),
        ))
        .await
        .expect("send unsubscribe");
    let ack = next_message(&mut socket).await;
    assert_eq!(ack["type"], "unsubscribed");
    assert_eq!(ack["sessionId"], session_id);

    // Then events published afterwards are no longer delivered
    publish_message(&server, &session_id, "assistant", "too late");
    assert_silent(&mut socket).await;
}

#[tokio::test]
async fn should_receive_session_closed_event_when_session_closes() {
    // Given a subscribed connection to a session (the artifacts handle is
    // kept so the test can publish after the manager dropped its own)
    let server = TestServer::start().await;
    let client = reqwest::Client::new();
    let session_id = create_session(&server, &client, "Close").await;
    let artifacts = server
        .state
        .session_manager
        .session_artifacts(&mirri_core::SessionId::new(session_id.clone()))
        .expect("session artifacts");
    let mut socket = connected_socket(&server).await;
    let ack = subscribe(&mut socket, &session_id).await;
    assert_eq!(ack["type"], "subscribed");

    // When the session is closed through the HTTP API
    let close_status = client
        .delete(format!(
            "{}/api/v1/sessions/{session_id}",
            server.base_url()
        ))
        .send()
        .await
        .expect("close session");
    assert_eq!(close_status.status(), reqwest::StatusCode::OK);

    // Then the connection receives the session_closed event first
    let frame = next_message(&mut socket).await;
    assert_eq!(frame["type"], "session_event");
    assert_eq!(frame["sessionId"], session_id);
    assert_eq!(frame["event"]["type"], "session_closed");

    // And the connection stays open (multiplexed connections outlive
    // sessions): a ping is still answered
    socket
        .send(Message::Text(r#"{"type":"ping"}"#.into()))
        .await
        .expect("send ping");
    let pong = next_message(&mut socket).await;
    assert_eq!(pong["type"], "pong");

    // And events published on the closed session's bus are no longer
    // delivered (the subscription was dropped)
    artifacts
        .event_bus
        .publish(mirri_core::SessionEvent::MessageReceived {
            agent_id: "main".to_string(),
            role: "assistant".to_string(),
            content_summary: "after close".to_string(),
        });
    assert_silent(&mut socket).await;
}

#[tokio::test]
async fn should_receive_interrupt_ack_when_sending_interrupt() {
    // Given a handshaken connection (interrupt does not require a
    // subscription)
    let server = TestServer::start().await;
    let client = reqwest::Client::new();
    let session_id = create_session(&server, &client, "Interrupt").await;
    let mut socket = connected_socket(&server).await;

    // When the client sends an interrupt for the session
    socket
        .send(Message::Text(
            json!({ "type": "interrupt", "sessionId": session_id })
                .to_string()
                .into(),
        ))
        .await
        .expect("send interrupt");

    // Then the server acknowledges it, echoing the session id
    let ack = next_message(&mut socket).await;
    assert_eq!(ack["type"], "interrupt_ack");
    assert_eq!(ack["sessionId"], session_id);
}

#[tokio::test]
async fn should_receive_error_when_subscribing_to_missing_session() {
    // Given a handshaken connection to a server without sessions
    let server = TestServer::start().await;
    let mut socket = connected_socket(&server).await;

    // When the client subscribes to a session that does not exist
    let frame = subscribe(&mut socket, "session-00000000-0000-0000-0000-000000000000").await;

    // Then the server answers with an error frame instead of subscribing
    assert_eq!(frame["type"], "error");
    assert!(
        frame["message"]
            .as_str()
            .is_some_and(|message| message.contains("not found")),
        "error names the missing session, got: {frame}"
    );
}

#[tokio::test]
async fn should_stay_silent_when_event_published_on_other_sessions_bus() {
    // Given two sessions and a connection subscribed to the first
    let server = TestServer::start().await;
    let client = reqwest::Client::new();
    let first_id = create_session(&server, &client, "First").await;
    let second_id = create_session(&server, &client, "Second").await;
    let mut socket = connected_socket(&server).await;
    let ack = subscribe(&mut socket, &first_id).await;
    assert_eq!(ack["type"], "subscribed");

    // When an event is published on the *second* session's bus
    publish_message(&server, &second_id, "user", "event from the wrong session");

    // Then the connection receives nothing
    assert_silent(&mut socket).await;

    // And it still receives events published on its own session's bus
    publish_message(
        &server,
        &first_id,
        "assistant",
        "event from the right session",
    );
    let frame = next_message(&mut socket).await;
    assert_eq!(frame["type"], "session_event");
    assert_eq!(frame["sessionId"], first_id);
    assert_eq!(frame["event"]["type"], "message_received");
    assert_eq!(
        frame["event"]["content_summary"],
        "event from the right session"
    );
}

#[tokio::test]
async fn should_attribute_events_when_subscribed_to_multiple_sessions() {
    // Given two sessions and a connection subscribed to both
    let server = TestServer::start().await;
    let client = reqwest::Client::new();
    let first_id = create_session(&server, &client, "First").await;
    let second_id = create_session(&server, &client, "Second").await;
    let mut socket = connected_socket(&server).await;
    for session_id in [&first_id, &second_id] {
        let ack = subscribe(&mut socket, session_id).await;
        assert_eq!(ack["type"], "subscribed");
        assert_eq!(ack["sessionId"].as_str(), Some(session_id.as_str()));
    }

    // When one event is published on each session's bus
    publish_message(&server, &first_id, "user", "event on the first session");
    publish_message(&server, &second_id, "user", "event on the second session");

    // Then both events arrive, each attributed to its own session
    let mut received: Vec<(String, String)> = Vec::new();
    for _ in 0..2 {
        let frame = next_message(&mut socket).await;
        assert_eq!(frame["type"], "session_event");
        received.push((
            frame["sessionId"].as_str().expect("sessionId").to_string(),
            frame["event"]["content_summary"]
                .as_str()
                .expect("summary")
                .to_string(),
        ));
    }
    received.sort();
    let mut expected = vec![
        (first_id, "event on the first session".to_string()),
        (second_id, "event on the second session".to_string()),
    ];
    expected.sort();
    assert_eq!(received, expected);
}

}

