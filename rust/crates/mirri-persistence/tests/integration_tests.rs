//! Golden session replay integration tests.
//!
//! These tests verify that the Rust reducer can correctly replay
//! TS-generated `wire.jsonl` files and reach the correct state.
//! They exercise the full pipeline: `JsonlAppendLog::open` → `read_all`
//! → `reduce` → assert on `AgentState`.
//!
//! The golden fixture file (`fixtures/basic-turn/wire.jsonl`) is a
//! handcrafted but realistic wire log that simulates a basic agent turn:
//! metadata → profile.bind → permission.set_mode → turn.prompt →
//! context.append_message → llm.request → loop events → turn.ended →
//! usage.record.

use mirri_persistence::{AppendLogStore, JsonlAppendLog};
use mirri_proto::{AgentState, WireOp, reduce};
use serde_json::json;
use tempfile::TempDir;

/// Helper: replay all ops from a wire.jsonl file through the reducer.
///
/// Opens the file in read-only mode, reads every op, and folds each one
/// through [`reduce`] starting from a fresh [`AgentState`]. Returns the
/// final state and the full list of ops that were read.
async fn replay_wire_file(path: &std::path::Path) -> (AgentState, Vec<WireOp>) {
    let log = JsonlAppendLog::open(path).await.unwrap();
    let ops: Vec<WireOp> = log.read_all().await.unwrap();

    let mut state = AgentState::new();
    for op in &ops {
        let (new_state, _events) = reduce(&state, op);
        state = new_state;
    }
    (state, ops)
}

/// Replay a basic turn session from a golden fixture file.
///
/// This is the primary golden-session test: it replays a realistic
/// wire.jsonl fixture through the reducer and verifies that every domain
/// (metadata, profile, permission, turn, context, usage) reaches the
/// expected final state.
#[tokio::test]
async fn should_reach_correct_final_state_when_replaying_basic_turn_session_from_fixture() {
    let wire_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/basic-turn/wire.jsonl");

    let (state, ops) = replay_wire_file(&wire_path).await;

    // Should have read all ops (13 lines in the fixture).
    assert!(
        ops.len() >= 10,
        "expected at least 10 ops, got {}",
        ops.len()
    );

    // Verify metadata was read (no-op for state).
    assert!(ops.iter().any(|op| matches!(op, WireOp::Metadata { .. })));

    // Verify profile.bind was read.
    assert!(
        ops.iter()
            .any(|op| matches!(op, WireOp::ProfileBind { .. }))
    );

    // Verify permission.set_mode was read and applied.
    assert_eq!(state.permission.mode, "auto");

    // Verify turn.prompt was read and turn counter incremented.
    assert_eq!(state.turn_counter, 1);

    // Verify context.append_message was applied — should have 1 message.
    assert_eq!(
        state.conversation.messages.len(),
        1,
        "should have 1 message from context.append_message"
    );
    assert_eq!(state.conversation.messages[0]["role"], "user");
    assert_eq!(
        state.conversation.messages[0]["content"][0]["text"],
        "read the test file"
    );

    // Verify context.append_loop_event was applied — should have loop events.
    assert!(
        state.conversation.loop_events.len() >= 5,
        "should have at least 5 loop events, got {}",
        state.conversation.loop_events.len()
    );

    // Verify usage.record was applied.
    assert_eq!(state.usage.total_input_tokens, 100);
    assert_eq!(state.usage.total_output_tokens, 20);
}

/// Unknown op types don't break replay.
///
/// Wire logs produced by future TS agents may contain op types the Rust
/// kernel doesn't yet recognise. These should fall through to
/// [`WireOp::Unknown`] and be silently skipped by the reducer — the
/// surrounding known ops should still be applied correctly.
#[tokio::test]
async fn should_skip_unknown_ops_without_breaking_replay_when_wire_log_contains_future_op_types() {
    let wire_content = r#"{"type":"metadata","protocol_version":"1.5","created_at":123}
{"type":"future_unknown_op","data":"something","time":124}
{"type":"context.append_message","message":{"role":"user","content":[{"type":"text","text":"hello"}],"toolCalls":[]},"time":125}
{"type":"another_future_op","field":"value","time":126}
{"type":"turn.ended","turnId":1,"reason":"completed","time":127}
"#;

    let tmp = TempDir::new().unwrap();
    let wire_path = tmp.path().join("wire.jsonl");
    std::fs::write(&wire_path, wire_content).unwrap();

    let log = JsonlAppendLog::open(&wire_path).await.unwrap();
    let ops: Vec<WireOp> = log.read_all().await.unwrap();

    assert_eq!(ops.len(), 5);
    assert!(matches!(ops[1], WireOp::Unknown));
    assert!(matches!(ops[3], WireOp::Unknown));

    // Replay should not panic.
    let mut state = AgentState::new();
    for op in &ops {
        let (new_state, _) = reduce(&state, op);
        state = new_state;
    }

    // User message should still be applied.
    assert_eq!(state.conversation.messages.len(), 1);
    assert_eq!(
        state.conversation.messages[0]["content"][0]["text"],
        "hello"
    );
}

