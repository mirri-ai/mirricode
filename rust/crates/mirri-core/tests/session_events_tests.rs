//! Integration tests for session events + gates (Task 33).
//!
//! Covers: [`EventBus`] fan-out / lag / close semantics, [`SessionEvent`]
//! serde (type tag + unknown-variant fallback), [`ApprovalGate`] and
//! [`QuestionGate`] request/response/drain flows, [`SessionShutdown`], and
//! [`SessionManager`] integration (fresh gates per session, events published
//! for resume/fork/close, drain-on-close).
//!
//! Tests use the Given-When-Then naming convention.

use std::sync::Arc;
use std::time::Duration;

use mirri_core::{
    AgentLifecycle, AgentScopeConfig, AppScope, AppScopeConfig, ApprovalDecision, ApprovalGate,
    BackgroundTaskInfo, CloseReason, CompactionResult, CompactionSource, EventBus, QuestionGate,
    SessionActivityState, SessionArtifacts, SessionEvent, SessionId, SessionManager,
    SessionMetadata, SessionPendingInteraction, SessionScopeConfig, SessionShutdown,
    SessionTurnOutcome, SuspendCause, TaskKind, TurnStopReason, WorkspaceId, WorkspaceScopeConfig,
    event_projection, uuid_from_id,
};
use tokio::sync::broadcast::{self, error::RecvError, error::TryRecvError};
use uuid::Uuid;

// ===========================================================================
// Helpers
// ===========================================================================

struct TestEnv {
    manager: SessionManager,
    _tmp: tempfile::TempDir,
}

impl TestEnv {
    async fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let app = AppScope::new(AppScopeConfig::default());
        let workspace = app
            .create_workspace_with_config(
                WorkspaceId::new("events-test-ws"),
                tmp.path().to_path_buf(),
                WorkspaceScopeConfig::default(),
            )
            .await
            .unwrap();
        let manager = SessionManager::with_persistence_dir(
            workspace,
            tmp.path().join(".mirri").join("sessions"),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        Self { manager, _tmp: tmp }
    }
}

fn make_metadata(title: &str) -> SessionMetadata {
    SessionMetadata {
        title: title.to_string(),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        tags: Default::default(),
        archived: false,
        is_custom_title: false,
        work_dir: None,
        last_prompt: None,
        forked_from: None,
        cache_root_id: None,
        custom: None,
        last_used_at: None,
    }
}

/// Poll a condition until it becomes true or a deadline passes.
async fn wait_until(label: &str, mut condition: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("condition never became true: {label}");
}

