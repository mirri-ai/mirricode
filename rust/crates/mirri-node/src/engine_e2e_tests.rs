//! End-to-end mock-LLM test for the NAPI-facing [`EngineBridge`].
//!
//! Simulates what the TUI does through the bridge: create a session, send a
//! fire-and-forget message, and consume the streaming session events — all
//! against an embedded engine driven by a scripted [`MockLlm`], with the
//! shared `Arc<MockLlm>` letting the test assert provider call counts.
//!
//! The test lives inline (`#[cfg(test)]`) instead of under `tests/` because
//! integration-test binaries are separate packages and do not receive the
//! N-API link arguments emitted by `napi_build::setup()` (the `_napi_*`
//! symbols would stay undefined at link time).
//!
//! The tests are `#[tokio::test]`: every [`EngineBridge`] public call is
//! async and hops onto the bridge's own Tokio runtime, so the test drives
//! engine work by awaiting directly — no `block_on`, no manual runtime
//! polling.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use mirri_core::llm::LlmToolCall;
use mirri_core::{
    AgentId, SessionId, SessionManager, SessionMetadata, SessionScopeConfig, WorkspaceId,
    WorkspaceScope, WorkspaceScopeConfig,
};
use mirri_embedded::EmbeddedServer;
use mirri_test_harness::{MockLlm, MockLlmFactory};
use mirri_transport::adapter::EngineAdapter as _;
use serde_json::{Value, json};
use tokio::time::timeout;

use crate::EngineBridge;

/// The canned assistant reply the mock LLM is scripted to produce.
const ASSISTANT_REPLY: &str = "Reply from the mock LLM";

/// The engine-side default model seeded into the test boots' config
/// snapshot: the embedded create chain requires a resolvable model (spec 11
/// R4 — no built-in fallback), so a `session.create` without an explicit
/// `modelId` needs SOME default configured, exactly like a real user's
/// config.toml would (the same suite-wide seed the mirri-server wire tests
/// and the cross-backend contract suite in 4cf03e13 carry).
const TEST_DEFAULT_MODEL: &str = "test-default-model";

/// The seeded engine config snapshot for the test boots.
fn test_engine_config() -> Arc<std::sync::RwLock<mirri_core::ServerConfig>> {
    Arc::new(std::sync::RwLock::new(mirri_core::ServerConfig {
        default_model: Some(TEST_DEFAULT_MODEL.to_string()),
        ..mirri_core::ServerConfig::default()
    }))
}

/// Build a bridge on a throwaway workspace with a scripted mock LLM,
/// returning the bridge and the shared mock for assertions.
fn bridge_with_mock_llm() -> (EngineBridge, Arc<MockLlm>) {
    let tmp = tempfile::tempdir().expect("tempdir must be creatable");
    let workspace = Arc::new(WorkspaceScope::new(
        WorkspaceId::new("node-e2e"),
        tmp.path().to_path_buf(),
        WorkspaceScopeConfig::default(),
    ));
    let manager = Arc::new(SessionManager::new(workspace));
    let mock = Arc::new(MockLlm::new());
    mock.push_text_response(ASSISTANT_REPLY);
    let factory = Arc::new(MockLlmFactory::new(Arc::clone(&mock)));
    let engine = Arc::new(EmbeddedServer::with_providers(
        manager,
        Some(factory),
        None,
        None,
        None,
        test_engine_config(),
        None,
    ));
    (EngineBridge::with_engine(engine), mock)
}