/// Compaction rebuilds the TS legacy summary shape and sets the summary.
///
/// When `context.apply_compaction` is replayed (a TS legacy record — no
/// kept counts), the reducer rebuilds the conversation as
/// `[summary-user-message, ...folded history from compactedCount on]`.
/// Here compactedCount covers the whole folded history, so the rebuild is
/// the summary message alone.
#[tokio::test]
async fn should_clear_messages_and_set_summary_when_replaying_compaction_op() {
    let wire_content = r#"{"type":"metadata","protocol_version":"1.5","created_at":123}
{"type":"context.append_message","message":{"role":"user","content":[{"type":"text","text":"msg 1"}],"toolCalls":[]},"time":124}
{"type":"context.append_message","message":{"role":"assistant","content":[{"type":"text","text":"reply 1"}],"toolCalls":[]},"time":125}
{"type":"context.append_message","message":{"role":"user","content":[{"type":"text","text":"msg 2"}],"toolCalls":[]},"time":126}
{"type":"context.apply_compaction","summary":"conversation about msgs","compactedCount":3,"tokensBefore":500,"tokensAfter":50,"time":127}
"#;

    let tmp = TempDir::new().unwrap();
    let wire_path = tmp.path().join("wire.jsonl");
    std::fs::write(&wire_path, wire_content).unwrap();

    let log = JsonlAppendLog::open(&wire_path).await.unwrap();
    let ops: Vec<WireOp> = log.read_all().await.unwrap();

    let mut state = AgentState::new();
    for op in &ops {
        let (new_state, _) = reduce(&state, op);
        state = new_state;
    }

    // After compaction, the rebuilt conversation is the summary message
    // alone (compactedCount covered the folded history) and the summary
    // text is set.
    assert_eq!(
        state.conversation.messages.len(),
        1,
        "the rebuild is the summary message alone"
    );
    assert_eq!(
        state.conversation.messages[0]["origin"]["kind"],
        "compaction_summary"
    );
    assert_eq!(
        state.conversation.messages[0]["content"][0]["text"],
        "conversation about msgs"
    );
    assert_eq!(
        state.conversation.summary.as_deref(),
        Some("conversation about msgs")
    );
}

/// Goal lifecycle: create → update → clear.
///
/// Exercises the full goal state machine: a goal is created, updated
/// through several status transitions, and finally cleared. After
/// `goal.clear`, the active goal should be `None`.
#[tokio::test]
async fn should_clear_goal_when_replaying_goal_lifecycle_with_goal_clear_op() {
    let wire_content = r#"{"type":"metadata","protocol_version":"1.5","created_at":123}
{"type":"goal.create","goalId":"g1","objective":"refactor auth","status":"active","actor":"user","time":124}
{"type":"goal.update","goalId":"g1","status":"paused","reason":"user requested pause","turnsUsed":5,"tokensUsed":1000,"time":125}
{"type":"goal.update","goalId":"g1","status":"active","reason":"resuming","time":126}
{"type":"goal.update","goalId":"g1","status":"complete","reason":"all tests pass","time":127}
{"type":"goal.clear","time":128}
"#;

    let tmp = TempDir::new().unwrap();
    let wire_path = tmp.path().join("wire.jsonl");
    std::fs::write(&wire_path, wire_content).unwrap();

    let log = JsonlAppendLog::open(&wire_path).await.unwrap();
    let ops: Vec<WireOp> = log.read_all().await.unwrap();

    let mut state = AgentState::new();
    for op in &ops {
        let (new_state, _) = reduce(&state, op);
        state = new_state;
    }

    // Goal should be cleared at the end.
    assert!(state.goal.is_none(), "goal should be cleared");
}