/// Receive events until one matches the predicate; panics on timeout/close.
async fn recv_matching(
    receiver: &mut broadcast::Receiver<SessionEvent>,
    label: &str,
    mut predicate: impl FnMut(&SessionEvent) -> bool,
) -> SessionEvent {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match receiver.recv().await {
                Ok(event) => {
                    if predicate(&event) {
                        return event;
                    }
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => {
                    panic!("event channel closed while waiting for {label}")
                }
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for event: {label}"))
}

// ===========================================================================
// EventBus — fan-out
// ===========================================================================

#[tokio::test]
async fn should_all_subscribers_receive_event_when_published_on_event_bus() {
    // Given an event bus with two subscribers
    let bus = Arc::new(EventBus::new(16));
    let mut receiver_a = bus.subscribe();
    let mut receiver_b = bus.subscribe();
    let session_id = Uuid::new_v4();

    // When publishing a SessionClosed event
    bus.publish(SessionEvent::SessionClosed { session_id });

    // Then both subscribers receive it
    for receiver in [&mut receiver_a, &mut receiver_b] {
        match receiver.recv().await.unwrap() {
            SessionEvent::SessionClosed {
                session_id: received_id,
            } => assert_eq!(received_id, session_id),
            other => panic!("expected SessionClosed, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn should_only_receive_future_events_when_subscribing_after_publish() {
    // Given an event bus that already published an event
    let bus = Arc::new(EventBus::new(16));
    bus.publish(SessionEvent::SessionResumed {
        session_id: Uuid::new_v4(),
    });

    // When subscribing afterwards
    let mut receiver = bus.subscribe();

    // Then the old event is not replayed
    assert!(
        matches!(receiver.try_recv(), Err(TryRecvError::Empty)),
        "new subscribers must not see events published before subscribing"
    );

    // And future events are received
    let session_id = Uuid::new_v4();
    bus.publish(SessionEvent::SessionClosed { session_id });
    match receiver.recv().await.unwrap() {
        SessionEvent::SessionClosed {
            session_id: received_id,
        } => assert_eq!(received_id, session_id),
        other => panic!("expected SessionClosed, got {other:?}"),
    }
}

#[tokio::test]
async fn should_return_lagged_error_when_events_exceed_buffer_capacity() {
    // Given a small event bus and a subscriber that does not keep up
    let bus = Arc::new(EventBus::new(2));
    let mut receiver = bus.subscribe();

    // When publishing more events than the buffer can hold
    for _ in 0..5 {
        bus.publish(SessionEvent::SessionClosed {
            session_id: Uuid::new_v4(),
        });
    }

    // Then the subscriber observes a Lagged error (missed events are dropped)
    match receiver.recv().await {
        Err(RecvError::Lagged(skipped)) => assert!(
            skipped >= 1,
            "expected at least one skipped event, got {skipped}"
        ),
        other => panic!("expected Lagged error, got {other:?}"),
    }
}

#[tokio::test]
async fn should_return_closed_error_when_all_event_bus_arcs_dropped() {
    // Given a receiver holding the only subscriber
    let bus = Arc::new(EventBus::new(8));
    let mut receiver = bus.subscribe();

    // When the last Arc<EventBus> is released
    drop(bus);

    // Then the receiver observes the channel closing
    assert!(matches!(receiver.recv().await, Err(RecvError::Closed)));
}

// ===========================================================================
// SessionEvent — serialization
// ===========================================================================

#[tokio::test]
async fn should_have_type_tag_identifying_variant_when_session_event_serialized() {
    // Given a tool approval request event
    let agent_id = Uuid::new_v4();
    let request_id = Uuid::new_v4();
    let event = SessionEvent::ToolApprovalRequested {
        agent_id,
        tool_name: "bash".to_string(),
        request_id,
    };

    // When serializing
    let json = serde_json::to_value(event).unwrap();

    // Then the type tag and fields are present
    assert_eq!(json["type"], "tool_approval_requested");
    assert_eq!(json["tool_name"], "bash");
    assert_eq!(json["agent_id"], agent_id.to_string());
    assert_eq!(json["request_id"], request_id.to_string());
}

#[tokio::test]
async fn should_deserialize_to_unknown_variant_when_wire_type_unrecognized() {
    // When deserializing JSON with an unrecognised type tag
    let event: SessionEvent =
        serde_json::from_str(r#"{"type":"some_future_event","payload":42}"#).unwrap();

    // Then the Unknown fallback variant is produced
    assert!(matches!(event, SessionEvent::Unknown));
}

#[tokio::test]
async fn should_preserve_status_when_agent_status_updated_event_roundtripped() {
    // Given an AgentStatusUpdated event carrying the structured status
    // snapshot (phase, model, context, usage)
    use mirri_core::session::agent_status::AgentPhase;
    use mirri_core::usage::{TokenUsage, UsageStatus};
    use std::collections::HashMap;

    let tokens = TokenUsage {
        input_tokens: 120,
        output_tokens: 30,
        cache_read_tokens: 5,
        cache_creation_tokens: 1,
    };
    let usage = UsageStatus {
        by_model: HashMap::from([("kimi-k2".to_string(), tokens.clone())]),
        current_turn: tokens.clone(),
        total: tokens,
        first_call: None,
    };
    let event = SessionEvent::AgentStatusUpdated {
        agent_id: "main".to_string(),
        phase: AgentPhase::Running,
        model: Some("kimi-k2".to_string()),
        thinking_effort: Some(mirri_core::ThinkingEffort::High),
        context_tokens: Some(120_000),
        max_context_tokens: Some(200_000),
        plan_mode: Some(false),
        swarm_mode: None,
        usage: Some(usage.clone()),
    };

    // When serializing and deserializing
    let json = serde_json::to_string(&event).unwrap();
    let roundtrip: SessionEvent = serde_json::from_str(&json).unwrap();

    // Then the tagged variant and its structured status survive the
    // roundtrip, and the absent optional field is entirely absent from
    // the serialised form (never a null key)
    match roundtrip {
        SessionEvent::AgentStatusUpdated {
            agent_id,
            phase,
            model,
            thinking_effort,
            context_tokens,
            max_context_tokens,
            plan_mode,
            swarm_mode,
            usage: received_usage,
        } => {
            assert_eq!(agent_id, "main");
            assert_eq!(phase, AgentPhase::Running);
            assert_eq!(model.as_deref(), Some("kimi-k2"));
            assert_eq!(thinking_effort, Some(mirri_core::ThinkingEffort::High));
            assert_eq!(context_tokens, Some(120_000));
            assert_eq!(max_context_tokens, Some(200_000));
            assert_eq!(plan_mode, Some(false));
            assert_eq!(swarm_mode, None);
            assert_eq!(received_usage, Some(usage));
        }
        other => panic!("expected AgentStatusUpdated, got {other:?}"),
    }
    assert!(
        !json.contains("swarm_mode") && !json.contains("swarmMode"),
        "an absent optional field must not be serialized at all: {json}"
    );
}

#[test]
fn should_roundtrip_a_session_work_changed_event_through_its_serde_form() {
    // Given a SessionWorkChanged event with a completed-turn state
    let event = SessionEvent::SessionWorkChanged {
        state: SessionActivityState {
            busy: true,
            main_turn_active: false,
            pending_interaction: SessionPendingInteraction::Question,
            last_turn_reason: Some(SessionTurnOutcome::Cancelled),
        },
    };

    // When serializing and deserializing the event itself (the ws relay
    // serialises SessionEvent directly, without going through the projection)
    let json = serde_json::to_value(&event).expect("event serialises");
    let roundtrip: SessionEvent = serde_json::from_value(json.clone()).unwrap();

    // Then the tagged variant and its snake_case activity state survive
    // (the enum's own serde shape uses snake_case, distinct from the
    // projection payload which mirrors it key-for-key)
    match roundtrip {
        SessionEvent::SessionWorkChanged { state } => {
            assert!(state.busy);
            assert!(!state.main_turn_active);
            assert_eq!(
                state.pending_interaction,
                SessionPendingInteraction::Question
            );
            assert_eq!(state.last_turn_reason, Some(SessionTurnOutcome::Cancelled));
        }
        other => panic!("expected SessionWorkChanged, got {other:?}"),
    }
    assert_eq!(json["type"], "session_work_changed");
    assert_eq!(json["state"]["busy"], serde_json::json!(true));
    assert_eq!(
        json["state"]["pending_interaction"],
        serde_json::json!("question")
    );
    assert_eq!(
        json["state"]["last_turn_reason"],
        serde_json::json!("cancelled")
    );
}

/// Given a SessionWorkChanged event whose state predates any completed
/// turn, When it is projected, Then the `last_turn_reason` key stays absent
/// from the payload (the projection strips it — the protocol says the key
/// is missing before any turn, so it never appears as a null placeholder).
#[test]
fn should_omit_last_turn_reason_from_the_payload_when_projecting_a_pre_turn_state() {
    // Given a SessionWorkChanged event with no completed turn yet
    let event = SessionEvent::SessionWorkChanged {
        state: SessionActivityState::default(),
    };

    // When projecting it onto the wire event
    let projected = projected(&event);

    // Then the idle flags are carried and the reason key stays absent
    assert_eq!(projected.method, "event.session.work_changed");
    assert_eq!(projected.payload["busy"], serde_json::json!(false));
    assert_eq!(
        projected.payload["main_turn_active"],
        serde_json::json!(false)
    );
    assert_eq!(
        projected.payload["pending_interaction"],
        serde_json::json!("none")
    );
    assert!(projected.payload.get("last_turn_reason").is_none());
}

// ===========================================================================
// ApprovalGate — auto-approve
// ===========================================================================

#[tokio::test]
async fn should_approve_immediately_when_gate_in_auto_approve_mode() {
    // Given a gate in auto-approve (headless) mode
    let gate = ApprovalGate::new(true);

    // When requesting approval
    let decision = gate.request("bash", "rm -rf /tmp/x").await;

    // Then the request is approved immediately without registering anything
    assert_eq!(decision, ApprovalDecision::Approved);
    assert_eq!(gate.pending_count(), 0);
}

// ===========================================================================
// ApprovalGate — request / resolve
// ===========================================================================

#[tokio::test]
async fn should_block_until_resolved_with_approved_when_request_made() {
    // Given a gate wired to an event bus (the bus carries the request_id)
    let bus = Arc::new(EventBus::new(16));
    let gate = Arc::new(ApprovalGate::with_event_bus(false, Arc::clone(&bus)));
    let mut receiver = bus.subscribe();

    let request_gate = Arc::clone(&gate);
    let handle = tokio::spawn(async move { request_gate.request("bash", "ls -la").await });

    // When the request is published and then resolved as approved
    let event = recv_matching(&mut receiver, "ToolApprovalRequested", |e| {
        matches!(e, SessionEvent::ToolApprovalRequested { .. })
    })
    .await;
    let request_id = match event {
        SessionEvent::ToolApprovalRequested {
            request_id,
            tool_name,
            ..
        } => {
            assert_eq!(tool_name, "bash");
            request_id
        }
        other => panic!("expected ToolApprovalRequested, got {other:?}"),
    };

    // Then the request is pending until resolved
    assert_eq!(gate.pending_count(), 1);
    assert!(gate.resolve(request_id, ApprovalDecision::Approved));
    assert_eq!(gate.pending_count(), 0);

    // And the requester receives the approved decision
    assert_eq!(handle.await.unwrap(), ApprovalDecision::Approved);
}

#[tokio::test]
async fn should_receive_denial_with_reason_when_resolved_as_denied() {
    // Given a pending request
    let bus = Arc::new(EventBus::new(16));
    let gate = Arc::new(ApprovalGate::with_event_bus(false, Arc::clone(&bus)));
    let mut receiver = bus.subscribe();

    let request_gate = Arc::clone(&gate);
    let handle = tokio::spawn(async move { request_gate.request("bash", "ls").await });

    let request_id = match recv_matching(&mut receiver, "ToolApprovalRequested", |e| {
        matches!(e, SessionEvent::ToolApprovalRequested { .. })
    })
    .await
    {
        SessionEvent::ToolApprovalRequested { request_id, .. } => request_id,
        other => panic!("expected ToolApprovalRequested, got {other:?}"),
    };

    // When resolving as denied with a reason
    assert!(gate.resolve(
        request_id,
        ApprovalDecision::Denied("user said no".to_string())
    ));

    // Then the requester receives the denial with its reason
    assert_eq!(
        handle.await.unwrap(),
        ApprovalDecision::Denied("user said no".to_string())
    );
}

#[tokio::test]
async fn should_return_false_when_resolving_unknown_request_id() {
    // Given a gate with no pending requests
    let gate = ApprovalGate::new(false);

    // When resolving a request id that was never registered
    let resolved = gate.resolve(Uuid::new_v4(), ApprovalDecision::Approved);

    // Then resolution reports failure
    assert!(!resolved);
}

#[tokio::test]
async fn should_return_false_on_second_resolve_when_same_request_resolved_twice() {
    // Given a pending request
    let bus = Arc::new(EventBus::new(16));
    let gate = Arc::new(ApprovalGate::with_event_bus(false, Arc::clone(&bus)));
    let mut receiver = bus.subscribe();

    let request_gate = Arc::clone(&gate);
    let handle = tokio::spawn(async move { request_gate.request("bash", "ls").await });

    let request_id = match recv_matching(&mut receiver, "ToolApprovalRequested", |e| {
        matches!(e, SessionEvent::ToolApprovalRequested { .. })
    })
    .await
    {
        SessionEvent::ToolApprovalRequested { request_id, .. } => request_id,
        other => panic!("expected ToolApprovalRequested, got {other:?}"),
    };

    // When resolving the same request twice
    let first = gate.resolve(request_id, ApprovalDecision::Approved);
    let second = gate.resolve(request_id, ApprovalDecision::Approved);

    // Then only the first resolution succeeds
    assert!(first);
    assert!(!second);
    assert_eq!(handle.await.unwrap(), ApprovalDecision::Approved);
}

#[tokio::test]
async fn should_resolve_pending_request_as_timeout_when_gate_drained() {
    // Given a pending request (the request_id is not needed to drain)
    let gate = Arc::new(ApprovalGate::new(false));
    let request_gate = Arc::clone(&gate);
    let handle = tokio::spawn(async move { request_gate.request("bash", "ls").await });
    wait_until("request registered", || gate.pending_count() == 1).await;

    // When draining the gate (session close)
    gate.drain().await;

    // Then the pending request resolves as Timeout and the gate is empty
    assert_eq!(handle.await.unwrap(), ApprovalDecision::Timeout);
    assert_eq!(gate.pending_count(), 0);
}

// ===========================================================================
// ApprovalGate — event bus integration
// ===========================================================================

#[tokio::test]
async fn should_publish_events_with_matching_ids_when_request_and_resolve_through_event_bus() {
    // Given a gate wired to an event bus
    let bus = Arc::new(EventBus::new(16));
    let gate = Arc::new(ApprovalGate::with_event_bus(false, Arc::clone(&bus)));
    let mut receiver = bus.subscribe();
    let agent_id = Uuid::new_v4();

    let request_gate = Arc::clone(&gate);
    let handle =
        tokio::spawn(async move { request_gate.request_for_agent(agent_id, "bash", "ls").await });

    // When the request is published and resolved as approved
    let request_id = match recv_matching(&mut receiver, "ToolApprovalRequested", |e| {
        matches!(e, SessionEvent::ToolApprovalRequested { .. })
    })
    .await
    {
        SessionEvent::ToolApprovalRequested {
            agent_id: event_agent_id,
            tool_name,
            request_id,
        } => {
            assert_eq!(event_agent_id, agent_id);
            assert_eq!(tool_name, "bash");
            request_id
        }
        other => panic!("expected ToolApprovalRequested, got {other:?}"),
    };
    assert!(gate.resolve(request_id, ApprovalDecision::Approved));
    assert_eq!(handle.await.unwrap(), ApprovalDecision::Approved);

    // Then a ToolApprovalResolved event is published for the same request
    match recv_matching(&mut receiver, "ToolApprovalResolved", |e| {
        matches!(e, SessionEvent::ToolApprovalResolved { .. })
    })
    .await
    {
        SessionEvent::ToolApprovalResolved {
            request_id: resolved_id,
            approved,
        } => {
            assert_eq!(resolved_id, request_id);
            assert!(approved);
        }
        other => panic!("expected ToolApprovalResolved, got {other:?}"),
    }
}

#[tokio::test]
async fn should_publish_resolved_event_with_approved_false_when_resolved_as_denied() {
    // Given a pending request
    let bus = Arc::new(EventBus::new(16));
    let gate = Arc::new(ApprovalGate::with_event_bus(false, Arc::clone(&bus)));
    let mut receiver = bus.subscribe();

    let request_gate = Arc::clone(&gate);
    let handle = tokio::spawn(async move { request_gate.request("bash", "ls").await });

    let request_id = match recv_matching(&mut receiver, "ToolApprovalRequested", |e| {
        matches!(e, SessionEvent::ToolApprovalRequested { .. })
    })
    .await
    {
        SessionEvent::ToolApprovalRequested { request_id, .. } => request_id,
        other => panic!("expected ToolApprovalRequested, got {other:?}"),
    };

    // When resolving as denied
    assert!(gate.resolve(request_id, ApprovalDecision::Denied("no".to_string())));
    assert_eq!(
        handle.await.unwrap(),
        ApprovalDecision::Denied("no".to_string())
    );

    // Then the resolved event reports approved: false with the same id
    match recv_matching(&mut receiver, "ToolApprovalResolved", |e| {
        matches!(e, SessionEvent::ToolApprovalResolved { .. })
    })
    .await
    {
        SessionEvent::ToolApprovalResolved {
            request_id: resolved_id,
            approved,
        } => {
            assert_eq!(resolved_id, request_id);
            assert!(!approved);
        }
        other => panic!("expected ToolApprovalResolved, got {other:?}"),
    }
}

// ===========================================================================
// QuestionGate — ask / answer / drain
// ===========================================================================

#[tokio::test]
async fn should_deliver_answer_when_question_asked_and_answered() {
    // Given a question gate wired to an event bus (the bus carries the id)
    let bus = Arc::new(EventBus::new(16));
    let gate = Arc::new(QuestionGate::with_event_bus(Arc::clone(&bus)));
    let mut receiver = bus.subscribe();

    let ask_gate = Arc::clone(&gate);
    let handle = tokio::spawn(async move { ask_gate.ask("what is 2+2?").await });

    // When the question is published and then answered
    let request_id = match recv_matching(&mut receiver, "QuestionAsked", |e| {
        matches!(e, SessionEvent::QuestionAsked { .. })
    })
    .await
    {
        SessionEvent::QuestionAsked {
            question,
            request_id,
            ..
        } => {
            assert_eq!(question, "what is 2+2?");
            request_id
        }
        other => panic!("expected QuestionAsked, got {other:?}"),
    };

    assert!(gate.answer(request_id, "4".to_string()));

    // Then the asker receives the answer
    assert_eq!(handle.await.unwrap(), Some("4".to_string()));
    assert_eq!(gate.pending_count(), 0);
}

#[tokio::test]
async fn should_return_false_when_answering_unknown_question_request_id() {
    // Given a question gate with no pending questions
    let gate = QuestionGate::new();

    // When answering a request id that was never registered
    let answered = gate.answer(Uuid::new_v4(), "42".to_string());

    // Then answering reports failure
    assert!(!answered);
}

#[tokio::test]
async fn should_return_none_when_question_gate_drained() {
    // Given a pending question
    let gate = Arc::new(QuestionGate::new());
    let ask_gate = Arc::clone(&gate);
    let handle = tokio::spawn(async move { ask_gate.ask("continue?").await });
    wait_until("question registered", || gate.pending_count() == 1).await;

    // When draining the gate (session close)
    gate.drain().await;

    // Then the asker gets None and the gate is empty
    assert_eq!(handle.await.unwrap(), None);
    assert_eq!(gate.pending_count(), 0);
}

#[tokio::test]
async fn should_publish_events_with_matching_ids_when_question_asked_and_answered_through_event_bus()
 {
    // Given a question gate wired to an event bus
    let bus = Arc::new(EventBus::new(16));
    let gate = Arc::new(QuestionGate::with_event_bus(Arc::clone(&bus)));
    let mut receiver = bus.subscribe();
    let agent_id = Uuid::new_v4();

    let ask_gate = Arc::clone(&gate);
    let handle = tokio::spawn(async move { ask_gate.ask_for_agent(agent_id, "proceed?").await });

    // When the question is published and then answered
    let request_id = match recv_matching(&mut receiver, "QuestionAsked", |e| {
        matches!(e, SessionEvent::QuestionAsked { .. })
    })
    .await
    {
        SessionEvent::QuestionAsked {
            agent_id: event_agent_id,
            question,
            request_id,
        } => {
            assert_eq!(event_agent_id, agent_id);
            assert_eq!(question, "proceed?");
            request_id
        }
        other => panic!("expected QuestionAsked, got {other:?}"),
    };
    assert!(gate.answer(request_id, "yes".to_string()));
    assert_eq!(handle.await.unwrap(), Some("yes".to_string()));

    // Then a QuestionAnswered event is published for the same request
    match recv_matching(&mut receiver, "QuestionAnswered", |e| {
        matches!(e, SessionEvent::QuestionAnswered { .. })
    })
    .await
    {
        SessionEvent::QuestionAnswered {
            request_id: answered_id,
            answer,
        } => {
            assert_eq!(answered_id, request_id);
            assert_eq!(answer, "yes");
        }
        other => panic!("expected QuestionAnswered, got {other:?}"),
    }
}

// ===========================================================================
// SessionShutdown — drain both gates
// ===========================================================================

#[tokio::test]
async fn should_timeout_approval_and_return_none_for_question_when_session_shuts_down() {
    // Given a session artifact set with one pending approval and one pending
    // question (the gates share the session's event bus)
    let artifacts = SessionArtifacts::new();

    let approval_request = Arc::clone(&artifacts.approval_gate);
    let approval_handle = tokio::spawn(async move { approval_request.request("bash", "ls").await });
    let question_request = Arc::clone(&artifacts.question_gate);
    let question_handle = tokio::spawn(async move { question_request.ask("continue?").await });
    wait_until("approval registered", || {
        artifacts.approval_gate.pending_count() == 1
    })
    .await;
    wait_until("question registered", || {
        artifacts.question_gate.pending_count() == 1
    })
    .await;

    // When the session shuts down
    let shutdown = SessionShutdown::new(&artifacts);
    shutdown.shutdown().await;

    // Then the approval times out and the question returns None
    assert_eq!(approval_handle.await.unwrap(), ApprovalDecision::Timeout);
    assert_eq!(question_handle.await.unwrap(), None);
    assert_eq!(artifacts.approval_gate.pending_count(), 0);
    assert_eq!(artifacts.question_gate.pending_count(), 0);
}

// ===========================================================================
// SessionManager — per-session artifacts
// ===========================================================================

#[tokio::test]
async fn should_get_distinct_artifacts_when_two_sessions_created() {
    // Given a manager with no sessions
    let env = TestEnv::new().await;

    // When creating two sessions
    let first = env
        .manager
        .create_session(make_metadata("First"), SessionScopeConfig::default())
        .await
        .unwrap();
    let second = env
        .manager
        .create_session(make_metadata("Second"), SessionScopeConfig::default())
        .await
        .unwrap();

    // Then each session exposes its own distinct event bus and gates
    let first_artifacts = env
        .manager
        .session_artifacts(first.id())
        .expect("first session artifacts");
    let second_artifacts = env
        .manager
        .session_artifacts(second.id())
        .expect("second session artifacts");
    assert!(!Arc::ptr_eq(
        &first_artifacts.event_bus,
        &second_artifacts.event_bus
    ));
    assert!(!Arc::ptr_eq(
        &first_artifacts.approval_gate,
        &second_artifacts.approval_gate
    ));
    assert!(!Arc::ptr_eq(
        &first_artifacts.question_gate,
        &second_artifacts.question_gate
    ));

    // And both sessions start with empty (fresh) gates
    assert_eq!(first_artifacts.approval_gate.pending_count(), 0);
    assert_eq!(first_artifacts.question_gate.pending_count(), 0);
    assert_eq!(second_artifacts.approval_gate.pending_count(), 0);
    assert_eq!(second_artifacts.question_gate.pending_count(), 0);
}

#[tokio::test]
async fn should_keep_first_session_request_pending_when_second_session_created() {
    // Given a session with a pending approval request
    let env = TestEnv::new().await;
    let first = env
        .manager
        .create_session(make_metadata("First"), SessionScopeConfig::default())
        .await
        .unwrap();
    let first_artifacts = env
        .manager
        .session_artifacts(first.id())
        .expect("first session artifacts");

    let request_gate = Arc::clone(&first_artifacts.approval_gate);
    let handle = tokio::spawn(async move { request_gate.request("bash", "ls").await });
    wait_until("approval registered", || {
        first_artifacts.approval_gate.pending_count() == 1
    })
    .await;

    // When a second session is created
    let second = env
        .manager
        .create_session(make_metadata("Second"), SessionScopeConfig::default())
        .await
        .unwrap();
    let second_artifacts = env
        .manager
        .session_artifacts(second.id())
        .expect("second session artifacts");

    // Then the first session's request is NOT drained — it stays pending
    // (per-session gates isolate sessions from each other)
    assert_eq!(first_artifacts.approval_gate.pending_count(), 1);
    assert_eq!(second_artifacts.approval_gate.pending_count(), 0);

    // And the requester only resolves once the first session closes, which
    // drains exactly that session's gates
    env.manager
        .close_session(first.id(), CloseReason::Exit)
        .await
        .unwrap();
    assert_eq!(handle.await.unwrap(), ApprovalDecision::Timeout);
    assert_eq!(first_artifacts.approval_gate.pending_count(), 0);
}

#[tokio::test]
async fn should_drain_gates_and_publish_event_when_session_closed() {
    // Given a session with a pending approval request
    let env = TestEnv::new().await;
    let session = env
        .manager
        .create_session(make_metadata("To Close"), SessionScopeConfig::default())
        .await
        .unwrap();
    let session_id = session.id().clone();
    let expected_uuid = uuid_from_id(session_id.as_str());
    let artifacts = env
        .manager
        .session_artifacts(&session_id)
        .expect("session artifacts");

    let request_gate = Arc::clone(&artifacts.approval_gate);
    let handle = tokio::spawn(async move { request_gate.request("bash", "ls").await });
    wait_until("approval registered", || {
        artifacts.approval_gate.pending_count() == 1
    })
    .await;

    let mut receiver = artifacts.event_bus.subscribe();

    // When closing the session
    env.manager
        .close_session(&session_id, CloseReason::Exit)
        .await
        .unwrap();

    // Then the pending request is drained (resolves as Timeout)
    assert_eq!(handle.await.unwrap(), ApprovalDecision::Timeout);
    assert_eq!(artifacts.approval_gate.pending_count(), 0);

    // And a resolution event is published before the SessionClosed event
    match recv_matching(&mut receiver, "ToolApprovalResolved", |e| {
        matches!(e, SessionEvent::ToolApprovalResolved { .. })
    })
    .await
    {
        SessionEvent::ToolApprovalResolved { approved, .. } => assert!(!approved),
        other => panic!("expected ToolApprovalResolved, got {other:?}"),
    }
    match recv_matching(&mut receiver, "SessionClosed", |e| {
        matches!(e, SessionEvent::SessionClosed { .. })
    })
    .await
    {
        SessionEvent::SessionClosed {
            session_id: closed_id,
        } => assert_eq!(closed_id, expected_uuid),
        other => panic!("expected SessionClosed, got {other:?}"),
    }

    // And the closed session's artifacts are no longer registered
    assert!(
        env.manager.session_artifacts(&session_id).is_none(),
        "closed session must not keep artifacts registered"
    );
}

#[tokio::test]
async fn should_install_fresh_artifacts_when_session_resumed() {
    // Given a persisted session (state.json + wire.jsonl on disk)
    let env = TestEnv::new().await;
    let session = env
        .manager
        .create_session(make_metadata("To Resume"), SessionScopeConfig::default())
        .await
        .unwrap();
    let session_id = session.id().clone();
    let expected_uuid = uuid_from_id(session_id.as_str());
    let old_artifacts = env
        .manager
        .session_artifacts(&session_id)
        .expect("pre-close artifacts");

    let wire_log_path = env.manager.persistence().wire_log_path(&session_id);
    tokio::fs::create_dir_all(wire_log_path.parent().unwrap())
        .await
        .unwrap();
    // A minimal resumable log: the metadata envelope plus the model record
    // a real embedded create writes (spec 11 R4 — the resume chain needs a
    // resolvable model: the wire log's record, else the base config).
    tokio::fs::write(
        &wire_log_path,
        concat!(
            "{\"type\":\"metadata\"}\n",
            "{\"type\":\"config.update\",\"modelAlias\":\"resume-events-model\"}\n",
        ),
    )
    .await
    .unwrap();
    env.manager
        .close_session(&session_id, CloseReason::Exit)
        .await
        .unwrap();
    assert!(
        env.manager.session_artifacts(&session_id).is_none(),
        "closed session must not keep artifacts registered"
    );

    // When resuming (the manager installs fresh artifacts; the resumed
    // announcement is the caller's to publish, after wiring its relay)
    env.manager.resume_session(&session_id).await.unwrap();

    // Then fresh artifacts are installed: a new event bus and empty gates
    let resumed_artifacts = env
        .manager
        .session_artifacts(&session_id)
        .expect("resumed session artifacts");
    assert!(
        !Arc::ptr_eq(&old_artifacts.event_bus, &resumed_artifacts.event_bus),
        "resumed session must get a fresh event bus"
    );
    assert_eq!(resumed_artifacts.approval_gate.pending_count(), 0);
    assert_eq!(resumed_artifacts.question_gate.pending_count(), 0);

    // And the fresh bus is functional for the resumed session
    let mut receiver = resumed_artifacts.event_bus.subscribe();
    resumed_artifacts
        .event_bus
        .publish(SessionEvent::SessionClosed {
            session_id: uuid_from_id(session_id.as_str()),
        });
    match recv_matching(&mut receiver, "SessionClosed", |e| {
        matches!(e, SessionEvent::SessionClosed { .. })
    })
    .await
    {
        SessionEvent::SessionClosed {
            session_id: closed_id,
        } => assert_eq!(closed_id, expected_uuid),
        other => panic!("expected SessionClosed, got {other:?}"),
    }
}

#[tokio::test]
async fn should_publish_session_forked_event_on_source_bus_when_session_forked() {
    // Given a source session with an agent
    let env = TestEnv::new().await;
    let source_session = env
        .manager
        .create_session(make_metadata("Source"), SessionScopeConfig::default())
        .await
        .unwrap();
    let source_uuid = uuid_from_id(source_session.id().as_str());
    let source_artifacts = env
        .manager
        .session_artifacts(source_session.id())
        .expect("source session artifacts");

    let lifecycle = AgentLifecycle::new(Arc::clone(&source_session));
    let source_agent = lifecycle
        .create_agent(AgentScopeConfig::default())
        .await
        .unwrap();

    // When forking the session (subscribing to the source session's bus
    // first, since the forked session's bus has no subscribers yet)
    let mut receiver = source_artifacts.event_bus.subscribe();
    let forked_session = env
        .manager
        .fork_session(&source_session, source_agent.id(), make_metadata("Forked"))
        .await
        .unwrap();
    let forked_uuid = uuid_from_id(forked_session.id().as_str());

    // Then a SessionForked event is published on the source session's bus
    // with both session UUIDs
    match recv_matching(&mut receiver, "SessionForked", |e| {
        matches!(e, SessionEvent::SessionForked { .. })
    })
    .await
    {
        SessionEvent::SessionForked {
            source_session_id,
            new_session_id,
        } => {
            assert_eq!(source_session_id, source_uuid);
            assert_eq!(new_session_id, forked_uuid);
        }
        other => panic!("expected SessionForked, got {other:?}"),
    }

    // And the forked session owns its own distinct artifacts
    let forked_artifacts = env
        .manager
        .session_artifacts(forked_session.id())
        .expect("forked session artifacts");
    assert!(
        !Arc::ptr_eq(&source_artifacts.event_bus, &forked_artifacts.event_bus),
        "forked session must get its own event bus"
    );
}

#[tokio::test]
async fn should_announce_goal_updated_on_fork_bus_when_fork_main_agent_creates_goal() {
    // Given a source session with an agent (whose config carries the SOURCE
    // session's event bus — the fork pipeline's inheritance seed)
    let env = TestEnv::new().await;
    let source_session = env
        .manager
        .create_session(make_metadata("Source"), SessionScopeConfig::default())
        .await
        .unwrap();
    let source_artifacts = env
        .manager
        .session_artifacts(source_session.id())
        .expect("source session artifacts");
    let lifecycle = AgentLifecycle::new(Arc::clone(&source_session));
    let source_agent = lifecycle
        .create_agent(AgentScopeConfig {
            event_bus: Some(Arc::clone(&source_artifacts.event_bus)),
            ..AgentScopeConfig::default()
        })
        .await
        .unwrap();

    // When forking the session
    let forked_session = env
        .manager
        .fork_session(&source_session, source_agent.id(), make_metadata("Forked"))
        .await
        .unwrap();
    let forked_artifacts = env
        .manager
        .session_artifacts(forked_session.id())
        .expect("forked session artifacts");

    // Then the fork's main agent carries the FORK session's event bus — a
    // goal created through its tools must announce on the fork's own stream,
    // never on the source's (the live goal card would otherwise never appear
    // in the fork's web client).
    let fork_main = forked_session
        .get_agent(&mirri_core::AgentId::new(
            mirri_core::session::manager::MAIN_AGENT_ID,
        ))
        .await
        .expect("fork main agent");
    let fork_bus = forked_artifacts.event_bus.clone();
    let agent_bus = fork_main
        .config()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .event_bus
        .clone();
    let agent_bus = agent_bus.expect("fork main agent must have an event bus");
    assert!(
        Arc::ptr_eq(&agent_bus, &fork_bus),
        "fork main agent's event bus must be the fork session's own bus"
    );
}

#[tokio::test]
async fn should_bind_fork_main_agent_to_fork_gates_when_fork_inherits_interaction_kernel() {
    // Given a source session whose main agent carries the source session's
    // interaction kernel (the embedded create path always wires one)
    let env = TestEnv::new().await;
    let source_session = env
        .manager
        .create_session(make_metadata("Source"), SessionScopeConfig::default())
        .await
        .unwrap();
    let source_artifacts = env
        .manager
        .session_artifacts(source_session.id())
        .expect("source session artifacts");
    let source_kernel = Arc::new(mirri_core::InteractionKernel::new(
        Arc::clone(&source_artifacts.approval_gate),
        Arc::clone(&source_artifacts.question_gate),
    ));
    let lifecycle = AgentLifecycle::new(Arc::clone(&source_session));
    let source_agent = lifecycle
        .create_agent(AgentScopeConfig {
            interaction_kernel: Some(source_kernel),
            ..AgentScopeConfig::default()
        })
        .await
        .unwrap();

    // When forking the session
    let forked_session = env
        .manager
        .fork_session(&source_session, source_agent.id(), make_metadata("Forked"))
        .await
        .unwrap();
    let forked_artifacts = env
        .manager
        .session_artifacts(forked_session.id())
        .expect("forked session artifacts");

    // Then the fork's main agent carries an interaction kernel bound to the
    // FORK session's gates — an approval asked through it registers with the
    // fork's pending lists, never with the source's.
    let fork_main = forked_session
        .get_agent(&mirri_core::AgentId::new(
            mirri_core::session::manager::MAIN_AGENT_ID,
        ))
        .await
        .expect("fork main agent");
    let kernel = fork_main
        .config()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .interaction_kernel
        .clone()
        .expect("fork main agent must have an interaction kernel");

    // Behavioral proof: register a pending approval on the FORK's gate and
    // observe it through the fork main agent's kernel.
    use mirri_core::ApprovalSnapshot;
    let fork_gate = forked_artifacts.approval_gate.clone();
    let (_receiver_id, _receiver) = fork_gate.register(ApprovalSnapshot {
        tool_name: "Bash".to_string(),
        action: "execute Bash".to_string(),
        display: serde_json::json!({}),
        agent_id: uuid::Uuid::nil(),
        turn_id: None,
        tool_call_id: None,
        created_at: 0,
        approval_rule: None,
        grant_dir: None,
    });
    let pending = kernel.approval_list_pending();
    assert_eq!(
        pending.len(),
        1,
        "the fork main agent's kernel must observe the fork's gate, not the source's"
    );
    assert!(
        source_artifacts.approval_gate.list_pending().is_empty(),
        "the source's gate must not observe the fork's registration"
    );
}

#[tokio::test]
async fn should_return_error_and_leave_live_session_intact_when_closing_nonexistent_session() {
    // Given a manager with an active session
    let env = TestEnv::new().await;
    let session = env
        .manager
        .create_session(make_metadata("Active"), SessionScopeConfig::default())
        .await
        .unwrap();
    let artifacts = env
        .manager
        .session_artifacts(session.id())
        .expect("session artifacts");
    let mut receiver = artifacts.event_bus.subscribe();

    // When closing a session that does not exist
    let result = env
        .manager
        .close_session(&SessionId::new("nonexistent-session"), CloseReason::Exit)
        .await;

    // Then the close fails, no event is published, and the live session's
    // gates are intact
    assert!(result.is_err());
    assert!(
        matches!(receiver.try_recv(), Err(TryRecvError::Empty)),
        "no event should be published for a failed close"
    );

    // And the live session's gates still accept and drain requests normally
    let request_gate = Arc::clone(&artifacts.approval_gate);
    let handle = tokio::spawn(async move { request_gate.request("bash", "ls").await });
    wait_until("approval registered", || {
        artifacts.approval_gate.pending_count() == 1
    })
    .await;
    artifacts.approval_gate.drain().await;
    assert_eq!(handle.await.unwrap(), ApprovalDecision::Timeout);
    assert_eq!(artifacts.approval_gate.pending_count(), 0);
}

// ===========================================================================
// Per-session isolation — events and gates never cross sessions
// ===========================================================================

#[tokio::test]
async fn should_keep_other_bus_silent_when_event_published_on_one_session_bus() {
    // Given two sessions with separate event buses
    let env = TestEnv::new().await;
    let first = env
        .manager
        .create_session(make_metadata("First"), SessionScopeConfig::default())
        .await
        .unwrap();
    let second = env
        .manager
        .create_session(make_metadata("Second"), SessionScopeConfig::default())
        .await
        .unwrap();
    let first_artifacts = env
        .manager
        .session_artifacts(first.id())
        .expect("first session artifacts");
    let second_artifacts = env
        .manager
        .session_artifacts(second.id())
        .expect("second session artifacts");
    let mut first_rx = first_artifacts.event_bus.subscribe();
    let mut second_rx = second_artifacts.event_bus.subscribe();

    // When an event is published on the first session's bus
    first_artifacts
        .event_bus
        .publish(SessionEvent::MessageReceived {
            agent_id: "main".to_string(),
            role: "user".to_string(),
            content_summary: "hello from session one".to_string(),
        });

    // Then the first session's subscriber receives it
    match recv_matching(&mut first_rx, "MessageReceived", |e| {
        matches!(e, SessionEvent::MessageReceived { .. })
    })
    .await
    {
        SessionEvent::MessageReceived {
            content_summary, ..
        } => assert_eq!(content_summary, "hello from session one"),
        other => panic!("expected MessageReceived, got {other:?}"),
    }

    // And the second session's bus never saw it
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        matches!(second_rx.try_recv(), Err(TryRecvError::Empty)),
        "events must not leak across session buses"
    );
}

#[tokio::test]
async fn should_not_affect_other_gate_when_approval_resolved_for_one_session() {
    // Given two sessions with separate approval gates
    let env = TestEnv::new().await;
    let first = env
        .manager
        .create_session(make_metadata("First"), SessionScopeConfig::default())
        .await
        .unwrap();
    let second = env
        .manager
        .create_session(make_metadata("Second"), SessionScopeConfig::default())
        .await
        .unwrap();
    let first_artifacts = env
        .manager
        .session_artifacts(first.id())
        .expect("first session artifacts");
    let second_artifacts = env
        .manager
        .session_artifacts(second.id())
        .expect("second session artifacts");

    // When a request is registered on the first session's gate
    let request_gate = Arc::clone(&first_artifacts.approval_gate);
    let handle = tokio::spawn(async move { request_gate.request("bash", "ls").await });
    let mut first_rx = first_artifacts.event_bus.subscribe();
    let request_id = match recv_matching(&mut first_rx, "ToolApprovalRequested", |e| {
        matches!(e, SessionEvent::ToolApprovalRequested { .. })
    })
    .await
    {
        SessionEvent::ToolApprovalRequested { request_id, .. } => request_id,
        other => panic!("expected ToolApprovalRequested, got {other:?}"),
    };
    assert_eq!(first_artifacts.approval_gate.pending_count(), 1);

    // And it is resolved through the first session's gate
    assert!(
        first_artifacts
            .approval_gate
            .resolve(request_id, ApprovalDecision::Approved)
    );
    assert_eq!(handle.await.unwrap(), ApprovalDecision::Approved);

    // Then the second session's gate never saw the request: it stayed empty,
    // and resolving the same request id against it reports failure
    assert_eq!(second_artifacts.approval_gate.pending_count(), 0);
    assert!(
        !second_artifacts
            .approval_gate
            .resolve(request_id, ApprovalDecision::Approved),
        "a request id from another session must not resolve on this gate"
    );
}

// ===========================================================================
// Event projection — the shared event.* wire vocabulary
// ===========================================================================

/// Envelope keys the transport owns; payload objects must never use them
/// (payload keys are merged flat into the notification's `params`).
const RESERVED_ENVELOPE_KEYS: [&str; 5] = ["sessionId", "volatile", "timestamp", "seq", "epoch"];

/// One instance of every [`SessionEvent`] variant (49 total), for
/// exhaustive enumeration in the projection invariants below.
fn all_variants() -> Vec<SessionEvent> {
    vec![
        SessionEvent::AgentCreated {
            agent_id: Uuid::nil(),
            session_id: Uuid::nil(),
        },
        SessionEvent::AgentStatusUpdated {
            agent_id: "main".to_string(),
            phase: mirri_core::session::agent_status::AgentPhase::Running,
            model: None,
            thinking_effort: None,
            context_tokens: None,
            max_context_tokens: None,
            plan_mode: None,
            swarm_mode: None,
            usage: None,
        },
        SessionEvent::AgentRemoved {
            agent_id: Uuid::nil(),
        },
        SessionEvent::SessionResumed {
            session_id: Uuid::nil(),
        },
        SessionEvent::SessionForked {
            source_session_id: Uuid::nil(),
            new_session_id: Uuid::nil(),
        },
        SessionEvent::SessionClosed {
            session_id: Uuid::nil(),
        },
        SessionEvent::ToolApprovalRequested {
            agent_id: Uuid::nil(),
            tool_name: "bash".to_string(),
            request_id: Uuid::nil(),
        },
        SessionEvent::ToolApprovalResolved {
            request_id: Uuid::nil(),
            approved: true,
        },
        SessionEvent::QuestionAsked {
            agent_id: Uuid::nil(),
            question: "q?".to_string(),
            request_id: Uuid::nil(),
        },
        SessionEvent::QuestionAnswered {
            request_id: Uuid::nil(),
            answer: "a".to_string(),
        },
        SessionEvent::QuestionDismissed {
            request_id: Uuid::nil(),
        },
        SessionEvent::MessageReceived {
            agent_id: "main".to_string(),
            role: "user".to_string(),
            content_summary: "hello".to_string(),
        },
        SessionEvent::AgentMessageChunk {
            agent_id: "main".to_string(),
            text: "chunk".to_string(),
        },
        SessionEvent::ThinkingDelta {
            agent_id: "main".to_string(),
            text: "thinking".to_string(),
        },
        SessionEvent::ToolCallDelta {
            agent_id: "main".to_string(),
            tool_call_id: "call_1".to_string(),
            tool_name: Some("bash".to_string()),
            arguments_part: "{\"a\":".to_string(),
        },
        SessionEvent::TurnFailed {
            agent_id: "main".to_string(),
            turn_id: "turn-1".to_string(),
            error: "boom".to_string(),
            error_code: "internal".to_string(),
            wire_turn_id: 1,
        },
        SessionEvent::TurnStarted {
            agent_id: "main".to_string(),
            session_id: "session-1".to_string(),
            turn_id: "turn-1".to_string(),
        },
        SessionEvent::TurnCompleted {
            agent_id: "main".to_string(),
            session_id: "session-1".to_string(),
            turn_id: "turn-1".to_string(),
            stop_reason: "completed".to_string(),
            wire_turn_id: 1,
            duration_ms: Some(500),
        },
        SessionEvent::ToolCallStarted {
            agent_id: "main".to_string(),
            session_id: "session-1".to_string(),
            tool_name: "bash".to_string(),
            tool_call_id: "call-1".to_string(),
            args: serde_json::json!({ "command": "cargo test" }),
        },
        SessionEvent::ToolCallCompleted {
            agent_id: "main".to_string(),
            session_id: "session-1".to_string(),
            tool_call_id: "call-1".to_string(),
            tool_name: "bash".to_string(),
            is_error: false,
            output: mirri_core::ToolOutput::Text("42 passed".to_string()),
        },
        SessionEvent::ToolProgress {
            agent_id: "main".to_string(),
            tool_call_id: "call-1".to_string(),
            update: mirri_core::tool::ToolUpdate {
                kind: mirri_core::tool::ToolUpdateKind::Progress,
                text: Some("halfway".to_string()),
                percent: Some(50.0),
                custom_kind: None,
                custom_data: None,
            },
        },
        SessionEvent::CompactionStarted {
            agent_id: "main".to_string(),
            trigger: CompactionSource::Manual,
            instruction: Some("keep the test facts".to_string()),
        },
        SessionEvent::CompactionCancelled {
            agent_id: "main".to_string(),
        },
        SessionEvent::CompactionBlocked {
            agent_id: "main".to_string(),
            turn_id: Some("turn-1".to_string()),
        },
        SessionEvent::CompactionCompleted {
            agent_id: "main".to_string(),
            result: CompactionResult {
                applied: true,
                summary: "summary".to_string(),
                context_summary: "context summary".to_string(),
                compacted_count: 4,
                tokens_before: 100,
                tokens_after: 20,
                dropped_count: 1,
                dropped_entry_positions: Vec::new(),
                dropped_entry_seqs: Vec::new(),
                retained_tail_count: 0,
                kept_head_user_message_count: 0,
                elision_message: None,
                first_replaced_entry_seq: None,
                last_replaced_entry_seq: None,
            },
        },
        SessionEvent::HookResult {
            hook_event: "UserPromptSubmit".to_string(),
            content: "hook says hi".to_string(),
            blocked: None,
        },
        SessionEvent::BackgroundTaskStarted {
            info: BackgroundTaskInfo {
                kind: TaskKind::Process {
                    command: Some("cargo build".to_string()),
                    dispatcher: None,
                },
                task_id: "task-1".to_string(),
                description: "release build".to_string(),
                status: "running".to_string(),
                started_at: 1_700_000_000_000_i64,
                ended_at: None,
                stop_reason: None,
                agent_id: None,
                exit_code: None,
            },
        },
        SessionEvent::BackgroundTaskTerminated {
            info: BackgroundTaskInfo {
                kind: TaskKind::Agent {
                    agent_id: Some("agent-1".to_string()),
                    subagent_type: None,
                    parent_tool_call_id: None,
                    swarm_index: None,
                    model: None,
                },
                task_id: "task-2".to_string(),
                description: "subagent".to_string(),
                status: "failed".to_string(),
                started_at: 1_700_000_000_000_i64,
                ended_at: Some(1_700_000_001_500_i64),
                stop_reason: Some("boom".to_string()),
                agent_id: None,
                exit_code: None,
            },
        },
        SessionEvent::ShellStarted {
            command_id: "shell-1".to_string(),
            task_id: "task-3".to_string(),
        },
        SessionEvent::ShellOutput {
            command_id: "shell-1".to_string(),
            update: mirri_core::tool::ToolUpdate {
                kind: mirri_core::tool::ToolUpdateKind::Stdout,
                text: Some("chunk".to_string()),
                percent: None,
                custom_kind: None,
                custom_data: None,
            },
            task_id: Some("task-3".to_string()),
        },
        SessionEvent::ShellCompleted {
            command_id: "shell-1".to_string(),
            is_error: false,
            task_id: Some("task-3".to_string()),
        },
        SessionEvent::SubagentSpawned {
            subagent_id: "agent-1".to_string(),
            subagent_name: "coder".to_string(),
            description: Some("test spawn".to_string()),
            run_in_background: false,
            model: Some("test-model".to_string()),
            parent_agent_id: Some("main".to_string()),
            parent_tool_call_id: Some("call-1".to_string()),
            swarm_index: Some(0),
        },
        SessionEvent::SubagentStarted {
            subagent_id: "agent-1".to_string(),
        },
        // The legacy pre-ADR-011 free-form reason stays part of the
        // vocabulary (the compat-window `Legacy` form).
        SessionEvent::SubagentSuspended {
            subagent_id: "agent-1".to_string(),
            reason: SuspendCause::Legacy("rate_limited".to_string()),
        },
        SessionEvent::SubagentCompleted {
            subagent_id: "agent-1".to_string(),
            result_summary: "done".to_string(),
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 2,
            cache_creation_tokens: 1,
        },
        SessionEvent::SubagentFailed {
            subagent_id: "agent-1".to_string(),
            error: "turn failed".to_string(),
        },
        SessionEvent::TurnStepStarted {
            turn_id: "turn-1".to_string(),
            step: 1,
        },
        SessionEvent::TurnStepCompleted {
            turn_id: "turn-1".to_string(),
            step: 1,
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 2,
            cache_creation_tokens: 1,
            model: None,
        },
        SessionEvent::TurnStepInterrupted {
            turn_id: Some("turn-1".to_string()),
            step: 2,
            reason: "aborted".to_string(),
            message: Some("cancelled by user".to_string()),
        },
        SessionEvent::TurnStepRetrying {
            turn_id: Some("turn-1".to_string()),
            step: 2,
            failed_attempt: 1,
            next_attempt: 2,
            max_attempts: 3,
            delay_ms: 0,
        },
        SessionEvent::GoalUpdated {
            snapshot: serde_json::json!({ "goalId": "goal-1", "status": "active" }),
            change: Some(serde_json::json!({ "kind": "lifecycle", "status": "active" })),
        },
        SessionEvent::SkillActivated {
            activation_id: "act-1".to_string(),
            skill_name: "commit".to_string(),
            args: None,
            trigger: "user-slash".to_string(),
            skill_path: Some("/skills/commit".to_string()),
            skill_source: Some("project".to_string()),
        },
        SessionEvent::McpServerStatus {
            name: "server-1".to_string(),
            transport: "stdio".to_string(),
            status: "connected".to_string(),
            tool_count: 3,
            error: None,
            pool: "workspace".to_string(),
            change_kind: Some("added".to_string()),
        },
        SessionEvent::SessionMetaUpdated {
            title: Some("renamed".to_string()),
            patch: Some(serde_json::json!({ "title": "renamed" })),
        },
        SessionEvent::ModelSwitched {
            from: "kimi-k2".to_string(),
            to: "claude-sonnet-4".to_string(),
        },
        SessionEvent::PluginCommandActivated {
            activation_id: "act-2".to_string(),
            plugin_id: "plugin-1".to_string(),
            command_name: "build".to_string(),
            command_args: Some("--release".to_string()),
            trigger: "user-slash".to_string(),
        },
        SessionEvent::CronFired {
            task_id: "cron-1".to_string(),
            cron: "* * * * *".to_string(),
            prompt: "check the build".to_string(),
            recurring: true,
            coalesced_count: 1,
            stale: false,
        },
        SessionEvent::SessionWorkChanged {
            state: SessionActivityState::default(),
        },
        SessionEvent::Unknown,
    ]
}

fn projected(event: &SessionEvent) -> event_projection::ProjectedEvent {
    event_projection::project(event).expect("event has a wire projection")
}

#[test]
fn should_carry_real_coalesce_and_stale_when_projecting_a_cron_fired_event() {
    // Given a CronFired event with a coalesced, stale fire
    let event = SessionEvent::CronFired {
        task_id: "cron-1".to_string(),
        cron: "*/5 * * * *".to_string(),
        prompt: "tick".to_string(),
        recurring: true,
        coalesced_count: 3,
        stale: true,
    };

    // When projected onto the wire event
    let projected = projected(&event);

    // Then the payload mirrors the TS CronFiredEvent origin
    assert_eq!(projected.method, "event.cron.fired");
    assert_eq!(projected.payload["origin"]["coalescedCount"], 3);
    assert_eq!(projected.payload["origin"]["stale"], true);
    assert_eq!(projected.payload["origin"]["jobId"], "cron-1");
    assert_eq!(projected.payload["prompt"], "tick");
}

#[test]
fn should_project_session_work_changed_onto_the_durable_event_with_snake_case_payload() {
    // Given a SessionWorkChanged event carrying a non-idle activity state
    let event = SessionEvent::SessionWorkChanged {
        state: SessionActivityState {
            busy: true,
            main_turn_active: true,
            pending_interaction: SessionPendingInteraction::None,
            last_turn_reason: Some(SessionTurnOutcome::Completed),
        },
    };

    // When projected onto the wire event
    let projected = projected(&event);

    // Then the durable event carries the snake_case activity state payload
    assert_eq!(projected.method, "event.session.work_changed");
    assert!(!projected.volatile);
    assert_eq!(projected.payload["busy"], serde_json::json!(true));
    assert_eq!(
        projected.payload["main_turn_active"],
        serde_json::json!(true)
    );
    assert_eq!(
        projected.payload["pending_interaction"],
        serde_json::json!("none")
    );
    assert_eq!(
        projected.payload["last_turn_reason"],
        serde_json::json!("completed")
    );
}

#[test]
fn should_project_message_received_to_event_message_received_when_event_is_message_received() {
    // Given a message_received event
    let event = SessionEvent::MessageReceived {
        agent_id: "main".to_string(),
        role: "user".to_string(),
        content_summary: "hello".to_string(),
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.message.received with the camelCase payload
    assert_eq!(projected.method, "event.message.received");
    assert!(!projected.volatile);
    assert_eq!(projected.payload["agentId"], "main");
    assert_eq!(projected.payload["role"], "user");
    assert_eq!(projected.payload["contentSummary"], "hello");
}

#[test]
fn should_project_agent_message_chunk_to_event_assistant_delta_when_event_is_agent_message_chunk() {
    // Given an agent_message_chunk streaming event
    let event = SessionEvent::AgentMessageChunk {
        agent_id: "main".to_string(),
        text: "streamed".to_string(),
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.assistant.delta with the chunk payload
    // (the `delta` key, no legacy `done` marker)
    assert_eq!(projected.method, "event.assistant.delta");
    assert!(projected.volatile);
    assert_eq!(projected.payload["agentId"], "main");
    assert_eq!(projected.payload["delta"], "streamed");
    assert!(
        projected.payload.get("done").is_none(),
        "the delta payload no longer carries the done marker"
    );
}

#[test]
fn should_project_a_thinking_delta_to_event_thinking_delta_when_event_is_thinking_delta() {
    // Given a thinking_delta streaming event (the live reasoning channel)
    let event = SessionEvent::ThinkingDelta {
        agent_id: "main".to_string(),
        text: "reasoning".to_string(),
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.thinking.delta with the same shape the
    // assistant delta carries — agent id plus the chunk payload, volatile
    assert_eq!(projected.method, "event.thinking.delta");
    assert!(projected.volatile);
    assert_eq!(projected.payload["agentId"], "main");
    assert_eq!(projected.payload["delta"], "reasoning");
}

#[test]
fn should_project_a_tool_call_delta_to_event_tool_call_delta_when_event_is_tool_call_delta() {
    // Given a tool_call_delta streaming event with a known identity
    let event = SessionEvent::ToolCallDelta {
        agent_id: "main".to_string(),
        tool_call_id: "call_1".to_string(),
        tool_name: Some("bash".to_string()),
        arguments_part: "{\"a\":".to_string(),
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.tool.call.delta — volatile, tool call
    // id plus the argument fragment, mirroring the thinking delta shape
    assert_eq!(projected.method, "event.tool.call.delta");
    assert!(projected.volatile);
    assert_eq!(projected.payload["agentId"], "main");
    assert_eq!(projected.payload["toolCallId"], "call_1");
    assert_eq!(projected.payload["name"], "bash");
    assert_eq!(projected.payload["argumentsPart"], "{\"a\":");
}

#[test]
fn should_omit_the_name_when_a_tool_call_delta_has_no_tool_name() {
    // Given a tool_call_delta whose provider part carried no identity
    let event = SessionEvent::ToolCallDelta {
        agent_id: "main".to_string(),
        tool_call_id: "call_1".to_string(),
        tool_name: None,
        arguments_part: "}".to_string(),
    };

    // When projecting it
    let projected = projected(&event);

    // Then the optional name stays absent — never a null placeholder, the
    // protocol schema (z.string().optional()) rejects null
    assert_eq!(projected.method, "event.tool.call.delta");
    assert!(projected.payload.get("name").is_none());
    assert_eq!(projected.payload["argumentsPart"], "}");
}

#[test]
fn should_project_a_compaction_started_to_event_compaction_started_when_event_is_compaction_started()
 {
    // Given a manual compaction started event
    let event = SessionEvent::CompactionStarted {
        agent_id: "main".to_string(),
        trigger: CompactionSource::Manual,
        instruction: Some("keep the test facts".to_string()),
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.compaction.started with the trigger and
    // the optional instruction, durable (compaction is a lifecycle change)
    assert_eq!(projected.method, "event.compaction.started");
    assert!(!projected.volatile);
    assert_eq!(projected.payload["trigger"], "manual");
    assert_eq!(projected.payload["instruction"], "keep the test facts");
}

#[test]
fn should_omit_the_instruction_when_a_compaction_started_has_no_instruction() {
    // Given an auto-triggered compaction started event without one
    let event = SessionEvent::CompactionStarted {
        agent_id: "main".to_string(),
        trigger: CompactionSource::Auto,
        instruction: None,
    };

    // When projecting it
    let projected = projected(&event);

    // Then the trigger maps to the protocol vocabulary and the absent
    // instruction stays absent (no null placeholder)
    assert_eq!(projected.payload["trigger"], "auto");
    assert!(projected.payload.get("instruction").is_none());
}

#[test]
fn should_project_a_compaction_cancelled_to_event_compaction_cancelled_when_event_is_compaction_cancelled()
 {
    // Given a compaction cancelled event
    let event = SessionEvent::CompactionCancelled {
        agent_id: "main".to_string(),
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.compaction.cancelled carrying only the
    // agent's attribution — the protocol type carries no other fields — and
    // durable
    assert_eq!(projected.method, "event.compaction.cancelled");
    assert!(!projected.volatile);
    assert_eq!(projected.payload, serde_json::json!({ "agentId": "main" }));
}

#[test]
fn should_project_a_compaction_completed_to_event_compaction_completed_when_event_is_compaction_completed()
 {
    // Given a compaction completed event carrying the engine's result
    let event = SessionEvent::CompactionCompleted {
        agent_id: "main".to_string(),
        result: CompactionResult {
            applied: true,
            summary: "the summary".to_string(),
            context_summary: "context: the summary".to_string(),
            compacted_count: 7,
            tokens_before: 1000,
            tokens_after: 120,
            dropped_count: 2,
            dropped_entry_positions: Vec::new(),
            dropped_entry_seqs: Vec::new(),
            retained_tail_count: 0,
            kept_head_user_message_count: 0,
            elision_message: None,
            first_replaced_entry_seq: None,
            last_replaced_entry_seq: None,
        },
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.compaction.completed with the protocol's
    // CompactionResult fields (camelCase); the rust-only context_summary
    // stays off the wire and the untracked kept* fields stay absent
    assert_eq!(projected.method, "event.compaction.completed");
    assert!(!projected.volatile);
    assert_eq!(projected.payload["result"]["summary"], "the summary");
    assert_eq!(projected.payload["result"]["compactedCount"], 7);
    assert_eq!(projected.payload["result"]["tokensBefore"], 1000);
    assert_eq!(projected.payload["result"]["tokensAfter"], 120);
    assert_eq!(projected.payload["result"]["droppedCount"], 2);
    assert!(projected.payload["result"].get("contextSummary").is_none());
    assert!(
        projected.payload["result"]
            .get("keptUserMessageCount")
            .is_none()
    );
    assert!(
        projected.payload["result"]
            .get("keptHeadUserMessageCount")
            .is_none()
    );
}

#[test]
fn should_project_a_compaction_blocked_to_event_compaction_blocked_when_event_is_compaction_blocked()
 {
    // Given a compaction blocked event without a wire turn id (the engine
    // does not plumb one down to the turn loop)
    let event = SessionEvent::CompactionBlocked {
        agent_id: "main".to_string(),
        turn_id: None,
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.compaction.blocked carrying only the
    // agent's attribution — turnId is optional on the protocol type and stays
    // absent
    assert_eq!(projected.method, "event.compaction.blocked");
    assert!(!projected.volatile);
    assert_eq!(projected.payload, serde_json::json!({ "agentId": "main" }));
}

#[test]
fn should_carry_the_turn_id_when_a_compaction_blocked_event_has_one() {
    // Given a compaction blocked event carrying a wire turn id
    let event = SessionEvent::CompactionBlocked {
        agent_id: "main".to_string(),
        turn_id: Some("turn-abc".to_string()),
    };

    // When projecting it
    let projected = projected(&event);

    // Then the payload carries it under turnId (a future turn-loop that
    // plumbs the wire id would surface it without a projection change)
    assert_eq!(projected.payload["turnId"], "turn-abc");
}

#[test]
fn should_omit_turn_id_from_the_serialised_form_when_a_compaction_blocked_has_none() {
    // Given a compaction blocked event without a wire turn id
    let event = SessionEvent::CompactionBlocked {
        agent_id: "main".to_string(),
        turn_id: None,
    };

    // When serde-serialising the event itself (the ws relay serialises
    // SessionEvent directly, without going through the projection)
    let json = serde_json::to_value(&event).expect("event serialises");

    // Then the wire type tag is present and the absent turn id stays absent
    // (no null placeholder on the relayed frame)
    assert_eq!(json["type"], "compaction_blocked");
    assert!(json.get("turn_id").is_none());
}

#[test]
fn should_project_a_hook_result_to_event_hook_result_when_event_is_hook_result() {
    // Given a passing hook's rendered result (no block)
    let event = SessionEvent::HookResult {
        hook_event: "UserPromptSubmit".to_string(),
        content: "hook says hi".to_string(),
        blocked: None,
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.hook.result with the camelCase payload —
    // durable (the result is rendered into the conversation context) and
    // the absent blocked flag stays absent (no null placeholder)
    assert_eq!(projected.method, "event.hook.result");
    assert!(!projected.volatile);
    assert_eq!(projected.payload["hookEvent"], "UserPromptSubmit");
    assert_eq!(projected.payload["content"], "hook says hi");
    assert!(projected.payload.get("blocked").is_none());
}

#[test]
fn should_carry_blocked_when_a_hook_result_event_has_one() {
    // Given a blocking hook's rendered result
    let event = SessionEvent::HookResult {
        hook_event: "UserPromptSubmit".to_string(),
        content: "you shall not pass".to_string(),
        blocked: Some(true),
    };

    // When projecting it
    let projected = projected(&event);

    // Then the payload carries the blocked flag
    assert_eq!(projected.method, "event.hook.result");
    assert_eq!(projected.payload["blocked"], serde_json::json!(true));
}

#[test]
fn should_omit_blocked_from_the_serialised_form_when_a_hook_result_has_none() {
    // Given a passing hook result event
    let event = SessionEvent::HookResult {
        hook_event: "UserPromptSubmit".to_string(),
        content: "hook says hi".to_string(),
        blocked: None,
    };

    // When serde-serialising the event itself (the ws relay serialises
    // SessionEvent directly, without going through the projection)
    let json = serde_json::to_value(&event).expect("event serialises");

    // Then the wire type tag is present and the absent blocked flag stays
    // absent (no null placeholder on the relayed frame)
    assert_eq!(json["type"], "hook_result");
    assert!(json.get("blocked").is_none());
}

#[test]
fn should_project_turn_started_with_agent_attribution() {
    // Given a turn_started event carrying the agent's wire identity
    let event = SessionEvent::TurnStarted {
        agent_id: "main".to_string(),
        session_id: "s-1".to_string(),
        turn_id: "turn-1".to_string(),
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.turn.started carrying the agentId the
    // web routes member rows by
    assert_eq!(projected.method, "event.turn.started");
    assert_eq!(projected.payload["agentId"], "main");
    assert_eq!(projected.payload["turnId"], "turn-1");
}

#[test]
fn should_project_turn_completed_with_agent_attribution() {
    // Given a turn_completed event carrying a subagent's wire identity
    let event = SessionEvent::TurnCompleted {
        agent_id: "agent-0192-aaaa".to_string(),
        session_id: "s-1".to_string(),
        turn_id: "turn-1".to_string(),
        stop_reason: "completed".to_string(),
        wire_turn_id: 1,
        duration_ms: None,
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.turn.ended carrying the agentId the
    // web routes member rows by plus the stop reason under `reason`
    assert_eq!(projected.method, "event.turn.ended");
    assert_eq!(projected.payload["agentId"], "agent-0192-aaaa");
    assert_eq!(projected.payload["reason"], "completed");
}

#[test]
fn should_project_turn_completed_with_the_server_measured_duration_when_the_event_carries_it() {
    // Given a turn_completed event whose engine measured the turn's duration
    // (the same wall-clock the wire `turn.ended` record carries — a client
    // that resynced mid-turn has no local anchor to estimate it from)
    let event = SessionEvent::TurnCompleted {
        agent_id: "main".to_string(),
        session_id: "s-1".to_string(),
        turn_id: "turn-1".to_string(),
        stop_reason: "completed".to_string(),
        wire_turn_id: 1,
        duration_ms: Some(90_324),
    };

    // When projecting it
    let projected = projected(&event);

    // Then the event.turn.ended payload carries the server's authoritative
    // durationMs, so a live completion renders "Worked for X" instead of a
    // naked "Worked" after a resync dropped the client's start anchor
    assert_eq!(projected.method, "event.turn.ended");
    assert_eq!(projected.payload["durationMs"], 90_324);
}

#[test]
fn should_omit_duration_ms_from_the_projection_when_the_completed_turn_carries_none() {
    // Given a turn_completed event with no measured duration (engines that
    // predate the field, or a turn that ended without a TurnEndResult)
    let event = SessionEvent::TurnCompleted {
        agent_id: "main".to_string(),
        session_id: "s-1".to_string(),
        turn_id: "turn-1".to_string(),
        stop_reason: "completed".to_string(),
        wire_turn_id: 1,
        duration_ms: None,
    };

    // When projecting it
    let projected = projected(&event);

    // Then the event.turn.ended payload omits the key entirely (absent, not
    // null) so legacy consumers keep their previous behavior
    assert_eq!(projected.method, "event.turn.ended");
    assert!(
        projected.payload.get("durationMs").is_none(),
        "payload: {}",
        projected.payload
    );
}

#[test]
fn should_project_compaction_events_with_agent_attribution() {
    // Given the compaction lifecycle events carrying the agent's wire
    // identity
    for event in [
        SessionEvent::CompactionStarted {
            agent_id: "main".to_string(),
            trigger: CompactionSource::Manual,
            instruction: None,
        },
        SessionEvent::CompactionCancelled {
            agent_id: "main".to_string(),
        },
        SessionEvent::CompactionBlocked {
            agent_id: "main".to_string(),
            turn_id: None,
        },
        SessionEvent::CompactionCompleted {
            agent_id: "main".to_string(),
            result: CompactionResult {
                applied: true,
                summary: "the summary".to_string(),
                context_summary: "context: the summary".to_string(),
                compacted_count: 7,
                tokens_before: 1000,
                tokens_after: 120,
                dropped_count: 2,
                dropped_entry_positions: Vec::new(),
                dropped_entry_seqs: Vec::new(),
                retained_tail_count: 0,
                kept_head_user_message_count: 0,
                elision_message: None,
                first_replaced_entry_seq: None,
                last_replaced_entry_seq: None,
            },
        },
    ] {
        // When projecting it
        let projected = projected(&event);

        // Then the payload carries the agentId the web routes member rows by
        assert_eq!(
            projected.payload["agentId"], "main",
            "payload: {}",
            projected.payload
        );
    }
}

#[test]
fn should_project_turn_failed_to_event_turn_ended_with_failed_reason_when_event_is_turn_failed() {
    // Given a turn_failed event whose engine error carries its domain code
    // and whose agent is identified by its wire id (the same string identity
    // every other session event carries — "main" for the main agent)
    let event = SessionEvent::TurnFailed {
        agent_id: "main".to_string(),
        turn_id: "turn-1".to_string(),
        error: "provider rejected the request".to_string(),
        error_code: "provider.api_error".to_string(),
        wire_turn_id: 0,
    };

    // When projecting it
    let projected = projected(&event);

    // Then it folds into the terminal event.turn.ended method with
    // reason "failed" and a structured error object carrying the real
    // domain code — not a hardcoded internal — plus its retryability
    // verdict. The payload's agentId must stay the agent's wire identity:
    // clients (the web projector) route turn.ended frames by it — an id
    // that is not the main agent's makes them treat the frame as a
    // subagent's and leaves the main turn's UI stuck "thinking"
    assert_eq!(projected.method, "event.turn.ended");
    assert!(!projected.volatile);
    assert_eq!(projected.payload["agentId"], "main");
    assert_eq!(projected.payload["turnId"], "turn-1");
    assert_eq!(projected.payload["reason"], "failed");
    assert_eq!(projected.payload["error"]["code"], "provider.api_error");
    assert_eq!(
        projected.payload["error"]["message"],
        "provider rejected the request"
    );
    assert_eq!(projected.payload["error"]["retryable"], false);
}

#[test]
fn should_project_the_retryable_verdict_when_a_failed_turn_reports_context_overflow() {
    // Given a turn_failed event whose engine error is context overflow —
    // the one domain code the metadata table marks retryable
    let event = SessionEvent::TurnFailed {
        agent_id: "main".to_string(),
        turn_id: "turn-2".to_string(),
        error: "context overflow after compaction".to_string(),
        error_code: "context.overflow".to_string(),
        wire_turn_id: 0,
    };

    // When projecting it
    let projected = projected(&event);

    // Then the structured error object reports retryable true, so clients
    // can offer a compact-and-resend action instead of a dead end
    assert_eq!(projected.payload["error"]["code"], "context.overflow");
    assert_eq!(projected.payload["error"]["retryable"], true);
}

#[test]
fn should_project_tool_call_started_to_event_tool_call_started_when_event_is_tool_call_started() {
    // Given a tool_call_started event
    let event = SessionEvent::ToolCallStarted {
        agent_id: "main".to_string(),
        session_id: "session-1".to_string(),
        tool_name: "bash".to_string(),
        tool_call_id: "call-1".to_string(),
        args: serde_json::json!({ "command": "cargo test" }),
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.tool.call.started — durable (its result
    // counterpart must replay so reconnecting clients see finished tool
    // calls), with the tool name under `name`, the full argument object
    // under `args` (the TS protocol's field), and no payload sessionId
    assert_eq!(projected.method, "event.tool.call.started");
    assert!(!projected.volatile);
    assert_eq!(projected.payload["toolCallId"], "call-1");
    assert_eq!(projected.payload["name"], "bash");
    assert_eq!(projected.payload["args"]["command"], "cargo test");
    assert!(projected.payload.get("sessionId").is_none());
}

/// Given a tool event emitted under a member agent's relay, When projected to
/// the wire, Then the frame carries the member's agentId — the field the web
/// routes member tool frames by (supervision P2+1; the pre-fix frames were
/// indistinguishable from the main agent's and leaked into its transcript).
#[test]
fn should_project_the_agent_id_on_tool_frames() {
    for event in [
        SessionEvent::ToolCallStarted {
            agent_id: "agent-34d6376b-c9ed-4555-9f99-2256e024348d".to_string(),
            session_id: "session-1".to_string(),
            tool_name: "Read".to_string(),
            tool_call_id: "call-1".to_string(),
            args: serde_json::json!({ "file_path": "note.txt" }),
        },
        SessionEvent::ToolCallCompleted {
            agent_id: "agent-34d6376b-c9ed-4555-9f99-2256e024348d".to_string(),
            session_id: "session-1".to_string(),
            tool_call_id: "call-1".to_string(),
            tool_name: "Read".to_string(),
            is_error: false,
            output: mirri_core::tool::ToolOutput::text("ok"),
        },
        SessionEvent::ToolProgress {
            agent_id: "agent-34d6376b-c9ed-4555-9f99-2256e024348d".to_string(),
            tool_call_id: "call-1".to_string(),
            update: mirri_core::tool::ToolUpdate {
                kind: mirri_core::tool::ToolUpdateKind::Status,
                text: Some("working".to_string()),
                percent: None,
                custom_kind: None,
                custom_data: None,
            },
        },
    ] {
        let projected =
            mirri_core::session::event_projection::project(&event).expect("tool events project");
        let payload = projected.payload;
        assert_eq!(
            payload.get("agentId").and_then(serde_json::Value::as_str),
            Some("agent-34d6376b-c9ed-4555-9f99-2256e024348d"),
            "agentId missing on {}: {:?}",
            projected.method,
            payload
        );
    }
}

#[test]
fn should_project_tool_call_completed_to_event_tool_result_when_event_is_tool_call_completed() {
    // Given a tool_call_completed event
    let event = SessionEvent::ToolCallCompleted {
        agent_id: "main".to_string(),
        session_id: "session-1".to_string(),
        tool_call_id: "call-1".to_string(),
        tool_name: "bash".to_string(),
        is_error: true,
        output: mirri_core::ToolOutput::Text("boom: exit 1".to_string()),
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.tool.result — durable so replay shows
    // finished tool calls — with the tool's name (a consumer that missed
    // the started event still attributes the result to its tool), the
    // result's output text, and no payload sessionId
    assert_eq!(projected.method, "event.tool.result");
    assert!(!projected.volatile);
    assert_eq!(projected.payload["toolCallId"], "call-1");
    assert_eq!(projected.payload["name"], "bash");
    assert_eq!(projected.payload["isError"], true);
    assert_eq!(projected.payload["output"], "boom: exit 1");
    assert!(projected.payload.get("sessionId").is_none());
}

#[test]
fn should_project_turn_steps_with_the_wire_turn_id_the_started_event_carries() {
    // Given a turn whose steps publish under the wire turn id (`turn-<uuid>`)
    // — the same key `turn.started` / `turn.ended` carry
    let started = SessionEvent::TurnStepStarted {
        turn_id: "turn-1".to_string(),
        step: 1,
    };
    let completed = SessionEvent::TurnStepCompleted {
        turn_id: "turn-1".to_string(),
        step: 1,
        input_tokens: 10,
        output_tokens: 5,
        cache_read_tokens: 2,
        cache_creation_tokens: 1,
        model: None,
    };

    // When projecting both
    let started_projected = projected(&started);
    let completed_projected = projected(&completed);

    // Then each payload carries the wire turn id under `turnId` — the
    // turn.step family routes by the same key the turn boundary events use
    assert_eq!(started_projected.method, "event.turn.step.started");
    assert_eq!(started_projected.payload["turnId"], "turn-1");
    assert_eq!(started_projected.payload["step"], 1);
    assert_eq!(completed_projected.method, "event.turn.step.completed");
    assert_eq!(completed_projected.payload["turnId"], "turn-1");
    assert_eq!(completed_projected.payload["step"], 1);
}

/// Given a turn step recorded under a model alias, when the step-completed
/// event is projected, then the wire payload carries the model alongside
/// the four usage buckets (turn-level usage attribution, ADR-002).
#[test]
fn should_carry_the_model_on_turn_step_completed_when_projected() {
    let event = SessionEvent::TurnStepCompleted {
        turn_id: "turn-1".to_string(),
        step: 1,
        input_tokens: 10,
        output_tokens: 5,
        cache_read_tokens: 200,
        cache_creation_tokens: 0,
        model: Some("expert".to_string()),
    };
    let projected = projected(&event);
    assert_eq!(projected.payload["model"], serde_json::json!("expert"));
}

/// Given a turn step with no model attribution, when the step-completed
/// event is projected, then the wire payload omits the `model` key —
/// no invented fallback value rides the event.
#[test]
fn should_omit_the_model_key_on_turn_step_completed_without_attribution() {
    let event = SessionEvent::TurnStepCompleted {
        turn_id: "turn-1".to_string(),
        step: 1,
        input_tokens: 10,
        output_tokens: 5,
        cache_read_tokens: 200,
        cache_creation_tokens: 0,
        model: None,
    };
    let projected = projected(&event);
    assert!(projected.payload.get("model").is_none());
}

#[test]
fn should_emit_usage_in_the_ts_client_shape_when_projecting_usage_events() {
    // Given a completed turn step and a completed subagent, each carrying
    // the engine's four raw token buckets (the counters the on-disk
    // `usage.record` keeps under the Rust spellings — ADR-012 pins those
    // on disk and assigns the TS client spellings to this projection layer)
    let step = SessionEvent::TurnStepCompleted {
        turn_id: "turn-1".to_string(),
        step: 1,
        input_tokens: 10,
        output_tokens: 5,
        cache_read_tokens: 2,
        cache_creation_tokens: 1,
        model: None,
    };
    let subagent = SessionEvent::SubagentCompleted {
        subagent_id: "agent-1".to_string(),
        result_summary: "done".to_string(),
        input_tokens: 10,
        output_tokens: 5,
        cache_read_tokens: 2,
        cache_creation_tokens: 1,
    };

    // When wrapped in the client notification envelope
    let step_notification = event_projection::event_notification(
        &step,
        "session-1",
        Some(7),
        Some("epoch-1"),
        chrono::Utc::now(),
    )
    .expect("a completed step projects a notification");
    let subagent_notification = event_projection::event_notification(
        &subagent,
        "session-1",
        Some(8),
        Some("epoch-1"),
        chrono::Utc::now(),
    )
    .expect("a completed subagent projects a notification");

    // Then each `usage` object carries exactly the TS client TokenUsage
    // spellings — the keys the TS consumers read (kap-server's
    // `tokenUsageSchema` for `turn.step.completed`/`subagent.completed`
    // and the web agentEventProjector's `normalizeUsage`, which has no
    // `inputTokens` fallback)
    let expected = serde_json::json!({
        "inputOther": 10,
        "output": 5,
        "inputCacheRead": 2,
        "inputCacheCreation": 1,
    });
    assert_eq!(step_notification.method, "event.turn.step.completed");
    assert_eq!(step_notification.params["usage"], expected);
    assert_eq!(subagent_notification.method, "event.subagent.completed");
    assert_eq!(subagent_notification.params["usage"], expected);

    // And no on-disk Rust spelling leaks into the client shape
    for params in [&step_notification.params, &subagent_notification.params] {
        for rust_spelling in [
            "inputTokens",
            "outputTokens",
            "cacheReadTokens",
            "cacheCreationTokens",
        ] {
            assert!(
                params["usage"].get(rust_spelling).is_none(),
                "the client usage shape must not carry the on-disk key `{rust_spelling}`"
            );
        }
    }

    // And the envelope carries the client-visible timestamp (the epoch-ms
    // `time` of the on-disk usage.record row stays on disk — ADR-011;
    // the client event contract carries no `model`/`usageScope` keys)
    assert!(
        step_notification.params["timestamp"]
            .as_str()
            .is_some_and(|stamp| !stamp.is_empty()),
        "the notification envelope carries an RFC 3339 timestamp"
    );
}

#[test]
fn should_project_step_interruption_and_retry_with_the_wire_turn_id_when_carried() {
    // Given an interrupted and a retrying step, both carrying the wire turn
    // id the relay callback captured at dispatch
    let interrupted = SessionEvent::TurnStepInterrupted {
        turn_id: Some("turn-1".to_string()),
        step: 2,
        reason: "aborted".to_string(),
        message: Some("cancelled by user".to_string()),
    };
    let retrying = SessionEvent::TurnStepRetrying {
        turn_id: Some("turn-1".to_string()),
        step: 2,
        failed_attempt: 1,
        next_attempt: 2,
        max_attempts: 3,
        delay_ms: 0,
    };

    // When projecting both
    let interrupted_projected = projected(&interrupted);
    let retrying_projected = projected(&retrying);

    // Then the protocol-required `turnId` is present on the wire (the
    // pre-alignment spelling omitted it)
    assert_eq!(interrupted_projected.method, "event.turn.step.interrupted");
    assert_eq!(interrupted_projected.payload["turnId"], "turn-1");
    assert_eq!(interrupted_projected.payload["reason"], "aborted");
    assert_eq!(retrying_projected.method, "event.turn.step.retrying");
    assert_eq!(retrying_projected.payload["turnId"], "turn-1");
    assert_eq!(retrying_projected.payload["failedAttempt"], 1);

    // And a callback built without a turn id still projects — the field
    // stays absent rather than null (the optional spelling)
    let bare = SessionEvent::TurnStepInterrupted {
        turn_id: None,
        step: 2,
        reason: "aborted".to_string(),
        message: None,
    };
    let bare_projected = projected(&bare);
    assert!(bare_projected.payload.get("turnId").is_none());
}

#[test]
fn should_project_tool_progress_to_event_tool_progress_when_event_is_tool_progress() {
    // Given a tool_progress event carrying a percent update
    let event = SessionEvent::ToolProgress {
        agent_id: "main".to_string(),
        tool_call_id: "call-1".to_string(),
        update: mirri_core::tool::ToolUpdate {
            kind: mirri_core::tool::ToolUpdateKind::Progress,
            text: Some("halfway".to_string()),
            percent: Some(50.0),
            custom_kind: None,
            custom_data: None,
        },
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.tool.progress — volatile per the
    // protocol's VOLATILE_EVENT_TYPES (no seq stamp, never replayed) — with
    // the update under `update` and no payload sessionId or turnId
    assert_eq!(projected.method, "event.tool.progress");
    assert!(projected.volatile);
    assert_eq!(projected.payload["toolCallId"], "call-1");
    assert_eq!(projected.payload["update"]["kind"], "progress");
    assert_eq!(projected.payload["update"]["text"], "halfway");
    assert_eq!(projected.payload["update"]["percent"], 50.0);
    assert!(projected.payload.get("sessionId").is_none());
    assert!(projected.payload.get("turnId").is_none());
    assert!(projected.payload["update"].get("customKind").is_none());
}

/// A [`mirri_core::UsageStatus`] fixture with one model bucket, shared by
/// the agent status projection tests.
fn test_usage_status() -> mirri_core::UsageStatus {
    use mirri_core::usage::TokenUsage;
    use std::collections::HashMap;

    let tokens = TokenUsage {
        input_tokens: 100,
        output_tokens: 20,
        cache_read_tokens: 5,
        cache_creation_tokens: 1,
    };
    mirri_core::UsageStatus {
        by_model: HashMap::from([("kimi-k2".to_string(), tokens.clone())]),
        current_turn: tokens.clone(),
        total: tokens,
        first_call: None,
    }
}

#[test]
fn should_project_agent_status_updated_with_structured_payload() {
    // Given an agent_status_updated event carrying the full structured
    // snapshot (phase, model, context, plan/swarm flags, usage)
    use mirri_core::session::agent_status::AgentPhase;

    let event = SessionEvent::AgentStatusUpdated {
        agent_id: "main".to_string(),
        phase: AgentPhase::Running,
        model: Some("kimi-k2".to_string()),
        thinking_effort: None,
        context_tokens: Some(120_000),
        max_context_tokens: Some(200_000),
        plan_mode: Some(false),
        swarm_mode: Some(false),
        usage: Some(test_usage_status()),
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.agent.status.updated with the
    // structured camelCase payload — key-by-key the protocol
    // AgentStatusUpdatedEvent shape — and it is volatile, aligned with
    // the protocol's VOLATILE_EVENT_TYPES
    assert_eq!(projected.method, "event.agent.status.updated");
    assert!(projected.volatile);
    assert_eq!(projected.payload["agentId"], serde_json::json!("main"));
    assert_eq!(projected.payload["phase"], serde_json::json!("running"));
    assert_eq!(projected.payload["model"], serde_json::json!("kimi-k2"));
    assert_eq!(
        projected.payload["contextTokens"],
        serde_json::json!(120_000)
    );
    assert_eq!(
        projected.payload["maxContextTokens"],
        serde_json::json!(200_000)
    );
    assert_eq!(projected.payload["contextUsage"], serde_json::json!(0.6));
    assert_eq!(projected.payload["planMode"], serde_json::json!(false));
    assert_eq!(projected.payload["swarmMode"], serde_json::json!(false));
    assert!(
        projected.payload.get("thinkingEffort").is_none(),
        "an absent optional key must not appear as null"
    );
    assert!(
        projected.payload.get("previousStatus").is_none(),
        "the TS v2 slice shape carries no previousStatus key"
    );

    // And the usage object uses the TS TokenUsage bucket spelling
    // (inputOther/output/inputCacheRead/inputCacheCreation — the same
    // dialect event.turn.step.completed's usage object speaks)
    assert_eq!(
        projected.payload["usage"]["total"]["inputOther"],
        serde_json::json!(100)
    );
    assert_eq!(
        projected.payload["usage"]["byModel"]["kimi-k2"]["output"],
        serde_json::json!(20)
    );
    assert_eq!(
        projected.payload["usage"]["currentTurn"]["inputCacheRead"],
        serde_json::json!(5)
    );
}

#[test]
fn should_project_a_custom_thinking_effort_verbatim_when_agent_status_updated() {
    // Given an agent_status_updated event carrying a model-declared custom
    // effort (the open vocabulary's escape hatch — not one of the four
    // canonical levels)
    use mirri_core::session::agent_status::AgentPhase;

    let event = SessionEvent::AgentStatusUpdated {
        agent_id: "main".to_string(),
        phase: AgentPhase::Running,
        model: Some("kimi-k2".to_string()),
        thinking_effort: Some(mirri_core::ThinkingEffort::Custom("max".to_string())),
        context_tokens: None,
        max_context_tokens: None,
        plan_mode: None,
        swarm_mode: None,
        usage: None,
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event carries the custom spelling verbatim as
    // thinkingEffort — never folded to a canonical level nor dropped
    assert_eq!(projected.method, "event.agent.status.updated");
    assert_eq!(
        projected.payload["thinkingEffort"],
        serde_json::json!("max")
    );
}

#[test]
fn should_omit_optional_status_fields_entirely_when_absent() {
    // Given an agent_status_updated event with only the identity and the
    // phase (every optional field absent)
    use mirri_core::session::agent_status::AgentPhase;

    let event = SessionEvent::AgentStatusUpdated {
        agent_id: "main".to_string(),
        phase: AgentPhase::Idle,
        model: None,
        thinking_effort: None,
        context_tokens: None,
        max_context_tokens: None,
        plan_mode: None,
        swarm_mode: None,
        usage: None,
    };

    // When projecting it
    let projected = projected(&event);

    // Then the payload carries only agentId and phase — no null keys, no
    // status/previousStatus debug-placeholder residue
    assert_eq!(projected.method, "event.agent.status.updated");
    let object = projected
        .payload
        .as_object()
        .expect("the payload is a JSON object");
    assert_eq!(
        object.len(),
        2,
        "only agentId and phase may be present, got {object:?}"
    );
    assert_eq!(projected.payload["agentId"], serde_json::json!("main"));
    assert_eq!(projected.payload["phase"], serde_json::json!("idle"));
}

#[test]
fn should_project_tool_approval_requested_to_event_approval_requested_when_event_is_tool_approval_requested()
 {
    // Given a tool_approval_requested event
    let request_id = Uuid::new_v4();
    let event = SessionEvent::ToolApprovalRequested {
        agent_id: Uuid::nil(),
        tool_name: "bash".to_string(),
        request_id,
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.approval.requested with the request payload
    assert_eq!(projected.method, "event.approval.requested");
    assert!(!projected.volatile);
    assert_eq!(projected.payload["agentId"], serde_json::json!(Uuid::nil()));
    assert_eq!(projected.payload["toolName"], "bash");
    assert_eq!(
        projected.payload["requestId"],
        serde_json::json!(request_id)
    );
}

#[test]
fn should_project_tool_approval_resolved_to_event_approval_resolved_when_event_is_tool_approval_resolved()
 {
    // Given a tool_approval_resolved event
    let event = SessionEvent::ToolApprovalResolved {
        request_id: Uuid::new_v4(),
        approved: false,
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.approval.resolved with the outcome
    assert_eq!(projected.method, "event.approval.resolved");
    assert!(!projected.volatile);
    assert!(projected.payload["requestId"].is_string());
    assert_eq!(projected.payload["approved"], false);
}

#[test]
fn should_project_question_asked_to_event_question_requested_when_event_is_question_asked() {
    // Given a question_asked event
    let request_id = Uuid::new_v4();
    let event = SessionEvent::QuestionAsked {
        agent_id: Uuid::nil(),
        question: "Deploy now?".to_string(),
        request_id,
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.question.requested with the question
    assert_eq!(projected.method, "event.question.requested");
    assert!(!projected.volatile);
    assert_eq!(projected.payload["agentId"], serde_json::json!(Uuid::nil()));
    assert_eq!(
        projected.payload["requestId"],
        serde_json::json!(request_id)
    );
    assert_eq!(projected.payload["question"], "Deploy now?");
}

#[test]
fn should_project_question_answered_to_event_question_answered_when_event_is_question_answered() {
    // Given a question_answered event
    let event = SessionEvent::QuestionAnswered {
        request_id: Uuid::new_v4(),
        answer: "yes".to_string(),
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.question.answered with the answer
    assert_eq!(projected.method, "event.question.answered");
    assert!(!projected.volatile);
    assert!(projected.payload["requestId"].is_string());
    assert_eq!(projected.payload["answer"], "yes");
}

#[test]
fn should_project_question_dismissed_to_event_question_dismissed_when_event_is_question_dismissed()
{
    // Given a question_dismissed event
    let request_id = Uuid::new_v4();
    let event = SessionEvent::QuestionDismissed { request_id };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.question.dismissed (durable, so clients
    // that reconnect learn the question was cancelled) carrying only the
    // requestId — no fabricated dismissal metadata
    assert_eq!(projected.method, "event.question.dismissed");
    assert!(!projected.volatile);
    assert_eq!(
        projected.payload["requestId"],
        serde_json::json!(request_id)
    );
    let keys: Vec<&str> = projected
        .payload
        .as_object()
        .expect("payload is an object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["requestId"]);
}

#[test]
fn should_project_session_resumed_and_forked_to_event_session_updated_when_session_changes() {
    // Given resume and fork events
    let new_session_id = Uuid::new_v4();
    let resumed = SessionEvent::SessionResumed {
        session_id: Uuid::nil(),
    };
    let forked = SessionEvent::SessionForked {
        source_session_id: Uuid::nil(),
        new_session_id,
    };

    // When projecting them
    let resumed_projected = projected(&resumed);
    let forked_projected = projected(&forked);

    // Then both map to event.session.updated with their change markers;
    // the fork's new session rides under `newSessionId` so it never
    // collides with the envelope's `sessionId`
    assert_eq!(resumed_projected.method, "event.session.updated");
    assert_eq!(resumed_projected.payload["change"], "resumed");
    assert_eq!(forked_projected.method, "event.session.updated");
    assert_eq!(forked_projected.payload["change"], "forked");
    assert_eq!(
        forked_projected.payload["sourceSessionId"],
        serde_json::json!(Uuid::nil())
    );
    assert_eq!(
        forked_projected.payload["newSessionId"],
        serde_json::json!(new_session_id)
    );
    assert!(
        forked_projected.payload.get("sessionId").is_none(),
        "the fork payload must not use the envelope's reserved sessionId key"
    );
}

#[test]
fn should_project_session_closed_to_event_session_deleted_when_event_is_session_closed() {
    // Given a session_closed event
    let event = SessionEvent::SessionClosed {
        session_id: Uuid::nil(),
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.session.deleted with the closure reason
    // (hardcoded `user_initiated` until the reason vocabulary widens, F3)
    assert_eq!(projected.method, "event.session.deleted");
    assert!(!projected.volatile);
    assert_eq!(projected.payload["reason"], "user_initiated");
}

#[test]
fn should_project_agent_created_to_event_agent_created_when_event_is_agent_created() {
    // Given an agent_created event
    let event = SessionEvent::AgentCreated {
        agent_id: Uuid::nil(),
        session_id: Uuid::nil(),
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.agent.created with only the agent id —
    // the session identity lives in the envelope
    assert_eq!(projected.method, "event.agent.created");
    assert!(!projected.volatile);
    assert_eq!(projected.payload["agentId"], serde_json::json!(Uuid::nil()));
    assert!(projected.payload.get("sessionId").is_none());
}

#[test]
fn should_project_agent_removed_to_event_agent_removed_when_event_is_agent_removed() {
    // Given an agent_removed event
    let event = SessionEvent::AgentRemoved {
        agent_id: Uuid::nil(),
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.agent.removed with the agent id
    assert_eq!(projected.method, "event.agent.removed");
    assert!(!projected.volatile);
    assert_eq!(projected.payload["agentId"], serde_json::json!(Uuid::nil()));
}

#[test]
fn should_return_none_when_event_is_unknown() {
    // Given an Unknown event
    let event = SessionEvent::Unknown;

    // When projecting it
    let projection = event_projection::project(&event);

    // Then there is no wire projection
    assert!(projection.is_none());
}

#[test]
fn should_cover_the_named_variants_with_forty_six_wire_methods_when_enumerated() {
    // Given every named SessionEvent variant
    let variants = all_variants();

    // When collecting the distinct methods they project onto
    let mut methods: Vec<&str> = variants
        .iter()
        .filter_map(|event| event_projection::project(event).map(|p| p.method))
        .collect();
    methods.sort_unstable();
    methods.dedup();

    // Then exactly the forty-five protocol methods are covered (the
    // variant count minus Unknown, with turn completed/failed and session
    // resumed/forked sharing their terminal methods)
    assert_eq!(
        methods,
        [
            "event.agent.created",
            "event.agent.removed",
            "event.agent.status.updated",
            "event.approval.requested",
            "event.approval.resolved",
            "event.assistant.delta",
            "event.background.task.started",
            "event.background.task.terminated",
            "event.compaction.blocked",
            "event.compaction.cancelled",
            "event.compaction.completed",
            "event.compaction.started",
            "event.cron.fired",
            "event.goal.updated",
            "event.hook.result",
            "event.mcp.server.status",
            "event.message.received",
            "event.plugin_command.activated",
            "event.question.answered",
            "event.question.dismissed",
            "event.question.requested",
            "event.session.deleted",
            "event.session.meta.updated",
            "event.session.model_switched",
            "event.session.updated",
            "event.session.work_changed",
            "event.shell.completed",
            "event.shell.output",
            "event.shell.started",
            "event.skill.activated",
            "event.subagent.completed",
            "event.subagent.failed",
            "event.subagent.spawned",
            "event.subagent.started",
            "event.subagent.suspended",
            "event.thinking.delta",
            "event.tool.call.delta",
            "event.tool.call.started",
            "event.tool.progress",
            "event.tool.result",
            "event.turn.ended",
            "event.turn.started",
            "event.turn.step.completed",
            "event.turn.step.interrupted",
            "event.turn.step.retrying",
            "event.turn.step.started",
        ]
    );
}

#[test]
fn should_project_the_variant_set_onto_exactly_the_projected_event_methods() {
    // Given one instance of every SessionEvent variant
    let variants = all_variants();

    // When projecting each (Unknown is the only variant without a
    // projection — asserted inline) and collecting the distinct methods
    let mut methods: Vec<&str> = Vec::new();
    for event in &variants {
        let projection = event_projection::project(event);
        assert_eq!(
            projection.is_none(),
            matches!(event, SessionEvent::Unknown),
            "only Unknown lacks a wire projection, got {event:?}"
        );
        if let Some(projected) = projection {
            methods.push(projected.method);
        }
    }
    methods.sort_unstable();
    methods.dedup();

    // Then the method set equals the exported closed vocabulary exactly — a
    // variant added without a PROJECTED_EVENT_METHODS entry (or a stale entry
    // without a projecting variant) fails here instead of drifting the
    // contract the TS suite reconciles against
    assert_eq!(methods, event_projection::PROJECTED_EVENT_METHODS.to_vec());
}

#[test]
fn should_keep_is_volatile_consistent_with_the_projection_for_every_variant() {
    // Given every SessionEvent variant
    let variants = all_variants();

    for event in &variants {
        // When asking whether it is volatile and projecting it
        // Then the two agree (Unknown is volatile and unprojected)
        assert_eq!(
            event_projection::is_volatile(event),
            event_projection::project(event).is_none_or(|p| p.volatile),
            "volatility disagrees with the projection for {event:?}"
        );
    }
}

#[test]
fn should_keep_payload_keys_off_the_reserved_envelope_keys_for_every_projected_event() {
    // Given every SessionEvent variant with a wire projection
    // (Unknown has none, so it is excluded from the enumeration)
    let variants: Vec<SessionEvent> = all_variants()
        .into_iter()
        .filter(|event| !matches!(event, SessionEvent::Unknown))
        .collect();

    for event in &variants {
        // When projecting it
        let projected = projected(event);

        // Then its payload keys never collide with the reserved envelope
        // keys (payloads are merged flat into the notification params)
        if let serde_json::Value::Object(payload) = &projected.payload {
            for key in payload.keys() {
                assert!(
                    !RESERVED_ENVELOPE_KEYS.contains(&key.as_str()),
                    "payload key {key} of {} collides with a reserved envelope key",
                    projected.method
                );
            }
        } else {
            panic!("payload of {} is not an object", projected.method);
        }
    }
}

#[test]
fn should_stamp_seq_and_epoch_on_durable_events_only_when_building_notifications() {
    // Given every SessionEvent variant with a wire projection, a seq, an
    // epoch, and a timestamp (Unknown has none, so it is excluded)
    let variants: Vec<SessionEvent> = all_variants()
        .into_iter()
        .filter(|event| !matches!(event, SessionEvent::Unknown))
        .collect();
    let timestamp = chrono::Utc::now();

    for event in &variants {
        // When building the notification envelope (the seq/epoch supplied
        // even for volatile events, as the server's forwarder does)
        let notification = event_projection::event_notification(
            event,
            "session-1",
            Some(42),
            Some("epoch-1"),
            timestamp,
        )
        .expect("projected event builds a notification");

        // Then the envelope always carries the session, volatility, and
        // timestamp...
        let params = &notification.params;
        assert_eq!(params["sessionId"], "session-1");
        assert_eq!(
            params["volatile"],
            event_projection::is_volatile(event),
            "envelope volatility matches the projection for {event:?}"
        );
        assert!(
            params["timestamp"]
                .as_str()
                .is_some_and(|stamp| !stamp.is_empty()),
            "the envelope carries an RFC 3339 timestamp"
        );

        // ...and seq/epoch appear on durable events only, even though the
        // caller supplied them for the volatile ones too
        if event_projection::is_volatile(event) {
            assert!(
                params.get("seq").is_none(),
                "volatile event {event:?} must not carry a seq"
            );
            assert!(
                params.get("epoch").is_none(),
                "volatile event {event:?} must not carry an epoch"
            );
        } else {
            assert_eq!(params["seq"], 42, "durable event {event:?} carries its seq");
            assert_eq!(params["epoch"], "epoch-1");
        }
    }
}

#[test]
fn should_return_none_when_building_a_notification_for_an_unknown_event() {
    // Given an Unknown event
    let event = SessionEvent::Unknown;

    // When building the notification envelope
    let notification = event_projection::event_notification(
        &event,
        "session-1",
        Some(42),
        Some("epoch-1"),
        chrono::Utc::now(),
    );

    // Then there is no notification to frame
    assert!(notification.is_none());
}

#[test]
fn should_map_each_turn_stop_reason_to_its_wire_string_when_asked() {
    // Given the seven turn stop reasons
    let cases = [
        (TurnStopReason::EndTurn, "completed"),
        (TurnStopReason::MaxTokens, "completed"),
        (TurnStopReason::Filtered, "filtered"),
        (TurnStopReason::Paused, "completed"),
        (TurnStopReason::Unknown, "completed"),
        (TurnStopReason::Aborted, "cancelled"),
        (TurnStopReason::BlockedByHook, "blocked"),
    ];

    for (reason, expected) in cases {
        // When mapping it to the wire string
        // Then it matches the TS kernel's turnStopReasonToWire vocabulary
        assert_eq!(
            event_projection::turn_stop_reason_wire(&reason),
            expected,
            "wire string for {reason:?}"
        );
    }
}

// --- background.task projection (design §7.8) -----------------------------

#[test]
fn should_project_background_task_started_to_event_background_task_started_with_info() {
    // Given a running background task's started event with a process-kind info
    let event = SessionEvent::BackgroundTaskStarted {
        info: BackgroundTaskInfo {
            kind: TaskKind::Process {
                command: Some("cargo build".to_string()),
                dispatcher: None,
            },
            task_id: "task-1".to_string(),
            description: "release build".to_string(),
            status: "running".to_string(),
            started_at: 1_700_000_000_000_i64,
            ended_at: None,
            stop_reason: None,
            agent_id: None,
            exit_code: None,
        },
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.background.task.started, durable, with
    // the info snapshot nested verbatim (the TS protocol's discriminated
    // union — kind discriminator in-band, optional fields absent)
    assert_eq!(projected.method, "event.background.task.started");
    assert!(!projected.volatile);
    assert_eq!(projected.payload["info"]["taskId"], "task-1");
    assert_eq!(projected.payload["info"]["kind"], "process");
    assert_eq!(projected.payload["info"]["command"], "cargo build");
    assert_eq!(projected.payload["info"]["status"], "running");
    assert_eq!(
        projected.payload["info"]["startedAt"],
        1_700_000_000_000_i64
    );
    assert_eq!(
        projected.payload["info"]["endedAt"],
        serde_json::Value::Null,
        "the required-nullable endedAt rides the started spelling as null"
    );
    assert!(projected.payload["info"].get("exitCode").is_none());
}

#[test]
fn should_project_background_task_terminated_with_terminal_info_when_event_is_terminated() {
    // Given a completed process task's terminated event
    let event = SessionEvent::BackgroundTaskTerminated {
        info: BackgroundTaskInfo {
            kind: TaskKind::Process {
                command: Some("cargo build".to_string()),
                dispatcher: None,
            },
            task_id: "task-1".to_string(),
            description: "release build".to_string(),
            status: "completed".to_string(),
            started_at: 1_700_000_000_000_i64,
            ended_at: Some(1_700_000_001_500_i64),
            stop_reason: None,
            agent_id: None,
            exit_code: Some(0),
        },
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.background.task.terminated, durable,
    // carrying the terminal snapshot (endedAt + exitCode present on the
    // completed process branch)
    assert_eq!(projected.method, "event.background.task.terminated");
    assert!(!projected.volatile);
    assert_eq!(projected.payload["info"]["status"], "completed");
    assert_eq!(projected.payload["info"]["endedAt"], 1_700_000_001_500_i64);
    assert_eq!(projected.payload["info"]["exitCode"], 0);
}

#[test]
fn should_project_the_shell_started_event_as_a_volatile_wire_event() {
    // Given a shell.started event carrying the command and task ids
    let event = SessionEvent::ShellStarted {
        command_id: "shell-1".to_string(),
        task_id: "task-3".to_string(),
    };

    // When projecting it
    let projected = event_projection::project(&event).expect("shell.started projects");

    // Then the wire method, volatility (the protocol's VOLATILE_EVENT_TYPES
    // lists shell.started), and camelCase payload match the TS shape
    assert_eq!(projected.method, "event.shell.started");
    assert!(projected.volatile);
    assert_eq!(projected.payload["commandId"], "shell-1");
    assert_eq!(projected.payload["taskId"], "task-3");
    assert!(event_projection::is_volatile(&event));
}

#[test]
fn should_project_the_shell_output_event_as_a_volatile_wire_event() {
    // Given a shell.output event streaming one stdout chunk
    let event = SessionEvent::ShellOutput {
        command_id: "shell-1".to_string(),
        update: mirri_core::tool::ToolUpdate {
            kind: mirri_core::tool::ToolUpdateKind::Stdout,
            text: Some("chunk".to_string()),
            percent: None,
            custom_kind: None,
            custom_data: None,
        },
        task_id: Some("task-3".to_string()),
    };

    // When projecting it
    let projected = event_projection::project(&event).expect("shell.output projects");

    // Then the chunk rides the camelCase ToolUpdate shape and the optional
    // task id stays present (never a null placeholder when absent)
    assert_eq!(projected.method, "event.shell.output");
    assert!(projected.volatile);
    assert_eq!(projected.payload["commandId"], "shell-1");
    assert_eq!(projected.payload["taskId"], "task-3");
    assert_eq!(projected.payload["update"]["kind"], "stdout");
    assert_eq!(projected.payload["update"]["text"], "chunk");
    assert!(event_projection::is_volatile(&event));

    // And the id-less spelling omits taskId instead of nulling it
    let idless = SessionEvent::ShellOutput {
        command_id: "shell-1".to_string(),
        update: mirri_core::tool::ToolUpdate {
            kind: mirri_core::tool::ToolUpdateKind::Stderr,
            text: Some("boom".to_string()),
            percent: None,
            custom_kind: None,
            custom_data: None,
        },
        task_id: None,
    };
    let projected = event_projection::project(&idless).expect("id-less shell.output projects");
    assert!(projected.payload.get("taskId").is_none());
}

#[test]
fn should_project_the_shell_completed_event_as_a_durable_wire_event() {
    // Given a shell.completed event settling a failed foreground run
    let event = SessionEvent::ShellCompleted {
        command_id: "shell-1".to_string(),
        is_error: true,
        task_id: None,
    };

    // When projecting it
    let projected = event_projection::project(&event).expect("shell.completed projects");

    // Then the settlement is durable (only the streaming pair is volatile)
    // and carries the TS camelCase payload
    assert_eq!(projected.method, "event.shell.completed");
    assert!(!projected.volatile);
    assert_eq!(projected.payload["commandId"], "shell-1");
    assert_eq!(projected.payload["isError"], true);
    assert!(projected.payload.get("taskId").is_none());
    assert!(!event_projection::is_volatile(&event));
}

#[test]
fn should_project_a_model_switched_event_with_from_and_to_payload() {
    // Given a ModelSwitched session event
    let event = SessionEvent::ModelSwitched {
        from: "kimi-k2".to_string(),
        to: "claude-sonnet-4".to_string(),
    };

    // When projecting it
    let projected = projected(&event);

    // Then the method carries the switch identity and the camelCase payload,
    // durable like the other session-scoped lifecycle events
    assert_eq!(projected.method, "event.session.model_switched");
    assert!(!projected.volatile);
    assert_eq!(projected.payload["from"], "kimi-k2");
    assert_eq!(projected.payload["to"], "claude-sonnet-4");
}

#[test]
fn should_preserve_info_when_background_task_events_roundtrip_through_the_session_event() {
    // Given the two background task lifecycle events carrying terminated
    // process- and agent-kind infos (the flatten + internally-tagged kind
    // combination the ws relay's `from_value::<SessionEvent>` consumers
    // deserialize)
    let events = [
        SessionEvent::BackgroundTaskStarted {
            info: BackgroundTaskInfo {
                kind: TaskKind::Process {
                    command: Some("cargo build".to_string()),
                    dispatcher: None,
                },
                task_id: "task-1".to_string(),
                description: "release build".to_string(),
                status: "running".to_string(),
                started_at: 1_700_000_000_000_i64,
                ended_at: None,
                stop_reason: None,
                agent_id: None,
                exit_code: None,
            },
        },
        SessionEvent::BackgroundTaskTerminated {
            info: BackgroundTaskInfo {
                kind: TaskKind::Agent {
                    agent_id: Some("agent-1".to_string()),
                    subagent_type: Some("coder".to_string()),
                    parent_tool_call_id: None,
                    swarm_index: None,
                    model: None,
                },
                task_id: "task-2".to_string(),
                description: "subagent".to_string(),
                status: "failed".to_string(),
                started_at: 1_700_000_000_000_i64,
                ended_at: Some(1_700_000_001_500_i64),
                stop_reason: Some("boom".to_string()),
                agent_id: None,
                exit_code: None,
            },
        },
    ];

    for event in events {
        // When serializing and deserializing
        let json = serde_json::to_string(&event).unwrap();
        let roundtrip: SessionEvent = serde_json::from_str(&json).unwrap();

        // Then the tagged variant and its info snapshot survive the
        // roundtrip field-for-field
        match (event, roundtrip) {
            (
                SessionEvent::BackgroundTaskStarted { info },
                SessionEvent::BackgroundTaskStarted { info: received },
            )
            | (
                SessionEvent::BackgroundTaskTerminated { info },
                SessionEvent::BackgroundTaskTerminated { info: received },
            ) => assert_eq!(received, info),
            (original, _) => panic!("variant changed across the roundtrip: {original:?}"),
        }
    }
}

#[test]
fn should_preserve_identity_when_a_tool_call_delta_roundtrips_through_the_session_event() {
    // Given a tool_call_delta carrying the optional tool name (the wire
    // relay's `from_value::<SessionEvent>` consumers deserialize this shape)
    let event = SessionEvent::ToolCallDelta {
        agent_id: "main".to_string(),
        tool_call_id: "call_1".to_string(),
        tool_name: Some("bash".to_string()),
        arguments_part: "{\"a\":".to_string(),
    };

    // When serializing and deserializing
    let json = serde_json::to_string(&event).unwrap();
    let roundtrip: SessionEvent = serde_json::from_str(&json).unwrap();

    // Then the tagged variant and its identity fields survive field-for-field
    match roundtrip {
        SessionEvent::ToolCallDelta {
            tool_call_id,
            tool_name,
            arguments_part,
            ..
        } => {
            assert_eq!(tool_call_id, "call_1");
            assert_eq!(tool_name.as_deref(), Some("bash"));
            assert_eq!(arguments_part, "{\"a\":");
        }
        other => panic!("variant changed across the roundtrip: {other:?}"),
    }
}

// ===========================================================================
// Subagent suspension — structured reason (ADR-011)
// ===========================================================================

/// Given a member suspension carrying the minted approval-gate ids, When it
/// is projected, Then the wire reason is the structured snake_case object
/// `{ kind: "approval", request_id, tool_call_id }` (ADR-011) — machine
/// readable for consumers that render the member's awaiting-user state.
#[test]
fn should_project_a_structured_approval_suspension_when_a_member_awaits_an_approval() {
    // Given a member suspension with a minted approval request
    let event = SessionEvent::SubagentSuspended {
        subagent_id: "agent-1".to_string(),
        reason: SuspendCause::Approval {
            request_id: "req-1".to_string(),
            tool_call_id: Some("call-bash".to_string()),
        },
    };

    // When projected onto the wire event
    let projected = projected(&event);

    // Then the payload carries the structured snake_case reason
    assert_eq!(projected.method, "event.subagent.suspended");
    assert!(!projected.volatile);
    assert_eq!(projected.payload["subagentId"], "agent-1");
    assert_eq!(
        projected.payload["reason"],
        serde_json::json!({
            "kind": "approval",
            "request_id": "req-1",
            "tool_call_id": "call-bash",
        })
    );
}

/// Given a rate-limit suspension, When it is projected, Then the wire
/// reason is the machine-readable `{ "kind": "rate_limit" }` object — the
/// legacy free-form string no longer appears on new suspensions.
#[test]
fn should_keep_rate_limit_suspension_reason_machine_readable() {
    // Given a rate-limit suspension
    let event = SessionEvent::SubagentSuspended {
        subagent_id: "agent-2".to_string(),
        reason: SuspendCause::RateLimit,
    };

    // When projected onto the wire event
    let projected = projected(&event);

    // Then the reason is the structured rate_limit object
    assert_eq!(projected.method, "event.subagent.suspended");
    assert_eq!(
        projected.payload["reason"],
        serde_json::json!({ "kind": "rate_limit" })
    );
}

/// Given a pre-ADR-011 journal row whose suspension reason is a free-form
/// string, When it is deserialized and projected, Then it stays receivable:
/// deserialization maps it to the `Legacy` form and the projection passes
/// the string through unchanged (old consumers discriminate with `typeof`;
/// nothing crashes).
#[test]
fn should_keep_legacy_string_suspension_reasons_receivable_when_replayed() {
    // Given an old journal row carrying the free-form reason
    let json = serde_json::json!({
        "type": "subagent_suspended",
        "subagent_id": "agent-3",
        "reason": "Provider rate limit; subagent requeued for retry.",
    });

    // When it is deserialized and projected
    let event: SessionEvent = serde_json::from_value(json).unwrap();
    let projected = projected(&event);

    // Then the reason survives as the original string — new consumers must
    // not crash on the legacy shape
    assert_eq!(
        projected.payload["reason"],
        "Provider rate limit; subagent requeued for retry."
    );
}

/// Given a suspension reason in an unknown shape, When the event is
/// deserialized, Then it does not crash — the shape is preserved as a
/// legacy string instead (the projection layer's unknown-shape pass-through
/// for the compatibility window).
#[test]
fn should_not_crash_on_unknown_suspension_reason_shapes() {
    // Given a reason that is neither a string nor a known structured kind
    let json = serde_json::json!({
        "type": "subagent_suspended",
        "subagent_id": "agent-4",
        "reason": { "kind": "future_kind", "detail": 7 },
    });

    // When it is deserialized
    let event: SessionEvent = serde_json::from_value(json).unwrap();

    // Then the event is preserved with a legacy fallback reason
    match event {
        SessionEvent::SubagentSuspended { reason, .. } => {
            assert!(matches!(reason, SuspendCause::Legacy(_)));
        }
        other => panic!("expected SubagentSuspended, got {other:?}"),
    }
}

/// Given a structured suspension reason, When the session event is
/// round-tripped through its serde, Then the shape survives verbatim
/// (journals written with the structured form replay it exactly).
#[test]
fn should_roundtrip_structured_suspension_reasons_through_the_session_event() {
    // Given a structured approval suspension event with an absent tool call
    // id (the optional facet omits — never a null placeholder)
    let event = SessionEvent::SubagentSuspended {
        subagent_id: "agent-5".to_string(),
        reason: SuspendCause::Approval {
            request_id: "req-2".to_string(),
            tool_call_id: None,
        },
    };

    // When serialized and deserialized
    let json = serde_json::to_string(&event).unwrap();
    let roundtrip: SessionEvent = serde_json::from_str(&json).unwrap();

    // Then the structured reason survives field-for-field
    match roundtrip {
        SessionEvent::SubagentSuspended {
            subagent_id,
            reason,
        } => {
            assert_eq!(subagent_id, "agent-5");
            assert_eq!(
                reason,
                SuspendCause::Approval {
                    request_id: "req-2".to_string(),
                    tool_call_id: None,
                }
            );
        }
        other => panic!("expected SubagentSuspended, got {other:?}"),
    }
}

// ===========================================================================
// AgentPhase derivation (Task 7, CR4)
// ===========================================================================

/// Port-fidelity tests for [`phase_from_activity`], the Rust port of the
/// kap-server TS function `toLegacyPhase`
/// (`packages/kap-server/src/services/legacyStatus/legacyStatus.ts:155-238`).
/// Each test builds an [`AgentActivityState`] by folding events onto a fresh
/// [`AgentActivityView`] — the same way the real fold produces snapshots.
mod agent_phase_derivation {
    use mirri_core::session::agent_status::{AgentPhase, phase_from_activity};
    use mirri_core::{
        ActivityRetryState, AgentActivityState, AgentActivityView, ApprovalRef, EndingReason,
        PromptOrigin, StreamKind, TurnEndReason,
    };

    /// Fold events onto a fresh view and return the resulting snapshot.
    fn snapshot(fold: impl FnOnce(&mut AgentActivityView)) -> AgentActivityState {
        let mut view = AgentActivityView::new();
        fold(&mut view);
        view.state().clone()
    }

    /// Given no active turn and no last-turn outcome, when deriving the
    /// phase, then it is `Idle`.
    #[test]
    fn should_derive_idle_when_no_turn_and_no_last_turn() {
        let state = snapshot(|_| {});

        assert_eq!(phase_from_activity(&state), AgentPhase::Idle);
    }

    /// Given no active turn but a recorded last-turn outcome, when deriving
    /// the phase, then it is `Ended`.
    #[test]
    fn should_derive_ended_when_no_turn_but_last_turn_present() {
        let state = snapshot(|view| {
            view.on_turn_started(1, PromptOrigin::User);
            view.on_turn_ended(1, TurnEndReason::Completed, Some(1500));
        });

        assert_eq!(phase_from_activity(&state), AgentPhase::Ended);
    }

    /// Given a turn that is ending AND has pending approvals, when deriving
    /// the phase, then it is `AwaitingApproval` — the TS source checks
    /// pending approvals before the ending flag (no premature `running` /
    /// `interrupted` while an approval is still outstanding).
    #[test]
    fn should_derive_awaiting_approval_when_approvals_pending() {
        let state = snapshot(|view| {
            view.on_turn_started(1, PromptOrigin::User);
            view.on_ending(Some(EndingReason::Aborted));
            view.on_approval_pending(ApprovalRef {
                approval_id: "ap-1".to_string(),
                tool_call_id: None,
                since: 0,
            });
        });

        assert_eq!(phase_from_activity(&state), AgentPhase::AwaitingApproval);
    }

    /// Given a turn in its ending phase with a known reason, when deriving
    /// the phase, then it is `Interrupted`.
    #[test]
    fn should_derive_interrupted_when_turn_is_ending() {
        let state = snapshot(|view| {
            view.on_turn_started(1, PromptOrigin::User);
            view.on_ending(Some(EndingReason::MaxSteps));
        });

        assert_eq!(phase_from_activity(&state), AgentPhase::Interrupted);
    }

    /// Given a turn whose ending flag is set but whose ending reason is
    /// unknown, when deriving the phase, then it falls through to the
    /// turn-phase mapping — the TS source requires `endingReason` to be
    /// defined before emitting `interrupted`.
    #[test]
    fn should_fall_through_to_turn_phase_when_ending_has_no_reason() {
        let state = snapshot(|view| {
            view.on_turn_started(1, PromptOrigin::User);
            view.on_streaming(StreamKind::Assistant);
            view.on_ending(None);
        });

        assert_eq!(phase_from_activity(&state), AgentPhase::Streaming);
    }

    /// Given a turn in the retrying phase, when deriving the phase, then it
    /// is `Retrying` — the TS source maps the `retrying` turn phase directly
    /// (retry detail defaults to zeroed fields when absent, it never falls
    /// back to `running`).
    #[test]
    fn should_derive_retrying_when_retry_state_present() {
        let state = snapshot(|view| {
            view.on_turn_started(1, PromptOrigin::User);
            view.on_retry(ActivityRetryState {
                failed_attempt: 1,
                next_attempt: 2,
                max_attempts: 3,
                delay_ms: 500,
                error_name: Some("rate_limited".to_string()),
                status_code: Some(429),
            });
        });

        assert_eq!(phase_from_activity(&state), AgentPhase::Retrying);
    }

    /// Given a turn in each of the plain phases (running, streaming,
    /// tool_call), when deriving the phase, then each maps 1:1 to the
    /// corresponding `AgentPhase` variant.
    #[test]
    fn should_derive_streaming_tool_call_and_running_from_turn_phase() {
        let running = snapshot(|view| {
            view.on_turn_started(1, PromptOrigin::User);
        });
        assert_eq!(phase_from_activity(&running), AgentPhase::Running);

        let streaming = snapshot(|view| {
            view.on_turn_started(2, PromptOrigin::User);
            view.on_streaming(StreamKind::Assistant);
        });
        assert_eq!(phase_from_activity(&streaming), AgentPhase::Streaming);

        let tool_call = snapshot(|view| {
            view.on_turn_started(3, PromptOrigin::User);
            view.on_tool_call("tc-1".to_string(), "Read".to_string());
        });
        assert_eq!(phase_from_activity(&tool_call), AgentPhase::ToolCall);
    }
}

// ===========================================================================
// SessionMetadata — last_used_at persistence roundtrip (SDR-014)
// ===========================================================================

/// Given a metadata with `last_used_at` set, When it is saved to `state.json`
/// and loaded back, Then `last_used_at` survives the round trip — and a
/// legacy state file predating the field deserializes to `None` (readers
/// fall back to `updated_at`).
#[tokio::test]
async fn should_roundtrip_last_used_at_through_state_persistence() {
    let env = TestEnv::new().await;
    let session = env
        .manager
        .create_session(make_metadata("Last Used"), SessionScopeConfig::default())
        .await
        .unwrap();
    let session_id = session.id().clone();

    // Given a metadata whose last_used_at is set (a send or a touch bumped it)
    let mut metadata = env
        .manager
        .persistence()
        .load_state(&session_id)
        .await
        .unwrap();
    let stamped = chrono::Utc::now();
    metadata.last_used_at = Some(stamped);
    env.manager
        .persistence()
        .save_state(&session_id, &metadata)
        .await
        .unwrap();

    // When the state is loaded back
    let reloaded = env
        .manager
        .persistence()
        .load_state(&session_id)
        .await
        .unwrap();

    // Then last_used_at survives the persisted round trip
    assert_eq!(reloaded.last_used_at, Some(stamped));
    // And the Default (a fresh session that never bumped it) is None
    assert_eq!(SessionMetadata::default().last_used_at, None);

    // And a legacy state.json predating the field deserializes to None
    let state_path = env.manager.persistence().state_path(&session_id);
    tokio::fs::write(
        &state_path,
        r#"{"title":"legacy","createdAt":"2024-01-01T00:00:00Z","updatedAt":"2024-01-02T00:00:00Z","tags":[]}"#,
    )
    .await
    .unwrap();
    let legacy = env
        .manager
        .persistence()
        .load_state(&session_id)
        .await
        .unwrap();
    assert_eq!(legacy.last_used_at, None);
}
