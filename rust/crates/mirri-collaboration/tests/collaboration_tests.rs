//! Integration tests for the collaboration crate: mode validation, session
//! lifecycle, the inter-agent message protocol, intervention, conflict
//! escalation, termination, and state transitions.
//!
//! Test names follow the Given-When-Then style: `should_<outcome>_when_<condition>`.

use mirri_collaboration::domain::{
    CollaborationMode, CollaborationRules, CommunicationPattern, ConflictResolution,
    InterventionPolicy, RoleDefinition,
};
use mirri_collaboration::error::CollaborationError;
use mirri_collaboration::manager::CollaborationManager;
use mirri_collaboration::protocol::{AgentMessage, AgentStatus, TaskStatus};
use mirri_collaboration::session::CollaborationState;
use uuid::Uuid;

/// A small review team: coordinator, executor, reviewer (design doc §3.3).
fn test_mode() -> CollaborationMode {
    CollaborationMode {
        id: "review-team".to_string(),
        name: "Review Team".to_string(),
        description: "Coordinator, executor and reviewer working on one task".to_string(),
        roles: vec![
            RoleDefinition {
                name: "coordinator".to_string(),
                description: "Splits the work and assigns tasks".to_string(),
                system_prompt: "You are the coordinator.".to_string(),
                capabilities: vec!["read".to_string(), "plan".to_string()],
                max_agents: Some(1),
            },
            RoleDefinition {
                name: "executor".to_string(),
                description: "Executes assigned tasks".to_string(),
                system_prompt: "You are the executor.".to_string(),
                capabilities: vec![
                    "read".to_string(),
                    "write".to_string(),
                    "run_tests".to_string(),
                ],
                max_agents: Some(1),
            },
            RoleDefinition {
                name: "reviewer".to_string(),
                description: "Reviews the executor's work".to_string(),
                system_prompt: "You are the reviewer.".to_string(),
                capabilities: vec!["read".to_string(), "run_tests".to_string()],
                max_agents: None,
            },
        ],
        rules: CollaborationRules {
            communication_pattern: CommunicationPattern::Sequential,
            intervention_policy: InterventionPolicy::Human,
            conflict_resolution: ConflictResolution::HumanArbitration,
            max_rounds: None,
        },
    }
}

/// Starts a fresh manager with one session of the review-team mode.
fn start_session() -> (CollaborationManager, Uuid) {
    let manager = CollaborationManager::new();
    let session_id = manager
        .start(test_mode(), "Refactor the auth module".to_string())
        .expect("test mode must be valid");
    (manager, session_id)
}

fn task_assignment(from: &str, to: &str, task: &str) -> AgentMessage {
    AgentMessage::TaskAssignment {
        from: from.to_string(),
        to: to.to_string(),
        task: task.to_string(),
        context: serde_json::json!({}),
    }
}

fn task_result(from: &str, task_id: &str, status: TaskStatus) -> AgentMessage {
    AgentMessage::TaskResult {
        from: from.to_string(),
        to: "coordinator".to_string(),
        task_id: task_id.to_string(),
        result: serde_json::json!({ "done": true }),
        status,
    }
}

// ---------------------------------------------------------------------------
// Collaboration mode
// ---------------------------------------------------------------------------

#[test]
fn should_build_collaboration_mode_when_given_roles_and_rules() {
    let mode = test_mode();

    assert_eq!(mode.id, "review-team");
    assert_eq!(mode.roles.len(), 3);
    assert_eq!(mode.roles[0].name, "coordinator");
    assert_eq!(
        mode.roles[0].capabilities,
        vec!["read".to_string(), "plan".to_string()]
    );
    assert_eq!(
        mode.rules.communication_pattern,
        CommunicationPattern::Sequential
    );
    assert_eq!(mode.rules.intervention_policy, InterventionPolicy::Human);
    assert_eq!(
        mode.rules.conflict_resolution,
        ConflictResolution::HumanArbitration
    );
    assert_eq!(mode.rules.max_rounds, None);
}

#[test]
fn should_round_trip_a_mode_through_json_when_serialized() {
    let mode = test_mode();

    let json = serde_json::to_string(&mode).unwrap();
    let decoded: CollaborationMode = serde_json::from_str(&json).unwrap();

    assert_eq!(decoded, mode);
}

#[test]
fn should_accept_a_valid_mode_when_validated() {
    assert!(test_mode().validate().is_ok());
}

#[test]
fn should_reject_duplicate_role_names_when_validating_a_mode() {
    let mut mode = test_mode();
    mode.roles[0].name = "executor".to_string();

    let error = mode.validate().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("'executor' is defined more than once")
    );
}

