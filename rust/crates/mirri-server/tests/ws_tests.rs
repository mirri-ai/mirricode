//! Integration tests for the multiplexed WebSocket endpoint (`/api/v1/ws`).
//!
//! Clients connect with `tokio-tungstenite`, complete the `server_hello` /
//! `client_hello` handshake, then subscribe to session event streams.
//! Session events arrive wrapped in `session_event` frames that carry the
//! id of the session they belong to.

mod common;

use std::time::Duration;

use common::TestServer;
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