/// Build a bridge on a throwaway workspace with an *empty* scripted mock
/// LLM, returning the bridge, the concrete server (for session/agent
/// inspection) and the shared mock. Callers queue the mock's script and
/// drive the bridge exactly like the TUI does.
fn bridge_with_mock_llm_and_server() -> (EngineBridge, Arc<EmbeddedServer>, Arc<MockLlm>) {
    let tmp = tempfile::tempdir().expect("tempdir must be creatable");
    let workspace = Arc::new(WorkspaceScope::new(
        WorkspaceId::new("node-e2e"),
        tmp.path().to_path_buf(),
        WorkspaceScopeConfig::default(),
    ));
    let manager = Arc::new(SessionManager::new(workspace));
    let mock = Arc::new(MockLlm::new());
    let factory = Arc::new(MockLlmFactory::new(Arc::clone(&mock)));
    let server = Arc::new(EmbeddedServer::with_providers(
        manager,
        Some(factory),
        None,
        None,
        None,
        test_engine_config(),
        None,
    ));
    let bridge = EngineBridge::with_engine(server.clone());
    (bridge, server, mock)
}

/// Wire the bridge's reverse-RPC forwarder to a scripted responder that
/// answers every engine request with `respond(method, params)`, recording
/// each request for later assertions.
///
/// The forwarder's callback runs synchronously on a bridge worker, so the
/// answer is delivered through [`EngineBridge::respond_reverse_rpc`] from a
/// task spawned on the bridge runtime — the same async hand-off the NAPI
/// layer performs between its ThreadsafeFunction callback and the bridge.
fn start_scripted_reverse_rpc_responder(
    bridge: &Arc<EngineBridge>,
    respond: impl Fn(&str, &Value) -> Result<Value, String> + Send + Sync + 'static,
) -> Arc<Mutex<Vec<(String, Value)>>> {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let callback = Arc::new(respond);
    let bridge_for_callback = Arc::clone(bridge);
    let requests_for_callback = Arc::clone(&requests);
    bridge.start_reverse_rpc_forwarder(move |request_id, method, params| {
        requests_for_callback
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((method.clone(), params.clone()));
        let callback = Arc::clone(&callback);
        let bridge_for_task = Arc::clone(&bridge_for_callback);
        bridge_for_callback.runtime().spawn(async move {
            let result = callback(&method, &params);
            bridge_for_task
                .respond_reverse_rpc(request_id, result)
                .await;
        });
    });
    requests
}

/// Create a session and return its id, waiting until the engine's
/// reverse-RPC forwarder registration has landed — the session's slot
/// resolves a callback. (`register_builtin_tools` registers the tool for
/// every session agent; the CALL resolves the current slot value, so the
/// registration race this loop once closed over is gone — the wait remains
/// as an ordering guarantee for the question.ask turn that follows.)
///
/// Registration runs as a task on the bridge runtime, so it can in theory
/// lose a race against `session.create`; the registration is enqueued first
/// and normally wins, and this loop closes and recreates the session on the
/// rare miss instead of letting the test flake on a missing callback.
async fn create_session_wired_for_reverse_rpc(
    bridge: &EngineBridge,
    manager: &SessionManager,
    params: Value,
) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let created = bridge
            .call_method("session.create", params.clone())
            .await
            .expect("session.create must succeed");
        let session_id = created["id"]
            .as_str()
            .expect("session.create returns a session id")
            .to_string();
        if session_slot_resolves_callback(manager, &session_id).await {
            return session_id;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for the reverse-RPC forwarder registration to land on the engine"
        );
        let _ = bridge
            .call_method("session.close", json!({ "session_id": session_id }))
            .await;
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Whether the session's main agent was provisioned with the reverse-RPC
/// callback — visible as a resolvable callback in the session's slot.
async fn session_slot_resolves_callback(manager: &SessionManager, session_id: &str) -> bool {
    let Some(session) = manager.get_session(&SessionId::new(session_id)).await else {
        return false;
    };
    session
        .reverse_rpc_slot()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .is_some()
}