#[test]
fn should_reject_an_empty_role_list_when_validating_a_mode() {
    let mut mode = test_mode();
    mode.roles.clear();

    let error = mode.validate().unwrap_err();
    assert!(error.to_string().contains("at least one role"));
}

#[test]
fn should_reject_nonsensical_limits_when_validating_a_mode() {
    let mut zero_max_agents = test_mode();
    zero_max_agents.roles[0].max_agents = Some(0);
    let error = zero_max_agents.validate().unwrap_err();
    assert!(error.to_string().contains("max_agents"));

    let mut zero_max_rounds = test_mode();
    zero_max_rounds.rules.max_rounds = Some(0);
    let error = zero_max_rounds.validate().unwrap_err();
    assert!(error.to_string().contains("max_rounds"));
}

// ---------------------------------------------------------------------------
// Session lifecycle
// ---------------------------------------------------------------------------

#[test]
fn should_return_invalid_mode_error_when_starting_a_session_with_an_invalid_mode() {
    let mut mode = test_mode();
    mode.roles[0].name = "executor".to_string(); // duplicate role name

    let manager = CollaborationManager::new();
    let error = manager
        .start(mode, "Refactor the auth module".to_string())
        .unwrap_err();

    assert!(matches!(error, CollaborationError::InvalidMode(_)));
}

#[test]
fn should_start_a_session_with_one_agent_per_role_when_given_a_valid_mode() {
    let (manager, session_id) = start_session();

    let session = manager.session(session_id).unwrap();
    assert_eq!(session.agents.len(), 3);
    assert!(matches!(
        session.state,
        CollaborationState::InProgress { round: 1 }
    ));
    assert_eq!(session.task, "Refactor the auth module");

    for (role, handle) in &session.agents {
        assert_eq!(&handle.role, role);
        assert!(
            handle.agent_id.starts_with(role.as_str()),
            "agent_id {} should start with its role {role}",
            handle.agent_id
        );
        assert_eq!(handle.status, AgentStatus::Idle);
    }
}

#[test]
fn should_error_when_querying_a_session_that_was_never_started() {
    let manager = CollaborationManager::new();

    let error = manager.state(Uuid::new_v4()).unwrap_err();
    assert!(matches!(error, CollaborationError::SessionNotFound { .. }));

    let error = manager.messages(Uuid::new_v4()).unwrap_err();
    assert!(matches!(error, CollaborationError::SessionNotFound { .. }));
}

// ---------------------------------------------------------------------------
// Inter-agent messaging
// ---------------------------------------------------------------------------

#[test]
fn should_deliver_a_task_assignment_when_both_agents_exist() {
    let (manager, session_id) = start_session();

    manager
        .send_message(
            session_id,
            task_assignment("coordinator", "executor", "Refactor auth"),
        )
        .unwrap();

    // The recipient starts working.
    assert_eq!(
        manager.agent_status(session_id, "executor").unwrap(),
        AgentStatus::Working
    );

    // The assignment lands in the shared log with its payload.
    match manager.messages(session_id).unwrap().first() {
        Some(AgentMessage::TaskAssignment { from, to, task, .. }) => {
            assert_eq!(from, "coordinator");
            assert_eq!(to, "executor");
            assert_eq!(task, "Refactor auth");
        }
        other => panic!("expected a task assignment, got {other:?}"),
    }
}

#[test]
fn should_preserve_message_order_when_several_messages_are_exchanged() {
    let (manager, session_id) = start_session();

    manager
        .send_message(
            session_id,
            task_assignment("coordinator", "executor", "first"),
        )
        .unwrap();
    manager
        .send_message(
            session_id,
            AgentMessage::Question {
                from: "executor".to_string(),
                to: "reviewer".to_string(),
                question: "Second?".to_string(),
            },
        )
        .unwrap();
    manager
        .send_message(
            session_id,
            AgentMessage::StatusUpdate {
                from: "reviewer".to_string(),
                status: AgentStatus::Done,
            },
        )
        .unwrap();

    let log = manager.messages(session_id).unwrap();
    assert_eq!(log.len(), 3);
    assert!(matches!(&log[0], AgentMessage::TaskAssignment { .. }));
    assert!(matches!(&log[1], AgentMessage::Question { .. }));
    assert!(matches!(&log[2], AgentMessage::StatusUpdate { .. }));
}

