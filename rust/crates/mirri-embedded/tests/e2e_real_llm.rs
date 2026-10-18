//! Real LLM end-to-end tests for the embedded engine.
//!
//! These tests exercise the full agent loop against a live LLM provider.
//! They are `#[ignore]` by default — run with `cargo test -- --ignored`.
//!
//! Required environment variables:
//! - `OPENAI_API_KEY` — API key for the OpenAI-compatible provider
//! - `OPENAI_BASE_URL` — (optional) base URL, defaults to `https://api.openai.com/v1`
//! - `MIRRI_E2E_MODEL` — (optional) model ID, defaults to `gpt-4o-mini`

use std::sync::Arc;

use mirri_core::{SessionManager, WorkspaceId, WorkspaceScope, WorkspaceScopeConfig};
use mirri_embedded::EmbeddedServer;
use mirri_embedded::providers::ProviderLlmFactory;
use mirri_transport::adapter::EngineAdapter;
use serde_json::json;
use tokio::time::timeout;

/// Serializes tests that read process-global environment variables.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Skip the test (return `None`) when no API key is set.
fn require_api_key() -> Option<String> {
    std::env::var("OPENAI_API_KEY").ok()
}

/// Build an [`EmbeddedServer`] backed by [`ProviderLlmFactory`] on a throwaway
/// workspace.
fn make_server() -> EmbeddedServer {
    let tmp = tempfile::tempdir().expect("tempdir must be creatable");
    let workspace = Arc::new(WorkspaceScope::new(
        WorkspaceId::new("e2e-real-llm"),
        tmp.path().to_path_buf(),
        WorkspaceScopeConfig::default(),
    ));
    let manager = Arc::new(SessionManager::new(workspace));
    EmbeddedServer::with_llm_factory(manager, Some(Arc::new(ProviderLlmFactory::default())))
}

#[tokio::test]
#[ignore]
#[allow(clippy::await_holding_lock)] // ENV_LOCK serializes env-var access across the whole test
async fn should_get_text_response_when_sending_simple_message_to_real_llm() {
    let api_key = match require_api_key() {
        Some(k) => k,
        None => {
            eprintln!("Skipping: OPENAI_API_KEY not set");
            return;
        }
    };
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _ = &api_key; // consumed by ProviderLlmFactory via env at LLM-creation time
    let model = std::env::var("MIRRI_E2E_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_string());

    let server = make_server();

    // Create a session with the OpenAI provider and the configured model.
    let create_result = server
        .call_method(
            "session.create",
            json!({
                "title": "E2E Real LLM Test",
                "providerType": "openai",
                "modelId": model,
                "systemPrompt": "You are a helpful assistant. Reply concisely.",
            }),
        )
        .await
        .expect("session.create must succeed");

    let session_id = create_result["id"]
        .as_str()
        .expect("session.create returns a session id")
        .to_string();

    // Send a simple message in wait mode — the turn must complete and report
    // a stop reason.
    let result = server
        .call_method(
            "message.send",
            json!({
                "sessionId": session_id,
                "text": "Reply with exactly: Hello from real LLM",
                "mode": "wait",
            }),
        )
        .await
        .expect("message.send must succeed");

    assert!(
        result["ok"].as_bool().unwrap_or(false),
        "turn should succeed: {result}"
    );
    assert_eq!(
        result["turn"]["stopReason"], "end_turn",
        "stop reason should be end_turn: {result}"
    );

    // Verify the agent's context was populated — the assistant turn produced
    // messages (at least the user message and the assistant reply).
    let context = server
        .call_method("agent.getContext", json!({ "sessionId": session_id }))
        .await
        .expect("agent.getContext must succeed");
    let message_count = context["context"]["messageCount"]
        .as_u64()
        .expect("context should have a messageCount");
    assert!(
        message_count >= 2,
        "context should contain at least the user message and the assistant reply, got {message_count}"
    );
}

#[tokio::test]
#[ignore]
#[allow(clippy::await_holding_lock)] // ENV_LOCK serializes env-var access across the whole test
async fn should_stream_events_when_using_fire_mode_with_real_llm() {
    let api_key = match require_api_key() {
        Some(k) => k,
        None => {
            eprintln!("Skipping: OPENAI_API_KEY not set");
            return;
        }
    };
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _ = &api_key; // consumed by ProviderLlmFactory via env at LLM-creation time
    let model = std::env::var("MIRRI_E2E_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_string());

    let server = make_server();

    let create_result = server
        .call_method(
            "session.create",
            json!({
                "title": "E2E Real LLM Stream Test",
                "providerType": "openai",
                "modelId": model,
                "systemPrompt": "You are a helpful assistant. Reply concisely.",
            }),
        )
        .await
        .expect("session.create must succeed");

    let session_id = create_result["id"]
        .as_str()
        .expect("session.create returns a session id")
        .to_string();

    // Subscribe before sending so no events are lost.
    let mut events = server
        .subscribe_events()
        .await
        .expect("subscribe_events must succeed");

    // Fire mode returns immediately with a turn id.
    let result = server
        .call_method(
            "message.send",
            json!({
                "sessionId": session_id,
                "text": "Say hello in one sentence.",
                "mode": "fire",
            }),
        )
        .await
        .expect("message.send must succeed");

    assert!(
        result["ok"].as_bool().unwrap_or(false),
        "fire mode should accept the message: {result}"
    );

    // Collect events until the turn ends, with an overall deadline.
    let mut got_text = false;
    let mut turn_ended = false;
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(60);
    while tokio::time::Instant::now() < deadline {
        match timeout(tokio::time::Duration::from_secs(15), events.recv()).await {
            Ok(Some(event)) => {
                if event.method == "event.assistant.delta" {
                    got_text = true;
                }
                if event.method == "event.turn.ended" {
                    if event.params["reason"] == "failed" {
                        break;
                    }
                    turn_ended = true;
                    break;
                }
            }
            _ => break,
        }
    }
    assert!(
        got_text,
        "should have received at least one event.assistant.delta notification"
    );
    assert!(turn_ended, "turn should end successfully, not fail");
}