/// Drain event notifications until the accumulated assistant delta text
/// equals `expected`, or fail after the deadline.
async fn wait_for_assistant_reply(
    rx: &mut tokio::sync::mpsc::Receiver<mirri_transport::frame::JsonRpcNotification>,
    session_id: &str,
    expected: &str,
) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut chunked_text = String::new();
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for assistant reply {expected:?}; got {chunked_text:?}"
        );
        let notification = timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("timed out waiting for an engine event")
            .expect("event channel closed unexpectedly");
        match notification.method.as_str() {
            "event.message.received" => {
                // The user message is echoed on the bus before the turn runs.
                assert_eq!(notification.params["sessionId"], session_id);
            }
            "event.assistant.delta" => {
                assert_eq!(notification.params["sessionId"], session_id);
                chunked_text.push_str(
                    notification.params["delta"]
                        .as_str()
                        .expect("event.assistant.delta params carry the delta text"),
                );
                if chunked_text == expected {
                    return;
                }
            }
            _ => {}
        }
    }
}

/// Drain event notifications until the accumulated assistant delta text
/// *contains* `expected`, returning the accumulated text; fail after the
/// deadline. Ignores all other event methods.
async fn wait_for_chunk_text_containing(
    rx: &mut tokio::sync::mpsc::Receiver<mirri_transport::frame::JsonRpcNotification>,
    session_id: &str,
    expected: &str,
) -> String {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut chunked_text = String::new();
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for assistant text containing {expected:?}; got {chunked_text:?}"
        );
        let notification = timeout(Duration::from_secs(10), rx.recv())
            .await
            .expect("timed out waiting for an engine event")
            .expect("event channel closed unexpectedly");
        if notification.method == "event.assistant.delta" {
            assert_eq!(notification.params["sessionId"], session_id);
            chunked_text.push_str(
                notification.params["delta"]
                    .as_str()
                    .expect("event.assistant.delta params carry the delta text"),
            );
            if chunked_text.contains(expected) {
                return chunked_text;
            }
        }
    }
}

#[tokio::test]
async fn should_stream_message_received_and_assistant_reply_when_message_sent_through_bridge() {
    // Given a bridge with a scripted mock LLM
    let (bridge, mock) = bridge_with_mock_llm();

    // When the TUI creates a session, subscribes to events, and sends a
    // fire-and-forget message
    let created = bridge
        .call_method("session.create", json!({ "title": "TUI e2e" }))
        .await
        .expect("session.create must succeed");
    let session_id = created["id"]
        .as_str()
        .expect("session.create returns a session id")
        .to_string();
    assert!(session_id.starts_with("session-"));

    let mut events = bridge
        .subscribe_events()
        .await
        .expect("subscribe_events must succeed");

    let sent = bridge
        .call_method(
            "message.send",
            json!({ "session_id": session_id, "text": "Hello TUI", "mode": "fire" }),
        )
        .await
        .expect("message.send must succeed");

    // Then the fire-and-forget response returns immediately with a turn id
    assert_eq!(sent["ok"], true);
    assert_eq!(sent["session_id"], session_id);
    assert!(
        sent["turn_id"]
            .as_str()
            .is_some_and(|id| id.starts_with("turn-")),
        "fire mode must return a turn_id: {sent}"
    );

    // And the event stream delivers the user message echo plus the assistant
    // reply, chunk by chunk
    wait_for_assistant_reply(&mut events, &session_id, ASSISTANT_REPLY).await;

    // And the mock LLM was consulted exactly once
    assert_eq!(
        mock.chat_call_count(),
        1,
        "one message must drive one chat call"
    );

    bridge.dispose().await.expect("dispose must succeed");
}

#[tokio::test]
async fn should_return_wait_mode_turn_result_when_message_sent_through_bridge() {
    // Given a bridge with a scripted mock LLM and a created session (titled
    // — the test counts the mock's calls, and an untitled session's settle
    // would spawn the side title call too)
    let (bridge, mock) = bridge_with_mock_llm();
    let created = bridge
        .call_method("session.create", json!({ "title": "wait turn e2e" }))
        .await
        .expect("session.create must succeed");
    let session_id = created["id"].as_str().unwrap().to_string();

    // When a wait-mode message is sent
    let result = bridge
        .call_method(
            "message.send",
            json!({ "session_id": session_id, "text": "Blocking ask", "mode": "wait" }),
        )
        .await
        .expect("message.send must succeed");

    // Then the full turn result is returned
    assert_eq!(result["turn"]["stopReason"], "end_turn");
    assert_eq!(result["agent_id"], "main");
    assert_eq!(mock.chat_call_count(), 1);

    bridge.dispose().await.unwrap();
}