/// Multi-turn session with multiple turns.
///
/// Verifies that the turn counter increments correctly across multiple
/// `turn.prompt` / `turn.ended` cycles and that messages accumulate.
#[tokio::test]
async fn should_increment_turn_counter_and_accumulate_messages_when_replaying_multi_turn_session() {
    let wire_content = r#"{"type":"metadata","protocol_version":"1.5","created_at":123}
{"type":"turn.prompt","input":[{"type":"text","text":"turn 1"}],"origin":{"kind":"user"},"time":124}
{"type":"context.append_message","message":{"role":"user","content":[{"type":"text","text":"turn 1"}],"toolCalls":[]},"time":124}
{"type":"turn.ended","turnId":1,"reason":"completed","time":125}
{"type":"turn.prompt","input":[{"type":"text","text":"turn 2"}],"origin":{"kind":"user"},"time":126}
{"type":"context.append_message","message":{"role":"user","content":[{"type":"text","text":"turn 2"}],"toolCalls":[]},"time":126}
{"type":"turn.ended","turnId":2,"reason":"completed","time":127}
{"type":"turn.prompt","input":[{"type":"text","text":"turn 3"}],"origin":{"kind":"user"},"time":128}
{"type":"context.append_message","message":{"role":"user","content":[{"type":"text","text":"turn 3"}],"toolCalls":[]},"time":128}
{"type":"turn.ended","turnId":3,"reason":"completed","time":129}
"#;

    let tmp = TempDir::new().unwrap();
    let wire_path = tmp.path().join("wire.jsonl");
    std::fs::write(&wire_path, wire_content).unwrap();

    let log = JsonlAppendLog::open(&wire_path).await.unwrap();
    let ops: Vec<WireOp> = log.read_all().await.unwrap();

    let mut state = AgentState::new();
    for op in &ops {
        let (new_state, _) = reduce(&state, op);
        state = new_state;
    }

    assert_eq!(state.turn_counter, 3, "should have 3 turns");
    assert_eq!(
        state.conversation.messages.len(),
        3,
        "should have 3 messages"
    );
}

/// Context undo removes last N messages.
///
/// `context.undo` with `count: 2` should truncate the last 2 messages
/// from the conversation, leaving the earlier ones intact.
#[tokio::test]
async fn should_remove_last_n_messages_when_replaying_context_undo_op() {
    let wire_content = r#"{"type":"metadata","protocol_version":"1.5","created_at":123}
{"type":"context.append_message","message":{"role":"user","content":[{"type":"text","text":"msg 1"}],"toolCalls":[]},"time":124}
{"type":"context.append_message","message":{"role":"assistant","content":[{"type":"text","text":"reply 1"}],"toolCalls":[]},"time":125}
{"type":"context.append_message","message":{"role":"user","content":[{"type":"text","text":"msg 2"}],"toolCalls":[]},"time":126}
{"type":"context.undo","count":2,"time":127}
"#;

    let tmp = TempDir::new().unwrap();
    let wire_path = tmp.path().join("wire.jsonl");
    std::fs::write(&wire_path, wire_content).unwrap();

    let log = JsonlAppendLog::open(&wire_path).await.unwrap();
    let ops: Vec<WireOp> = log.read_all().await.unwrap();

    let mut state = AgentState::new();
    for op in &ops {
        let (new_state, _) = reduce(&state, op);
        state = new_state;
    }

    assert_eq!(
        state.conversation.messages.len(),
        1,
        "should have 1 message after undoing 2"
    );
    assert_eq!(
        state.conversation.messages[0]["content"][0]["text"],
        "msg 1"
    );
}