#[test]
fn should_reject_a_message_when_the_sender_is_not_part_of_the_session() {
    let (manager, session_id) = start_session();

    let error = manager
        .send_message(session_id, task_assignment("ghost", "executor", "x"))
        .unwrap_err();
    assert!(
        matches!(
            &error,
            CollaborationError::ProtocolViolation(msg)
                if msg.contains("'ghost'")
                    && msg.contains("available agents: coordinator, executor, reviewer")
        ),
        "expected ProtocolViolation listing available agents for 'ghost', got {error:?}"
    );
}

#[test]
fn should_reject_a_message_when_the_recipient_is_not_part_of_the_session() {
    let (manager, session_id) = start_session();

    let error = manager
        .send_message(session_id, task_assignment("coordinator", "ghost", "x"))
        .unwrap_err();
    assert!(
        matches!(
            &error,
            CollaborationError::ProtocolViolation(msg)
                if msg.contains("'ghost'")
                    && msg.contains("available agents: coordinator, executor, reviewer")
        ),
        "expected ProtocolViolation listing available agents for 'ghost', got {error:?}"
    );
}

#[test]
fn should_update_the_sender_status_when_a_task_result_arrives() {
    let (manager, session_id) = start_session();

    manager
        .send_message(
            session_id,
            task_result("executor", "task-1", TaskStatus::Completed),
        )
        .unwrap();
    assert_eq!(
        manager.agent_status(session_id, "executor").unwrap(),
        AgentStatus::Done
    );

    manager
        .send_message(
            session_id,
            task_result("reviewer", "task-2", TaskStatus::InProgress),
        )
        .unwrap();
    assert_eq!(
        manager.agent_status(session_id, "reviewer").unwrap(),
        AgentStatus::Working
    );

    manager
        .send_message(
            session_id,
            task_result("coordinator", "task-3", TaskStatus::Blocked),
        )
        .unwrap();
    assert_eq!(
        manager.agent_status(session_id, "coordinator").unwrap(),
        AgentStatus::Waiting
    );

    manager
        .send_message(
            session_id,
            task_result("executor", "task-4", TaskStatus::Failed),
        )
        .unwrap();
    assert_eq!(
        manager.agent_status(session_id, "executor").unwrap(),
        AgentStatus::Error
    );
}

#[test]
fn should_advance_the_round_when_a_task_result_arrives() {
    let (manager, session_id) = start_session();
    assert!(matches!(
        manager.state(session_id).unwrap(),
        CollaborationState::InProgress { round: 1 }
    ));

    manager
        .send_message(
            session_id,
            task_result("executor", "task-1", TaskStatus::Completed),
        )
        .unwrap();

    assert!(matches!(
        manager.state(session_id).unwrap(),
        CollaborationState::InProgress { round: 2 }
    ));
}

#[test]
fn should_record_question_and_answer_exchanges_when_agents_communicate() {
    let (manager, session_id) = start_session();

    manager
        .send_message(
            session_id,
            AgentMessage::Question {
                from: "executor".to_string(),
                to: "reviewer".to_string(),
                question: "Are the tests green?".to_string(),
            },
        )
        .unwrap();
    manager
        .send_message(
            session_id,
            AgentMessage::Answer {
                from: "reviewer".to_string(),
                to: "executor".to_string(),
                question_id: "q-1".to_string(),
                answer: "Yes, all green.".to_string(),
            },
        )
        .unwrap();

    let log = manager.messages(session_id).unwrap();
    assert_eq!(log.len(), 2);
    assert!(matches!(
        &log[0],
        AgentMessage::Question { question, .. } if question == "Are the tests green?"
    ));
    assert!(matches!(
        &log[1],
        AgentMessage::Answer { question_id, answer, .. }
            if question_id == "q-1" && answer == "Yes, all green."
    ));
}

#[test]
fn should_record_an_agent_status_update_when_an_agent_reports_state() {
    let (manager, session_id) = start_session();

    manager
        .send_message(
            session_id,
            AgentMessage::StatusUpdate {
                from: "coordinator".to_string(),
                status: AgentStatus::Waiting,
            },
        )
        .unwrap();

    assert_eq!(
        manager.agent_status(session_id, "coordinator").unwrap(),
        AgentStatus::Waiting
    );
}

// ---------------------------------------------------------------------------
// Conflict escalation
// ---------------------------------------------------------------------------