#[tokio::test]
async fn should_return_method_not_found_when_call_method_unknown() {
    // Given a bridge
    let (bridge, _mock) = bridge_with_mock_llm();

    // When a method outside the 94-method CoreAPI surface is called
    let error = bridge
        .call_method("no.such.method", json!({}))
        .await
        .unwrap_err();

    // Then the error is a method-not-found engine error
    assert!(matches!(
        error,
        mirri_transport::adapter::EngineError::MethodNotFound(ref m) if m == "no.such.method"
    ));

    bridge.dispose().await.unwrap();
}

#[tokio::test]
async fn should_approve_tool_call_when_reverse_rpc_approval_ask_received_and_approved() {
    // Given a bridge with a scripted mock LLM that first requests the Bash
    // tool and then closes the turn with a plain reply
    let (bridge, server, mock) = bridge_with_mock_llm_and_server();
    mock.push_tool_use_response(vec![LlmToolCall {
        id: "call_1".to_string(),
        name: "Bash".to_string(),
        args: json!({ "command": "echo hello" }),
    }]);
    mock.push_text_response("Done");
    let bridge = Arc::new(bridge);

    // And a reverse-RPC responder wired through the bridge that approves
    // every approval request
    let requests = start_scripted_reverse_rpc_responder(&bridge, |method, _params| match method {
        "approval.ask" => Ok(json!({ "decision": "approved" })),
        other => Err(format!("unexpected reverse RPC method {other:?}")),
    });

    // When a Manual-mode session is created — Bash is not default-approved,
    // so the permission gate must Ask — and a fire-and-forget message asks
    // for the shell
    let manager = server.session_manager();
    let session_id = create_session_wired_for_reverse_rpc(
        &bridge,
        &manager,
        json!({ "title": "approval e2e", "permission_mode": "manual" }),
    )
    .await;
    let mut events = bridge
        .subscribe_events()
        .await
        .expect("subscribe_events must succeed");
    bridge
        .call_method(
            "message.send",
            json!({ "session_id": session_id, "text": "run echo", "mode": "fire" }),
        )
        .await
        .expect("message.send must succeed");

    // Then the turn streams the end-turn reply — proof the approved Bash
    // call executed and the loop ran on
    let chunked = wait_for_chunk_text_containing(&mut events, &session_id, "Done").await;
    assert_eq!(chunked, "Done");

    // And the mock LLM was consulted exactly twice: the tool-use turn, then
    // the end-turn reply after the tool result came back
    assert_eq!(
        mock.chat_call_count(),
        2,
        "tool use + end turn must drive two chat calls"
    );

    // And exactly one approval.ask was issued — carrying the Bash tool call —
    // and answered with an approval decision
    {
        let requests = requests.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            requests.len(),
            1,
            "exactly one reverse RPC expected: {requests:?}"
        );
        assert_eq!(requests[0].0, "approval.ask");
        assert_eq!(requests[0].1["tool_name"], "Bash");
        assert_eq!(requests[0].1["tool_call_id"], "call_1");
        assert_eq!(requests[0].1["action"], "execute Bash");
    }

    // And the approved Bash call actually executed: its tool result — the
    // captured `echo hello` output, plain text since ADR-013 — is recorded
    // in the context history (a blocked or rejected call would leave no
    // stdout payload)
    let session = manager
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session stays live");
    let agent = session
        .get_agent(&AgentId::new("main"))
        .await
        .expect("main agent stays live");
    let history = agent
        .context_memory()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .history()
        .to_vec();
    assert!(
        history.iter().any(|entry| {
            entry.message.tool_call_id.as_deref() == Some("call_1")
                && entry.message.text_content().contains("hello")
        }),
        "the approved Bash tool result (echo hello output) must be in the context history: {history:?}"
    );

    bridge.dispose().await.expect("dispose must succeed");
}