/// Write ops to wire.jsonl and read them back — full roundtrip.
///
/// This test constructs `WireOp` variants directly in Rust, appends them
/// to a wire log via `JsonlAppendLog::create` / `append`, then reopens
/// the file in read mode and replays through the reducer. It verifies
/// that the serialised form round-trips correctly through the TS wire
/// format (camelCase field names, dotted type discriminators).
#[tokio::test]
async fn should_roundtrip_ops_through_write_and_replay_when_constructing_wire_ops_in_rust() {
    let tmp = TempDir::new().unwrap();
    let wire_path = tmp.path().join("wire.jsonl");
    let mut log = JsonlAppendLog::create(&wire_path).await.unwrap();

    // Write a sequence of ops.
    let ops = vec![
        WireOp::Metadata {
            protocol_version: Some("1.5".to_string()),
            created_at: Some(123),
        },
        WireOp::PermissionSetMode {
            mode: "auto".to_string(),
        },
        WireOp::TurnPrompt {
            input: json!([{"type": "text", "text": "hello"}]),
            origin: json!({"kind": "user"}),
        },
        WireOp::ContextAppendMessage {
            message: json!({"role": "user", "content": [{"type": "text", "text": "hello"}], "toolCalls": []}),
        },
        WireOp::GoalCreate {
            goal_id: "g1".to_string(),
            objective: "test".to_string(),
            completion_criterion: None,
            status: Some("active".to_string()),
            actor: Some("user".to_string()),
            budget_limits: None,
        },
    ];

    for op in &ops {
        log.append(op).await.unwrap();
    }
    drop(log);

    // Read back and replay.
    let log = JsonlAppendLog::open(&wire_path).await.unwrap();
    let read_ops: Vec<WireOp> = log.read_all().await.unwrap();

    assert_eq!(read_ops.len(), ops.len());

    let mut state = AgentState::new();
    for op in &read_ops {
        let (new_state, _) = reduce(&state, op);
        state = new_state;
    }

    // Verify final state.
    assert_eq!(state.permission.mode, "auto");
    assert_eq!(state.turn_counter, 1);
    assert_eq!(state.conversation.messages.len(), 1);
    assert!(state.goal.is_some());
    assert_eq!(state.goal.unwrap().objective, "test");
}

/// 端到端：wire.jsonl 写入 → 回放 → 快照 → 恢复 → 增量回放 → 状态一致
///
/// 这是 PoC-1 的最终集成测试：构建 20 个 op 的 wire log，在第 10 个 op
/// 后写快照，然后模拟重启——从快照恢复 + 增量回放剩余 op——验证最终
/// 状态与连续回放完全一致。
#[tokio::test]
async fn should_match_continuous_replay_state_when_restoring_from_snapshot_and_incremental_replay()
{
    use mirri_persistence::{AppendLogStore, JsonlAppendLog, SnapshotStore};
    use mirri_proto::{AgentState, WireOp, reduce};
    use serde_json::json;

    let tmp = tempfile::TempDir::new().unwrap();
    let wire_path = tmp.path().join("wire.jsonl");
    let snapshot_path = tmp.path().join("snapshot.json");

    // Phase 1: Build state with 20 ops, snapshot at op 10
    let mut log = JsonlAppendLog::create(&wire_path).await.unwrap();
    let mut state = AgentState::new();
    for i in 0..20 {
        let op = WireOp::ContextAppendMessage {
            message: json!({
                "role": "user",
                "content": [{"type": "text", "text": format!("message {i}")}],
                "toolCalls": [],
            }),
        };
        log.append(&op).await.unwrap();
        state = reduce(&state, &op).0;

        // Snapshot at op 10 (after index 9)
        if i == 9 {
            SnapshotStore::write(&snapshot_path, &state, 10)
                .await
                .unwrap();
        }
    }
    drop(log);

    // Phase 2: Simulate restart — restore from snapshot + replay remaining
    let (mut restored_state, snapshot_index) =
        SnapshotStore::read(&snapshot_path).await.unwrap().unwrap();
    assert_eq!(snapshot_index, 10);
    assert_eq!(restored_state.conversation.messages.len(), 10);

    let log = JsonlAppendLog::open(&wire_path).await.unwrap();
    let remaining = log.read_from(snapshot_index as usize).await.unwrap();
    assert_eq!(
        remaining.len(),
        10,
        "should have 10 remaining ops after snapshot"
    );
    for op in &remaining {
        restored_state = reduce(&restored_state, op).0;
    }

    // Phase 3: Verify state matches the continuously-built state
    assert_eq!(
        restored_state.conversation.messages.len(),
        state.conversation.messages.len(),
        "restored state should match continuously-built state"
    );
    for (i, msg) in restored_state.conversation.messages.iter().enumerate() {
        assert_eq!(
            msg["content"][0]["text"], state.conversation.messages[i]["content"][0]["text"],
            "message {i} content mismatch"
        );
    }
    assert_eq!(restored_state.turn_counter, state.turn_counter);
}
