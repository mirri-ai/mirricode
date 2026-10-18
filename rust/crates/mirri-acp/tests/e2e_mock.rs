//! End-to-end mock-LLM test for the ACP handler.
//!
//! Instead of spawning the `mirri-acp` binary, this test drives the library
//! target directly: it builds an [`EmbeddedServer`] on a throwaway workspace
//! with a [`MockLlmFactory`], wraps it in an [`AcpHandler`], and exercises
//! the full ACP flow — initialize → session/new → session/prompt — asserting
//! both the response values and the streaming `session/update`
//! notifications. Because the [`Arc<MockLlm>`] is shared with the factory,
//! the test can also assert provider call counts.

use std::sync::Arc;
use std::time::Duration;

use mirri_acp::AcpHandler;
use mirri_core::{ServerConfig, SessionManager, WorkspaceId, WorkspaceScope, WorkspaceScopeConfig};
use mirri_embedded::EmbeddedServer;
use mirri_test_harness::{MockLlm, MockLlmFactory};
use serde_json::{Value, json};
use tokio::time::timeout;

/// The canned assistant reply the mock LLM is scripted to produce.
const ASSISTANT_REPLY: &str = "Hello from the mock assistant";

/// Deadline for one request/response round trip or a notification batch.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

/// Build an ACP handler on a throwaway workspace with a scripted mock LLM,
/// returning the handler and the shared mock for assertions.
fn handler_with_mock_llm() -> (AcpHandler, Arc<MockLlm>) {
    let tmp = tempfile::tempdir().expect("tempdir must be creatable");
    let workspace = Arc::new(WorkspaceScope::new(
        WorkspaceId::new("acp-e2e"),
        tmp.path().to_path_buf(),
        WorkspaceScopeConfig::default(),
    ));
    let manager = Arc::new(SessionManager::new(workspace));
    let mock = Arc::new(MockLlm::new());
    mock.push_text_response(ASSISTANT_REPLY);
    let factory = Arc::new(MockLlmFactory::new(Arc::clone(&mock)));
    // The embedded create chain requires a resolvable model (spec 11 R4 —
    // no built-in fallback), so this in-process engine needs a
    // `default_model` configured, exactly like a real user's config.toml.
    let config = ServerConfig {
        default_model: Some("e2e-default-model".to_string()),
        ..ServerConfig::default()
    };
    let engine = Arc::new(EmbeddedServer::with_providers(
        manager,
        Some(factory),
        None,
        None,
        None,
        Arc::new(std::sync::RwLock::new(config)),
        None,
    ));
    (AcpHandler::with_engine(engine), mock)
}

/// Drain `session/update` notifications until the accumulated
/// `agent_message_chunk` text contains `needle`, or fail after
/// [`RESPONSE_TIMEOUT`].
async fn wait_for_chunked_text(
    updates: &mut tokio::sync::broadcast::Receiver<mirri_transport::frame::JsonRpcNotification>,
    session_id: &str,
    needle: &str,
) -> String {
    let deadline = tokio::time::Instant::now() + RESPONSE_TIMEOUT;
    let mut chunked_text = String::new();
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for assistant text {needle:?}; got {chunked_text:?}"
        );
        let update = timeout(RESPONSE_TIMEOUT, updates.recv())
            .await
            .expect("timed out waiting for a session/update notification")
            .expect("update channel closed unexpectedly");
        assert_eq!(update.method, "session/update");
        assert_eq!(
            update.params["sessionId"].as_str(),
            Some(session_id),
            "every update must address the prompted session"
        );
        if update.params["update"]["sessionUpdate"].as_str() == Some("agent_message_chunk") {
            let text = update.params["update"]["content"]["text"]
                .as_str()
                .expect("agent_message_chunk carries a text ContentBlock");
            chunked_text.push_str(text);
            if chunked_text.contains(needle) {
                return chunked_text;
            }
        }
    }
}