#[tokio::test]
async fn should_answer_question_when_reverse_rpc_question_ask_received() {
    // Given a bridge with a scripted mock LLM that first asks a structured
    // question and then closes the turn with a plain reply
    let (bridge, server, mock) = bridge_with_mock_llm_and_server();
    mock.push_tool_use_response(vec![LlmToolCall {
        id: "call_1".to_string(),
        name: "AskUserQuestion".to_string(),
        args: json!({
            "questions": [{
                "question": "Which language?",
                "header": "lang",
                "options": [
                    { "label": "Rust" },
                    { "label": "TypeScript" },
                ],
            }],
        }),
    }]);
    mock.push_text_response("Got it");
    let bridge = Arc::new(bridge);

    // And a reverse-RPC responder wired through the bridge that answers the
    // user question with a scripted selection
    let requests = start_scripted_reverse_rpc_responder(&bridge, |method, _params| match method {
        "question.ask" => Ok(json!({ "answers": { "Which language?": "Rust" } })),
        other => Err(format!("unexpected reverse RPC method {other:?}")),
    });

    // When a Yolo-mode session is created — everything is pre-approved, so
    // only the question round-trip fires — and a fire-and-forget message
    // prompts the agent to ask
    let manager = server.session_manager();
    let session_id = create_session_wired_for_reverse_rpc(
        &bridge,
        &manager,
        json!({ "title": "question e2e", "permission_mode": "yolo" }),
    )
    .await;
    let mut events = bridge
        .subscribe_events()
        .await
        .expect("subscribe_events must succeed");
    bridge
        .call_method(
            "message.send",
            json!({ "session_id": session_id, "text": "ask me a question", "mode": "fire" }),
        )
        .await
        .expect("message.send must succeed");

    // Then the turn streams the end-turn reply — proof the answered question
    // let the loop run on
    let chunked = wait_for_chunk_text_containing(&mut events, &session_id, "Got it").await;
    assert_eq!(chunked, "Got it");

    // And the mock LLM was consulted exactly twice: the tool-use turn, then
    // the end-turn reply after the answer came back
    assert_eq!(
        mock.chat_call_count(),
        2,
        "tool use + end turn must drive two chat calls"
    );

    // And exactly one question.ask was issued — carrying the scripted
    // question — and answered with the user's selection
    {
        let requests = requests.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            requests.len(),
            1,
            "exactly one reverse RPC expected: {requests:?}"
        );
        assert_eq!(requests[0].0, "question.ask");
        let questions = requests[0].1["questions"]
            .as_array()
            .expect("question.ask carries the questions array");
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0]["question"], "Which language?");
    }

    // And the answered question reached the loop: the AskUserQuestion tool
    // result — the pretty-printed `{ "answers": { ... } }` — is recorded in
    // the context history
    let session = manager
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session stays live");
    let agent = session
        .get_agent(&AgentId::new("main"))
        .await
        .expect("main agent stays live");
    let history = agent
        .context_memory()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .history()
        .to_vec();
    assert!(
        history.iter().any(|entry| {
            let text = entry.message.text_content();
            text.contains("\"answers\"") && text.contains("Which language?")
        }),
        "the answered question must be in the context history: {history:?}"
    );

    bridge.dispose().await.expect("dispose must succeed");
}