#[test]
fn should_pause_the_session_when_a_conflict_is_escalated() {
    let (manager, session_id) = start_session();
    let parties = vec!["reviewer".to_string(), "executor".to_string()];

    manager
        .escalate_conflict(session_id, "reviewer", ">= vs > boundary case", &parties)
        .unwrap();

    // The dispute awaits arbitration: the session pauses.
    assert_eq!(
        manager.state(session_id).unwrap(),
        CollaborationState::Paused
    );

    match manager.messages(session_id).unwrap().last() {
        Some(AgentMessage::ConflictEscalation {
            from,
            description,
            parties: logged_parties,
        }) => {
            assert_eq!(from, "reviewer");
            assert_eq!(description, ">= vs > boundary case");
            assert_eq!(logged_parties, &parties);
        }
        other => panic!("expected a conflict escalation, got {other:?}"),
    }

    // A paused session accepts no regular traffic until the dispute is resolved.
    let error = manager
        .send_message(
            session_id,
            task_assignment("coordinator", "executor", "more work"),
        )
        .unwrap_err();
    assert!(matches!(error, CollaborationError::InvalidState { .. }));
}

#[test]
fn should_pause_the_session_when_a_conflict_escalation_message_arrives() {
    let (manager, session_id) = start_session();

    manager
        .send_message(
            session_id,
            AgentMessage::ConflictEscalation {
                from: "reviewer".to_string(),
                description: "Unresolved boundary disagreement".to_string(),
                parties: vec!["reviewer".to_string(), "executor".to_string()],
            },
        )
        .unwrap();

    assert_eq!(
        manager.state(session_id).unwrap(),
        CollaborationState::Paused
    );
}

#[test]
fn should_reject_conflict_escalation_when_a_party_is_unknown() {
    let (manager, session_id) = start_session();
    let parties = vec!["reviewer".to_string(), "ghost".to_string()];

    let error = manager
        .escalate_conflict(session_id, "reviewer", "dispute", &parties)
        .unwrap_err();
    assert!(
        matches!(
            &error,
            CollaborationError::ProtocolViolation(msg)
                if msg.contains("'ghost'")
                    && msg.contains("available agents: coordinator, executor, reviewer")
        ),
        "expected ProtocolViolation listing available agents for 'ghost', got {error:?}"
    );

    // The failed escalation must not have paused the session.
    assert!(matches!(
        manager.state(session_id).unwrap(),
        CollaborationState::InProgress { .. }
    ));
}

// ---------------------------------------------------------------------------
// Intervention
// ---------------------------------------------------------------------------

#[test]
fn should_record_an_intervention_when_a_directive_is_issued() {
    let (manager, session_id) = start_session();

    manager
        .intervene(session_id, "executor", "Do not modify auth.rs directly")
        .unwrap();

    match manager.messages(session_id).unwrap().last() {
        Some(AgentMessage::Intervention {
            from,
            to,
            directive,
        }) => {
            assert_eq!(from, "human");
            assert_eq!(to, "executor");
            assert_eq!(directive, "Do not modify auth.rs directly");
        }
        other => panic!("expected an intervention, got {other:?}"),
    }

    // A directive steers the target's next turn; it does not pause the session.
    assert!(matches!(
        manager.state(session_id).unwrap(),
        CollaborationState::InProgress { .. }
    ));
}

#[test]
fn should_reject_an_intervention_when_the_target_is_unknown() {
    let (manager, session_id) = start_session();

    let error = manager.intervene(session_id, "ghost", "stop").unwrap_err();
    assert!(
        matches!(
            &error,
            CollaborationError::ProtocolViolation(msg)
                if msg.contains("'ghost'")
                    && msg.contains("available agents: coordinator, executor, reviewer")
        ),
        "expected ProtocolViolation listing available agents for 'ghost', got {error:?}"
    );
}

// ---------------------------------------------------------------------------
// Termination and terminal states
// ---------------------------------------------------------------------------

#[test]
fn should_terminate_the_session_when_requested() {
    let (manager, session_id) = start_session();

    manager.terminate(session_id).unwrap();
    assert!(matches!(
        manager.state(session_id).unwrap(),
        CollaborationState::Failed(reason) if reason == "terminated"
    ));

    // Terminated sessions reject new traffic ...
    let error = manager
        .send_message(
            session_id,
            task_assignment("coordinator", "executor", "late work"),
        )
        .unwrap_err();
    assert!(matches!(error, CollaborationError::InvalidState { .. }));

    // ... cannot be terminated twice ...
    let error = manager.terminate(session_id).unwrap_err();
    assert!(matches!(error, CollaborationError::InvalidState { .. }));

    // ... and their recorded log stays readable.
    assert!(manager.messages(session_id).unwrap().is_empty());
}

