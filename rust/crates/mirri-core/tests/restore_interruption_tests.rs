//! Integration tests for interruption-reminder reconciliation after a wire
//! restore (Task: context-injection-rust-wiring, Task 5).
//!
//! These tests verify the resume path's `reconcilePendingReminders`
//! semantics (TS parity): when the replayed `AgentState.interruption_pending`
//! is non-empty — the crash point was between the durable `turn.cancel`
//! write and the reminder delivery — the resume must deliver the reminder
//! into the reconstructed memory and record the delivery ops on the wire
//! log it just re-opened for appending, so the next replay starts from a
//! drained set:
//!
//! - a journal that stopped before the delivery gains the reminder message
//!   plus the `context.append_message` / `interruptionReminder.recorded`
//!   ops after the restore;
//! - a journal that already recorded the full delivery sequence is not
//!   duplicated;
//! - a journal that crashed after the durable reminder append (but before
//!   the `interruptionReminder.recorded` marker) skips the duplicate append
//!   while still recording the pending marker.
//!
//! Tests use the Given-When-Then naming convention.

use std::collections::HashSet;

use mirri_core::{
    AgentId, AgentScopeConfig, AppScope, AppScopeConfig, PromptOrigin, SessionId, SessionManager,
    SessionMetadata, WorkspaceId, WorkspaceScopeConfig, WorkspaceTrustLevel,
};
use mirri_persistence::{AppendLogStore, JsonlAppendLog};
use mirri_proto::WireOp;
use serde_json::json;

// ===========================================================================
// Helpers
// ===========================================================================

/// A temp-dir-backed session manager (same scaffolding as the resume tests).
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
                WorkspaceId::new("test-ws"),
                tmp.path().to_path_buf(),
                WorkspaceScopeConfig {
                    trust_level: WorkspaceTrustLevel::Trusted,
                    ..Default::default()
                },
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
        tags: HashSet::new(),
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

/// A resume base config carrying an explicit model (the hand-written wire
/// logs here carry no model record, so the base config seeds the model the
/// way the embedded host seeds it from the config snapshot's
/// `default_model`).
fn resume_base_config() -> AgentScopeConfig {
    AgentScopeConfig {
        model_id: "test-resume-model".to_string(),
        ..AgentScopeConfig::default()
    }
}

/// The `<system-reminder>`-wrapped interruption reminder text exactly as
/// `ContextMemory::append_system_reminder` stores it.
fn reminder_wire_text() -> String {
    mirri_core::system_reminder_wire::system_reminder_text(
        mirri_core::interruption_reminder::INTERRUPTION_REMINDER_TEXT,
    )
}

/// A user cancellation of the active turn (the only `turn.cancel` shape
/// that parks an interruption reminder).
fn user_cancel_op(turn_id: u64) -> WireOp {
    WireOp::TurnCancel {
        turn_id: Some(turn_id),
        target: Some("active".to_string()),
        reason: Some("user_cancelled".to_string()),
    }
}

/// The durable reminder-append op exactly as the live write side records
/// it (Task 4's `deliver_interruption_reminder`).
fn reminder_append_op() -> WireOp {
    mirri_core::system_reminder_wire::system_reminder_op(
        mirri_core::interruption_reminder::INTERRUPTION_REMINDER_TEXT,
    )
}

/// Write a sequence of wire ops to the session's `wire.jsonl`
/// (truncating any existing file).
async fn write_wire_log(manager: &SessionManager, session_id: &SessionId, ops: &[WireOp]) {
    let wire_log_path = manager.persistence().wire_log_path(session_id);
    let mut log = JsonlAppendLog::create(&wire_log_path).await.unwrap();
    for op in ops {
        log.append(op).await.unwrap();
    }
}

/// Read the session's current `wire.jsonl` back as parsed wire ops.
async fn read_wire_log_ops(manager: &SessionManager, session_id: &SessionId) -> Vec<WireOp> {
    let wire_log_path = manager.persistence().wire_log_path(session_id);
    let contents = std::fs::read_to_string(&wire_log_path)
        .expect("the session's wire log must be readable after the resume");
    contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<WireOp>(line).expect("wire log line must parse"))
        .collect()
}

/// How many interruption reminders (Injection origin, reminder text) the
/// agent's reconstructed memory holds.
fn reminder_count(agent: &mirri_core::AgentScope) -> usize {
    let memory = agent.context_memory();
    let guard = memory.lock().unwrap();
    guard
        .history()
        .iter()
        .filter(|m| {
            m.origin == Some(PromptOrigin::Injection)
                && m.message.text_content() == reminder_wire_text()
        })
        .count()
}

/// The resumed session's recreated main agent.
async fn main_agent(session: &mirri_core::SessionScope) -> std::sync::Arc<mirri_core::AgentScope> {
    session
        .get_agent(&AgentId::new("main"))
        .await
        .expect("resumed session must recreate the main agent")
}

// ===========================================================================
// Restore reconciles a pending interruption reminder
// ===========================================================================

