//! Self-tests for `mirri-test-harness`: every test boots a real
//! `mirri-server` (see [`common::TestServer`]) and drives it through
//! [`HttpHarness`], exercising the same black-box surface a cross-backend
//! test suite would. Naming follows the Given-When-Then convention.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::TestServer;
use mirri_core::{SessionEvent, SessionId, uuid_from_id};
use mirri_test_harness::{HarnessError, HttpHarness, MirriHarness, MockLlm, MockLlmFactory};
use uuid::Uuid;

/// How long an event is allowed to travel from the server to the harness.
const EVENT_TIMEOUT: Duration = Duration::from_secs(5);

// ===========================================================================
// Infra: health & meta
// ===========================================================================

#[tokio::test]
async fn should_report_healthy_when_health_checked() {
    // Given a running server
    let server = TestServer::start().await;
    let harness = HttpHarness::new(server.base_url());

    // When the harness checks health
    let health = harness.health().await.expect("health probe succeeds");

    // Then the server reports alive and ready
    assert!(health.ok, "health reports ok");
    assert!(health.ready, "health reports ready");
}

#[tokio::test]
async fn should_report_server_identity_when_meta_fetched() {
    // Given a running server
    let server = TestServer::start().await;
    let harness = HttpHarness::new(server.base_url());

    // When the harness fetches server metadata
    let meta = harness.meta().await.expect("meta probe succeeds");

    // Then it reports the server version, a stable server id, and a start time
    assert_eq!(meta.server_version, "0.1.0-test");
    assert!(!meta.server_id.is_empty(), "server id is present");
    assert!(!meta.started_at.is_empty(), "started at is present");
}

// ===========================================================================
// Session lifecycle
// ===========================================================================

#[tokio::test]
async fn should_complete_lifecycle_when_session_created_fetched_and_closed() {
    // Given a running server and a harness
    let server = TestServer::start().await;
    let harness = HttpHarness::new(server.base_url());

    // When a session is created, fetched, listed, and closed
    let session_id = harness
        .create_session("Harness lifecycle")
        .await
        .expect("create session");

    let details = harness.get_session(&session_id).await.expect("get session");
    assert_eq!(details.id, session_id);
    assert_eq!(details.title, "Harness lifecycle");
    assert_eq!(details.agent_count, 1, "the main agent is provisioned");
    assert_eq!(
        details
            .agents
            .iter()
            .map(|agent| agent.id.as_str())
            .collect::<Vec<_>>(),
        vec!["main"]
    );

    let sessions = harness.list_sessions().await.expect("list sessions");
    assert!(
        sessions.iter().any(|s| s.id == session_id),
        "created session appears in the list"
    );
    assert_eq!(
        sessions
            .iter()
            .find(|s| s.id == session_id)
            .expect("session in list")
            .title,
        "Harness lifecycle"
    );

    harness
        .close_session(&session_id)
        .await
        .expect("close session");

    // Then the closed session is gone
    let error = harness
        .get_session(&session_id)
        .await
        .expect_err("closed session is not found");
    assert!(
        matches!(error, HarnessError::NotFound(_)),
        "expected NotFound, got {error:?}"
    );
}

#[tokio::test]
async fn should_report_not_found_when_unknown_session_fetched() {
    // Given a running server and a harness
    let server = TestServer::start().await;
    let harness = HttpHarness::new(server.base_url());

    // When an unknown session is addressed
    let error = harness
        .get_session("session-does-not-exist")
        .await
        .expect_err("unknown session is not found");

    // Then the harness surfaces a NotFound error
    assert!(
        matches!(error, HarnessError::NotFound(_)),
        "expected NotFound, got {error:?}"
    );
}

// ===========================================================================
// Tools
// ===========================================================================

#[tokio::test]
async fn should_list_echo_tool_when_registered() {
    // Given a session whose eagerly provisioned main agent has the echo
    // tool registered (`tool.list` is engine-domain now and addresses one
    // agent — the session's first — when no agentId is given, so the echo
    // tool is registered on the main agent it lists)
    let server = TestServer::start().await;
    let harness = HttpHarness::new(server.base_url());
    let session_id = harness
        .create_session("Echo tool")
        .await
        .expect("create session");
    server.register_echo_tool(&session_id, "main").await;

    // When the tools are listed
    let tools = harness.list_tools(&session_id).await.expect("list tools");

    // Then the echo tool is listed alongside the main agent's builtin tools,
    // with its metadata
    let echo = tools
        .iter()
        .find(|tool| tool.name == "Echo")
        .expect("the registered echo tool is listed");
    assert!(!echo.description.is_empty());
    assert!(!echo.deferred);
    // Registering the echo tool replaces the agent's ACTIVE tool set (the
    // engine's `tool.list` serves the addressed agent's loop tools), so the
    // builtins are no longer expected here.
}

// ===========================================================================
// Engine turns (message.send with a mock LLM)
// ===========================================================================