#[test]
fn should_reject_messages_when_the_session_is_paused() {
    let (manager, session_id) = start_session();
    manager
        .escalate_conflict(session_id, "reviewer", "dispute", &["executor".to_string()])
        .unwrap();
    assert_eq!(
        manager.state(session_id).unwrap(),
        CollaborationState::Paused
    );

    let error = manager
        .send_message(
            session_id,
            task_assignment("coordinator", "executor", "more work"),
        )
        .unwrap_err();
    assert!(matches!(error, CollaborationError::InvalidState { .. }));
}

#[test]
fn should_complete_the_session_when_every_agent_reports_done() {
    let (manager, session_id) = start_session();

    for role in ["coordinator", "executor", "reviewer"] {
        manager
            .send_message(
                session_id,
                AgentMessage::StatusUpdate {
                    from: role.to_string(),
                    status: AgentStatus::Done,
                },
            )
            .unwrap();
    }

    assert_eq!(
        manager.state(session_id).unwrap(),
        CollaborationState::Completed
    );

    // Completed sessions reject new traffic and interventions.
    let error = manager
        .send_message(
            session_id,
            task_assignment("coordinator", "executor", "too late"),
        )
        .unwrap_err();
    assert!(matches!(error, CollaborationError::InvalidState { .. }));

    let error = manager
        .intervene(session_id, "executor", "stop")
        .unwrap_err();
    assert!(matches!(error, CollaborationError::InvalidState { .. }));
}

#[test]
fn should_force_termination_when_the_round_limit_is_exceeded() {
    let mut mode = test_mode();
    mode.rules.max_rounds = Some(2);
    let manager = CollaborationManager::new();
    let session_id = manager
        .start(mode, "Refactor the auth module".to_string())
        .expect("valid mode");

    // Two task-result rounds are allowed ...
    manager
        .send_message(
            session_id,
            task_result("executor", "task-1", TaskStatus::Completed),
        )
        .unwrap();
    assert!(matches!(
        manager.state(session_id).unwrap(),
        CollaborationState::InProgress { round: 2 }
    ));

    // ... the third one trips the loop guard and force-terminates the session.
    manager
        .send_message(
            session_id,
            task_result("executor", "task-2", TaskStatus::Completed),
        )
        .unwrap();
    match manager.state(session_id).unwrap() {
        CollaborationState::Failed(reason) => {
            assert!(
                reason.contains("max_rounds (2)"),
                "unexpected failure reason: {reason}"
            );
        }
        other => panic!("expected the round limit to force termination, got {other:?}"),
    }

    // The tripping message is still part of the journal.
    assert_eq!(manager.messages(session_id).unwrap().len(), 2);
}

// ---------------------------------------------------------------------------
// Wire format
// ---------------------------------------------------------------------------

#[test]
fn should_serialize_messages_with_a_type_tag_when_written_as_json() {
    let message = task_assignment("coordinator", "executor", "Refactor auth");

    let json = serde_json::to_string(&message).unwrap();
    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(value["type"], "task_assignment");
    assert_eq!(value["from"], "coordinator");
    assert_eq!(value["to"], "executor");
    assert_eq!(value["task"], "Refactor auth");

    // The round trip is lossless.
    let decoded: AgentMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded, message);
}

#[test]
fn should_deserialize_an_unknown_message_type_when_reading_a_journal() {
    let json = r#"{"type":"telepathy","from":"coordinator","to":"executor"}"#;

    let decoded: AgentMessage = serde_json::from_str(json).unwrap();
    assert_eq!(decoded, AgentMessage::Unknown);
    assert_eq!(decoded.sender(), None);
    assert_eq!(decoded.recipient(), None);

    // Unknown messages carry no routing information and cannot be sent.
    let (manager, session_id) = start_session();
    let error = manager
        .send_message(session_id, AgentMessage::Unknown)
        .unwrap_err();
    assert!(matches!(error, CollaborationError::ProtocolViolation(_)));
}

#[test]
fn should_serialize_statuses_with_snake_case_when_written_as_json() {
    assert_eq!(
        serde_json::to_string(&TaskStatus::InProgress).unwrap(),
        "\"in_progress\""
    );
    assert_eq!(
        serde_json::to_string(&TaskStatus::Blocked).unwrap(),
        "\"blocked\""
    );
    assert_eq!(
        serde_json::to_string(&AgentStatus::Waiting).unwrap(),
        "\"waiting\""
    );
    assert_eq!(
        serde_json::to_string(&AgentStatus::Done).unwrap(),
        "\"done\""
    );
}