#[tokio::test]
async fn should_replay_pending_interruption_reminder_after_restore() {
    // Given a persisted session whose journal records one user message and
    // a user cancellation of turn 9 — but NOT the reminder delivery (the
    // crash point was between the durable `turn.cancel` write and the
    // reminder injection)
    let env = TestEnv::new().await;
    let session_id = SessionId::new("restore-pending-reminder");
    env.manager
        .persistence()
        .save_state(&session_id, &make_metadata("Pending Reminder"))
        .await
        .unwrap();
    write_wire_log(
        &env.manager,
        &session_id,
        &[
            WireOp::Metadata {
                protocol_version: Some("1.5".to_string()),
                created_at: Some(1700000000000),
            },
            WireOp::ContextAppendMessage {
                message: json!({
                    "role": "user",
                    "content": [{"type": "text", "text": "hello"}],
                    "toolCalls": [],
                }),
            },
            user_cancel_op(9),
        ],
    )
    .await;
    let ops_before = read_wire_log_ops(&env.manager, &session_id).await;

    // When resuming the session
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then the main agent's memory ends with the interruption reminder
    // (Injection origin, the <system-reminder>-wrapped text)
    let agent = main_agent(&resumed).await;
    {
        let memory = agent.context_memory();
        let guard = memory.lock().unwrap();
        let last = guard
            .history()
            .last()
            .expect("the reconciled memory must not be empty");
        assert_eq!(last.origin, Some(PromptOrigin::Injection));
        assert_eq!(last.message.text_content(), reminder_wire_text());
    }
    assert_eq!(reminder_count(&agent), 1);

    // And the journal gained exactly the durable delivery ops after the
    // restore: `context.append_message` (the reminder) then
    // `interruptionReminder.recorded { turnId: 9 }`
    let ops_after = read_wire_log_ops(&env.manager, &session_id).await;
    assert_eq!(
        ops_after.len(),
        ops_before.len() + 2,
        "the restore must append exactly the reminder append + the recorded marker"
    );
    let tail: Vec<&WireOp> = ops_after[ops_before.len()..].iter().collect();
    match tail[0] {
        WireOp::ContextAppendMessage { message } => {
            let WireOp::ContextAppendMessage { message: expected } = reminder_append_op() else {
                unreachable!()
            };
            assert_eq!(message, &expected);
        }
        other => panic!("expected a context.append_message op, got {other:?}"),
    }
    match tail[1] {
        WireOp::InterruptionReminderRecorded { turn_id } => assert_eq!(*turn_id, 9),
        other => panic!("expected an interruptionReminder.recorded op, got {other:?}"),
    }
}

#[tokio::test]
async fn should_not_duplicate_reminder_when_replaying_journal_that_already_delivered() {
    // Given a persisted session whose journal records the full delivery
    // sequence: the user cancellation of turn 9, the reminder's
    // `context.append_message`, and the `interruptionReminder.recorded`
    // marker — the replayed pending set is empty
    let env = TestEnv::new().await;
    let session_id = SessionId::new("restore-delivered-reminder");
    env.manager
        .persistence()
        .save_state(&session_id, &make_metadata("Delivered Reminder"))
        .await
        .unwrap();
    write_wire_log(
        &env.manager,
        &session_id,
        &[
            WireOp::Metadata {
                protocol_version: Some("1.5".to_string()),
                created_at: Some(1700000000000),
            },
            WireOp::ContextAppendMessage {
                message: json!({
                    "role": "user",
                    "content": [{"type": "text", "text": "hello"}],
                    "toolCalls": [],
                }),
            },
            user_cancel_op(9),
            reminder_append_op(),
            WireOp::InterruptionReminderRecorded { turn_id: 9 },
        ],
    )
    .await;
    let ops_before = read_wire_log_ops(&env.manager, &session_id).await;

    // When resuming the session
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then the memory holds exactly one reminder (the replayed one — no
    // second copy appended) and the journal is unchanged
    let agent = main_agent(&resumed).await;
    assert_eq!(reminder_count(&agent), 1);
    let ops_after = read_wire_log_ops(&env.manager, &session_id).await;
    assert_eq!(
        ops_after.len(),
        ops_before.len(),
        "a fully delivered journal must not gain any ops on restore"
    );
}

#[tokio::test]
async fn should_skip_reminder_append_but_record_marker_when_journal_crashed_after_durable_append() {
    // Given a persisted session whose journal crashed after the durable
    // reminder append but before the `interruptionReminder.recorded`
    // marker: the replayed pending set holds turn 9 while the
    // reconstructed memory already ends with the reminder
    let env = TestEnv::new().await;
    let session_id = SessionId::new("restore-half-delivered-reminder");
    env.manager
        .persistence()
        .save_state(&session_id, &make_metadata("Half Delivered"))
        .await
        .unwrap();
    write_wire_log(
        &env.manager,
        &session_id,
        &[
            WireOp::Metadata {
                protocol_version: Some("1.5".to_string()),
                created_at: Some(1700000000000),
            },
            user_cancel_op(9),
            reminder_append_op(),
        ],
    )
    .await;
    let ops_before = read_wire_log_ops(&env.manager, &session_id).await;

    // When resuming the session
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then the reminder is not appended a second time — the memory holds
    // exactly one reminder (the replayed one)
    let agent = main_agent(&resumed).await;
    assert_eq!(reminder_count(&agent), 1);

    // And the journal gains exactly one op: the pending turn's
    // `interruptionReminder.recorded` marker, so the next replay starts
    // from a drained set
    let ops_after = read_wire_log_ops(&env.manager, &session_id).await;
    assert_eq!(
        ops_after.len(),
        ops_before.len() + 1,
        "only the missing recorded marker may be appended"
    );
    match ops_after.last() {
        Some(WireOp::InterruptionReminderRecorded { turn_id }) => assert_eq!(*turn_id, 9),
        other => panic!("expected an interruptionReminder.recorded op, got {other:?}"),
    }
}