#[tokio::test]
async fn should_complete_turn_with_end_turn_when_message_sent() {
    // Given a server whose main agent is served by a scripted mock LLM
    let mock = Arc::new(MockLlm::new());
    let factory = Arc::new(MockLlmFactory::new(Arc::clone(&mock)));
    let server = TestServer::start_with_llm_factory(Some(factory)).await;
    let harness = HttpHarness::new(server.base_url());
    let session_id = harness
        .create_session("Mock LLM turn")
        .await
        .expect("create session");

    // When a message is sent (and the mock replays its scripted answer)
    mock.push_text_response("Hello from the mock!");
    let result = harness
        .send_message(&session_id, "Hi there")
        .await
        .expect("send message runs a turn");

    // Then the turn completed with an end_turn stop reason
    assert_eq!(result.stop_reason, "end_turn");
    assert!(result.turn_id >= 1, "turn id is allocated");
    assert!(result.steps >= 1, "at least one step ran");
    assert_eq!(mock.chat_call_count(), 1, "the mock was consulted once");
}

#[tokio::test]
async fn should_deliver_assistant_text_to_memory_when_message_sent() {
    // Given a server with a mock LLM and a session (whose eagerly
    // provisioned main agent the turn runs on)
    let mock = Arc::new(MockLlm::new());
    let factory = Arc::new(MockLlmFactory::new(Arc::clone(&mock)));
    let server = TestServer::start_with_llm_factory(Some(factory)).await;
    let harness = HttpHarness::new(server.base_url());
    let session_id = harness
        .create_session("Mock text")
        .await
        .expect("create session");

    // When a message is sent and the mock answers with text
    mock.push_text_response("The answer is 42.");
    let result = harness
        .send_message(&session_id, "What is the answer?")
        .await
        .expect("send message runs a turn");
    assert_eq!(result.stop_reason, "end_turn");

    // Then the assistant text lands in the main agent's context memory
    let session = server
        .state
        .session_manager
        .get_session(&SessionId::new(&session_id))
        .await
        .expect("session is live");
    let agent = session
        .get_agent(&mirri_core::AgentId::new("main"))
        .await
        .expect("agent is live");
    let messages = agent
        .context_memory()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .messages();
    let assistant = messages
        .iter()
        .find(|message| message.role == mirri_core::Role::Assistant)
        .expect("assistant message recorded");
    let assistant_texts: Vec<&str> = assistant
        .content
        .iter()
        .filter_map(|part| match part {
            mirri_core::ContentPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        assistant_texts
            .iter()
            .any(|text| text.contains("The answer is 42.")),
        "assistant text from the mock turn is in memory, got {assistant_texts:?}"
    );
}

#[tokio::test]
async fn should_report_api_error_when_message_sent() {
    // Given a server without an LLM provider factory, and a session whose
    // main agent is eagerly provisioned by session.create
    let server = TestServer::start().await;
    let harness = HttpHarness::new(server.base_url());
    let session_id = harness
        .create_session("No provider")
        .await
        .expect("create session");

    // When a message is sent and no provider is configured, the harness
    // surfaces the server's API error
    let error = harness
        .send_message(&session_id, "Hi")
        .await
        .expect_err("no provider configured");
    assert!(
        matches!(error, HarnessError::Api { code: -32603, .. }),
        "expected internal API error, got {error:?}"
    );

    // And the same holds for a separately created named agent
    server.create_agent(&session_id, "agent-test").await;
    let error = harness
        .send_message(&session_id, "Hi")
        .await
        .expect_err("no provider configured");
    assert!(
        matches!(error, HarnessError::Api { code: -32603, .. }),
        "expected internal API error, got {error:?}"
    );
}

// ===========================================================================
// Event subscription over WebSocket
// ===========================================================================

#[tokio::test]
async fn should_observe_event_when_event_published() {
    // Given a session the harness is subscribed to
    let server = TestServer::start().await;
    let harness = HttpHarness::new(server.base_url());
    let session_id = harness
        .create_session("Subscribed")
        .await
        .expect("create session");
    let mut receiver = harness
        .subscribe(&session_id)
        .await
        .expect("subscribe to session");

    // When an event is published on the session's bus
    let agent_id = "agent-main".to_string();
    let artifacts = server
        .state
        .session_manager
        .session_artifacts(&SessionId::new(&session_id))
        .expect("session artifacts");
    artifacts.event_bus.publish(SessionEvent::MessageReceived {
        agent_id: agent_id.clone(),
        role: "user".to_string(),
        content_summary: "hello harness".to_string(),
    });

    // Then the harness receiver delivers the same event
    let event = tokio::time::timeout(EVENT_TIMEOUT, receiver.recv())
        .await
        .expect("event arrives within timeout")
        .expect("event received");
    match event {
        SessionEvent::MessageReceived {
            agent_id: got_agent,
            role,
            content_summary,
        } => {
            assert_eq!(got_agent, agent_id);
            assert_eq!(role, "user");
            assert_eq!(content_summary, "hello harness");
        }
        other => panic!("expected MessageReceived, got {other:?}"),
    }
}

#[tokio::test]
async fn should_report_not_found_when_subscribed() {
    // Given a running server and a harness
    let server = TestServer::start().await;
    let harness = HttpHarness::new(server.base_url());

    // When subscribing to a session that does not exist
    let error = harness
        .subscribe("session-does-not-exist")
        .await
        .expect_err("subscribe to unknown session fails");

    // Then the harness surfaces a NotFound error
    assert!(
        matches!(error, HarnessError::NotFound(_)),
        "expected NotFound, got {error:?}"
    );
}

// ===========================================================================
// Approvals & questions (gate round trips)
// ===========================================================================

#[tokio::test]
async fn should_approve_when_resolved_via_harness() {
    // Given a session with a pending tool-approval request
    let server = TestServer::start().await;
    let harness = HttpHarness::new(server.base_url());
    let session_id = harness
        .create_session("Approval")
        .await
        .expect("create session");
    let artifacts = server
        .state
        .session_manager
        .session_artifacts(&SessionId::new(&session_id))
        .expect("session artifacts");
    let mut bus = artifacts.event_bus.subscribe();
    let gate = artifacts.approval_gate.clone();
    let approval_task = tokio::spawn(async move { gate.request("Echo", "echo hi").await });

    let event = tokio::time::timeout(EVENT_TIMEOUT, bus.recv())
        .await
        .expect("approval request event arrives")
        .expect("event received");
    let SessionEvent::ToolApprovalRequested {
        request_id,
        tool_name,
        ..
    } = event
    else {
        panic!("expected ToolApprovalRequested, got {event:?}");
    };
    assert_eq!(tool_name, "Echo");
    assert_ne!(request_id, Uuid::nil());

    // When the harness resolves the request as approved
    harness
        .resolve_approval(&session_id, &request_id.to_string(), true)
        .await
        .expect("resolve approval");

    // Then the awaiting gate observes the approved decision
    let decision = approval_task.await.expect("approval task completes");
    assert!(
        matches!(decision, mirri_core::ApprovalDecision::Approved),
        "expected Approved, got {decision:?}"
    );
}

#[tokio::test]
async fn should_return_answer_when_answered_via_harness() {
    // Given a session with a pending user question
    let server = TestServer::start().await;
    let harness = HttpHarness::new(server.base_url());
    let session_id = harness
        .create_session("Question")
        .await
        .expect("create session");
    let artifacts = server
        .state
        .session_manager
        .session_artifacts(&SessionId::new(&session_id))
        .expect("session artifacts");
    let mut bus = artifacts.event_bus.subscribe();
    let gate = artifacts.question_gate.clone();
    let question_task =
        tokio::spawn(async move { gate.ask("What is the answer to everything?").await });

    let event = tokio::time::timeout(EVENT_TIMEOUT, bus.recv())
        .await
        .expect("question event arrives")
        .expect("event received");
    let SessionEvent::QuestionAsked {
        request_id,
        question,
        ..
    } = event
    else {
        panic!("expected QuestionAsked, got {event:?}");
    };
    assert_eq!(question, "What is the answer to everything?");
    assert_ne!(request_id, Uuid::nil());

    // When the harness answers the question
    harness
        .answer_question(&session_id, &request_id.to_string(), "42")
        .await
        .expect("answer question");

    // Then the awaiting gate receives the answer
    let answer = question_task
        .await
        .expect("question task completes")
        .expect("question was answered, not drained");
    assert_eq!(answer, "42");
}

// ===========================================================================
// Authentication
// ===========================================================================

#[tokio::test]
async fn should_succeed_when_harness_authenticates() {
    // Given a server requiring a bearer token
    let server = TestServer::start_with_auth("s3cret-token").await;

    // When the harness presents the token
    let harness = HttpHarness::with_auth(server.base_url(), "s3cret-token".to_string());
    let session_id = harness
        .create_session("Authed session")
        .await
        .expect("authenticated create session");

    // Then every request succeeds — and the public probes need no token
    let details = harness
        .get_session(&session_id)
        .await
        .expect("authenticated get session");
    assert_eq!(details.id, session_id);
    let health = harness.health().await.expect("public health probe");
    assert!(health.ok);
    let meta = harness.meta().await.expect("public meta probe");
    assert_eq!(meta.server_version, "0.1.0-test");

    // And a harness without the token is rejected with the auth API error
    let anonymous = HttpHarness::new(server.base_url());
    let error = anonymous
        .create_session("Rejected")
        .await
        .expect_err("anonymous request is rejected");
    assert!(
        matches!(error, HarnessError::Api { code: -32002, .. }),
        "expected unauthorized API error, got {error:?}"
    );
}

// ===========================================================================
// Misc: uuid_from_id sanity (session ids used by events)
// ===========================================================================

#[test]
fn should_round_trip_when_extracting_uuid_from_session_id() {
    // Given a session id in the server's `<kind>-<uuid>` convention
    let uuid = Uuid::new_v4();
    let session_id = format!("session-{uuid}");

    // When the embedded uuid is extracted
    let extracted = uuid_from_id(&session_id);

    // Then it matches the original uuid
    assert_eq!(extracted, uuid);
}