/// Given a shared Mirri home whose registry carries more workspaces than the
/// NAPI host negotiated (design 08 §2.2's embedded-host pin), when the bridge
/// drives an engine rooted at the negotiated directory, then the negotiated
/// root is the default workspace, every other registry row stays
/// registered-but-lazy (D5), and a default `session.list` through the bridge
/// shows ONLY the pinned root's sessions — another workspace's persisted
/// session is not this host's to see (W9's §2.2 pin, observed through the
/// NAPI-facing `EngineBridge`).
#[tokio::test]
async fn should_pin_the_shared_home_to_the_negotiated_root_when_the_registry_is_non_empty() {
    let home = tempfile::tempdir().expect("home tempdir");
    let pinned_root = tempfile::tempdir().expect("pinned root tempdir");
    let other_root = tempfile::tempdir().expect("other root tempdir");
    let other_key = mirri_core::encode_work_dir_key(other_root.path());
    std::fs::write(
        home.path().join("workspaces.json"),
        serde_json::to_string(&serde_json::json!({
            "version": 1,
            "workspaces": {
                other_key.as_str(): {
                    "root": other_root.path().to_string_lossy(),
                    "name": "other",
                    "created_at": "2024-01-01T00:00:00.000Z",
                    "last_opened_at": "2024-06-01T00:00:00.000Z",
                }
            },
            "deleted_workspace_ids": [],
        }))
        .expect("registry json serializes"),
    )
    .expect("workspaces.json written");

    // And one persisted session per workspace bucket (a seeding pass with the
    // serve-like view — the shared index rows a busy home carries)
    let (pinned_session, other_session) = {
        let seeder = EmbeddedServer::for_workspace_root(
            WorkspaceId::new("node-pin-seed"),
            pinned_root.path().to_path_buf(),
            Some(home.path().to_path_buf()),
            Vec::new(),
        );
        // The visibility knob is irrelevant to this seeding pass: the
        // sessions are created directly on their managers and the reconcile
        // converges them into the shared index regardless.
        let now = chrono::Utc::now();
        let metadata = || SessionMetadata {
            title: "shared home session".to_string(),
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
        };
        let pinned = seeder
            .session_manager()
            .create_session(metadata(), SessionScopeConfig::default())
            .await
            .expect("the pinned bucket's session is created");
        let other_runtime = seeder
            .workspace_registry()
            .ensure_built(&other_key)
            .expect("the other row is registered")
            .expect("the seeding runtime builds");
        let other = other_runtime
            .manager
            .create_session(metadata(), SessionScopeConfig::default())
            .await
            .expect("the other bucket's session is created");
        // The one-time reconcile converges both rows into the shared index.
        seeder
            .call_method("workspace.list", serde_json::json!({}))
            .await
            .expect("the seeding pass's first RPC succeeds");
        (
            pinned.id().as_str().to_string(),
            other.id().as_str().to_string(),
        )
    };

    // When the NAPI host's engine boots on that home (the embedded-host
    // constructor — the §2.2 pin) and the bridge lists with default params
    let server = Arc::new(EmbeddedServer::for_workspace_root(
        WorkspaceId::new("node-pin"),
        pinned_root.path().to_path_buf(),
        Some(home.path().to_path_buf()),
        Vec::new(),
    ));
    let bridge = EngineBridge::with_engine(
        Arc::clone(&server) as Arc<dyn mirri_transport::adapter::EngineAdapter>
    );

    // Then the pinned root is the default and the other row stays lazy (D5:
    // registering never builds; only the default runtime is eager)
    assert_eq!(
        server.workspace_registry().built_count(),
        1,
        "only the pinned default workspace's runtime is built"
    );
    assert_eq!(
        server.workspace_registry().default_workspace_id(),
        Some(mirri_core::encode_work_dir_key(pinned_root.path())),
        "the negotiated root is the default workspace"
    );

    // And the default listing through the bridge shows only the pinned
    // root's session — the other workspace's persisted session stays
    // invisible (an explicit workspace filter crosses the pin, but the
    // default never does).
    let listed = bridge
        .call_method("session.list", serde_json::json!({}))
        .await
        .expect("session.list through the bridge succeeds");
    let ids: Vec<String> = listed["items"]
        .as_array()
        .expect("the page envelope's items")
        .iter()
        .map(|row| row["id"].as_str().expect("the id").to_string())
        .collect();
    assert_eq!(ids, vec![pinned_session.clone()]);
    assert!(
        !ids.contains(&other_session),
        "the other workspace's session is not this host's to see: {ids:?}"
    );

    bridge.dispose().await.expect("dispose must succeed");
}
