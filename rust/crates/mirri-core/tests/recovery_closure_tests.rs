//! Integration tests for the synthetic interrupted turn.ended closure
//! (plan: interrupted-task-recovery, Task 3).
//!
//! Task 3 writes the recovery backfill: a wire log whose tail ends with a
//! dangling `turn.prompt` (the process died mid-turn) gains a synthetic
//! `turn.ended { reason: "interrupted", error: { cause } }` whose turnId is
//! the dangling prompt's ordinal (ADR-009). The resume test then proves the
//! closure replays through a real `SessionManager` resume without error —
//! a closure landing between prompts is a normal append order for the
//! replay's out-of-order tolerance.
//!
//! Tests use the Given-When-Then naming convention.

use std::collections::HashSet;

use mirri_core::{
    AgentId, AgentScopeConfig, AppScope, AppScopeConfig, SessionId, SessionManager,
    SessionMetadata, WorkspaceId, WorkspaceScopeConfig, WorkspaceTrustLevel,
    session::recovery_scan::append_interrupted_closure,
};
use mirri_persistence::{AppendLogStore, JsonlAppendLog};
use mirri_proto::WireOp;
use serde_json::json;

// ===========================================================================
// Helpers (same scaffolding as restore_interruption_tests.rs)
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
/// logs here carry no model record, so the base config seeds the model).
fn resume_base_config() -> AgentScopeConfig {
    AgentScopeConfig {
        model_id: "test-resume-model".to_string(),
        ..AgentScopeConfig::default()
    }
}

/// Write a sequence of wire ops to the session's `wire.jsonl`
/// (truncating any existing file). The `time` stamps on these ops are
/// injected by the append layer itself (TS parity).
async fn write_wire_log(manager: &SessionManager, session_id: &SessionId, ops: &[WireOp]) {
    let wire_log_path = manager.persistence().wire_log_path(session_id);
    let mut log = JsonlAppendLog::create(&wire_log_path).await.unwrap();
    for op in ops {
        log.append(op).await.unwrap();
    }
}

fn turn_prompt_op() -> WireOp {
    WireOp::TurnPrompt {
        input: json!({ "content": [] }),
        origin: json!({ "kind": "user" }),
    }
}

fn turn_ended_op(turn_id: u64) -> WireOp {
    WireOp::TurnEnded {
        turn_id,
        reason: "completed".to_string(),
        error: None,
        duration_ms: None,
    }
}

// ===========================================================================
// The synthetic closure record
// ===========================================================================

#[tokio::test]
async fn should_append_interrupted_turn_ended_with_prompt_ordinal() {
    // Given a dangling-prompt wire log (one closed turn plus one prompt the
    // process never closed)
    let tmp = tempfile::tempdir().unwrap();
    let wire_log_path = tmp.path().join("wire.jsonl");
    let mut log = JsonlAppendLog::create(&wire_log_path).await.unwrap();
    log.append(&turn_prompt_op()).await.unwrap();
    log.append(&turn_ended_op(1)).await.unwrap();
    log.append(&turn_prompt_op()).await.unwrap();
    log.close().await;

    // When the synthetic interrupted closure is appended for ordinal 3
    append_interrupted_closure(&wire_log_path, 3, "service_exit")
        .await
        .unwrap();

    // Then the LAST line is a turn.ended closing prompt 3 as interrupted —
    // parsed-field assertions, not a char-by-char compare (the append layer
    // injects the `time` stamp itself).
    let contents = std::fs::read_to_string(&wire_log_path).unwrap();
    let last_line = contents
        .lines()
        .rfind(|line| !line.trim().is_empty())
        .expect("the wire log must not be empty after the closure");
    let record: serde_json::Value = serde_json::from_str(last_line).unwrap();
    assert_eq!(record["type"], "turn.ended");
    assert_eq!(record["turnId"], 3);
    assert_eq!(record["reason"], "interrupted");
    assert_eq!(record["error"]["cause"], "service_exit");
    assert!(record.get("time").and_then(|t| t.as_u64()).is_some());

    // And the pre-existing records are intact (the closure appends, never
    // truncates — open_for_append semantics)
    let line_count = contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    assert_eq!(line_count, 4);
}

// ===========================================================================
// Replay tolerance: the closure survives a real resume
// ===========================================================================

#[tokio::test]
async fn should_replay_interrupted_closure_without_error() {
    // Given a persisted session whose wire log holds one closed turn and a
    // dangling prompt (time 4000 — the crash shape), then gains the
    // synthetic interrupted closure via the recovery path
    let env = TestEnv::new().await;
    let session_id = SessionId::new("replay-interrupted-closure");
    env.manager
        .persistence()
        .save_state(&session_id, &make_metadata("Interrupted Closure"))
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
            turn_prompt_op(),
            turn_ended_op(1),
            turn_prompt_op(),
        ],
    )
    .await;
    let wire_log_path = env.manager.persistence().wire_log_path(&session_id);
    append_interrupted_closure(&wire_log_path, 2, "service_exit")
        .await
        .unwrap();

    // When the session is resumed, the closure landing between prompts is
    // a normal append order for the replay — the resume must succeed
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await;

    // Then the resume succeeds (no error, the main agent is recreated)
    let resumed =
        resumed.expect("resuming a journal with a synthetic interrupted closure must succeed");
    resumed
        .get_agent(&AgentId::new("main"))
        .await
        .expect("the resumed session must recreate the main agent");
}