#[tokio::test]
async fn should_stream_agent_message_chunks_and_end_turn_when_prompt_sent_with_mock_llm() {
    // Given an ACP handler with a scripted mock LLM
    let (handler, mock) = handler_with_mock_llm();
    let mut updates = handler.subscribe_updates();

    // When the client initializes, creates a session, and prompts it
    let result = handler
        .handle("initialize", json!({"protocolVersion": 1}))
        .await
        .expect("initialize must succeed");
    assert_eq!(result["protocolVersion"], 1);

    let result = handler
        .handle("session/new", json!({"title": "Mock E2E"}))
        .await
        .expect("session/new must succeed");
    let session_id = result["sessionId"]
        .as_str()
        .expect("session/new returns a sessionId")
        .to_string();
    assert!(
        session_id.starts_with("session-"),
        "sessionId must be a real session id: {session_id}"
    );

    let result = handler
        .handle(
            "session/prompt",
            json!({
                "sessionId": session_id,
                "prompt": [{"type": "text", "text": "Say hello"}],
            }),
        )
        .await
        .expect("session/prompt must succeed");

    // Then the prompt response reports end_turn
    assert_eq!(
        result["stopReason"], "end_turn",
        "the wait-mode turn must complete with end_turn"
    );

    // And the assistant reply streamed through session/update notifications
    let chunked_text = wait_for_chunked_text(&mut updates, &session_id, ASSISTANT_REPLY).await;
    assert_eq!(chunked_text, ASSISTANT_REPLY);

    // And the mock LLM was consulted exactly once
    assert_eq!(
        mock.chat_call_count(),
        1,
        "one prompt must drive one chat call"
    );
}

#[tokio::test]
async fn should_list_created_sessions_when_session_list_called() {
    // Given a handler with a scripted mock LLM
    let (handler, _mock) = handler_with_mock_llm();
    handler
        .handle("initialize", json!({"protocolVersion": 1}))
        .await
        .expect("initialize must succeed");
    let created = handler
        .handle("session/new", json!({"title": "Listed"}))
        .await
        .expect("session/new must succeed");
    let session_id = created["sessionId"].as_str().unwrap();

    // When sessions are listed
    let result = handler
        .handle("session/list", json!({}))
        .await
        .expect("session/list must succeed");

    // Then the created session appears with its title
    let sessions = result["sessions"]
        .as_array()
        .expect("session/list returns a sessions array");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0]["sessionId"], session_id);
    assert_eq!(sessions[0]["title"], "Listed");
}

#[tokio::test]
async fn should_return_session_not_found_when_prompting_unknown_session() {
    // Given a handler with a scripted mock LLM
    let (handler, mock) = handler_with_mock_llm();
    handler
        .handle("initialize", json!({"protocolVersion": 1}))
        .await
        .expect("initialize must succeed");

    // When a prompt addresses an unknown session
    let (code, message) = handler
        .handle(
            "session/prompt",
            json!({"sessionId": "session-nope", "prompt": "hi"}),
        )
        .await
        .expect_err("unknown session must fail");

    // Then the ACP session-not-found error is returned
    assert_eq!(code, -32002);
    assert!(message.contains("session-nope"));
    assert_eq!(
        mock.chat_call_count(),
        0,
        "no LLM turn may run for a missing session"
    );
}

#[tokio::test]
async fn should_return_method_not_found_when_event_requested() {
    // Given a handler with a scripted mock LLM
    let (handler, _mock) = handler_with_mock_llm();
    handler
        .handle("initialize", json!({"protocolVersion": 1}))
        .await
        .expect("initialize must succeed");

    // When a not-yet-wired ACP method is requested
    let (code, message) = handler
        .handle("session/event", json!({"sessionId": "session-1"}))
        .await
        .expect_err("session/event is not wired yet");

    // Then the handler reports it as not implemented, not a missing session
    assert_eq!(code, -32601);
    assert!(message.contains("session/event"));
}

#[tokio::test]
async fn should_include_protocol_version_and_session_id_when_prompt_response_has_value() {
    // Given a handler with a scripted mock LLM
    let (handler, _mock) = handler_with_mock_llm();

    // When the full flow runs with values in the params
    let result: Value = handler
        .handle("initialize", json!({"protocolVersion": 2}))
        .await
        .expect("initialize must succeed");
    assert_eq!(
        result["protocolVersion"], 1,
        "clamped to the supported version"
    );

    let result = handler
        .handle("session/new", json!({"title": "Values"}))
        .await
        .expect("session/new must succeed");
    let session_id = result["sessionId"].as_str().unwrap().to_string();

    let result = handler
        .handle(
            "session/prompt",
            json!({"sessionId": session_id, "prompt": "plain string prompt"}),
        )
        .await
        .expect("string prompts are accepted");

    // Then the turn completes with end_turn for a bare-string prompt too
    assert_eq!(result["stopReason"], "end_turn");
}
