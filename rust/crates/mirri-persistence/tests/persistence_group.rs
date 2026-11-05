// layer: T1
// Domain group binary consolidated from per-file test binaries
// per 16-test-execution-strategy.md §4.4 (pure file moves).
mod append_log_tests {
//! Integration tests for the `wire.jsonl` append log store.
//!
//! These tests exercise the full write → read round-trip against real
//! files in a temp directory, including forward-compatibility (unknown op
//! types) and resilience (a torn final line is dropped, any other
//! unparseable line is reported as corruption).

use mirri_persistence::{AppendLogError, AppendLogStore, JsonlAppendLog};
use mirri_proto::WireOp;
use serde_json::json;
use tempfile::TempDir;

#[tokio::test]
async fn should_read_back_all_ops_when_appending_to_log() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("wire.jsonl");
    let mut log = JsonlAppendLog::create(&path).await.unwrap();

    let ops = vec![
        WireOp::Metadata {
            protocol_version: Some("1.5".to_string()),
            created_at: Some(123),
        },
        WireOp::ContextAppendMessage {
            message: json!({"role": "user", "content": [{"type": "text", "text": "hello"}]}),
        },
        WireOp::TurnPrompt {
            input: json!([{"type": "text", "text": "hello"}]),
            origin: json!({"kind": "user"}),
        },
    ];

    for op in &ops {
        log.append(op).await.unwrap();
    }

    drop(log);

    let log = JsonlAppendLog::open(&path).await.unwrap();
    let read_ops: Vec<WireOp> = log.read_all().await.unwrap();

    assert_eq!(read_ops.len(), 3);
    assert!(matches!(read_ops[0], WireOp::Metadata { .. }));
    assert!(matches!(read_ops[1], WireOp::ContextAppendMessage { .. }));
    assert!(matches!(read_ops[2], WireOp::TurnPrompt { .. }));
}

#[tokio::test]
async fn should_return_ops_from_offset_when_reading_from_offset() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("wire.jsonl");
    let mut log = JsonlAppendLog::create(&path).await.unwrap();

    for i in 0..10 {
        log.append(&WireOp::ContextAppendMessage {
            message: json!({"role": "user", "content": [{"type": "text", "text": format!("msg {i}")}]}),
        })
        .await
        .unwrap();
    }

    drop(log);

    let log = JsonlAppendLog::open(&path).await.unwrap();
    let ops: Vec<WireOp> = log.read_from(5).await.unwrap();

    assert_eq!(ops.len(), 5);
    if let WireOp::ContextAppendMessage { message } = &ops[0] {
        assert_eq!(message["content"][0]["text"], "msg 5");
    } else {
        panic!("expected ContextAppendMessage");
    }
}

#[tokio::test]
async fn should_return_correct_count_when_counting_ops_in_log() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("wire.jsonl");
    let mut log = JsonlAppendLog::create(&path).await.unwrap();

    for _ in 0..5 {
        log.append(&WireOp::TurnPrompt {
            input: json!([{"type": "text", "text": "test"}]),
            origin: json!({"kind": "user"}),
        })
        .await
        .unwrap();
    }

    drop(log);

    let log = JsonlAppendLog::open(&path).await.unwrap();
    assert_eq!(log.count().await.unwrap(), 5);
}

#[tokio::test]
async fn should_not_break_read_when_log_contains_unknown_op_type() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("wire.jsonl");

    // Write a line with an unknown op type (simulating a future TS op).
    std::fs::write(
        &path,
        r#"{"type":"future_op_v2","data":"something","time":123}
{"type":"metadata","protocol_version":"1.5","created_at":123}
{"type":"context.clear","time":124}
"#,
    )
    .unwrap();

    let log = JsonlAppendLog::open(&path).await.unwrap();
    let ops: Vec<WireOp> = log.read_all().await.unwrap();

    assert_eq!(ops.len(), 3);
    assert!(matches!(ops[0], WireOp::Unknown));
    assert!(matches!(ops[1], WireOp::Metadata { .. }));
    assert!(matches!(ops[2], WireOp::ContextClear {}));
}

#[tokio::test]
async fn should_return_corrupted_error_when_log_contains_mid_file_invalid_line() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("wire.jsonl");

    std::fs::write(
        &path,
        r#"{"type":"metadata","protocol_version":"1.5","created_at":123}
THIS IS NOT JSON
{"type":"context.clear","time":124}
"#,
    )
    .unwrap();

    let log = JsonlAppendLog::open(&path).await.unwrap();
    let error = log.read_all().await.unwrap_err();

    // A complete line in the middle of the log that does not parse is
    // corruption, not a torn write — the read must fail loudly.
    let corrupted = error
        .downcast_ref::<AppendLogError>()
        .expect("error must be AppendLogError::Corrupted");
    match corrupted {
        AppendLogError::Corrupted {
            line_number,
            message,
        } => {
            assert_eq!(*line_number, 2);
            assert!(!message.is_empty());
        }
    }
}

#[tokio::test]
async fn should_return_corrupted_error_when_final_line_is_invalid_with_trailing_newline() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("wire.jsonl");

    // The final line is invalid JSON but the file ends with a newline, so
    // the line's write completed — genuine corruption, not a torn write.
    std::fs::write(
        &path,
        "{\"type\":\"context.clear\",\"time\":124}\nNOT JSON\n",
    )
    .unwrap();

    let log = JsonlAppendLog::open(&path).await.unwrap();
    let error = log.read_all().await.unwrap_err();

    let corrupted = error
        .downcast_ref::<AppendLogError>()
        .expect("error must be AppendLogError::Corrupted");
    match corrupted {
        AppendLogError::Corrupted {
            line_number,
            message,
        } => {
            assert_eq!(*line_number, 2);
            assert!(!message.is_empty());
        }
    }
}

#[tokio::test]
async fn should_drop_torn_final_line_when_log_ends_without_newline() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("wire.jsonl");

    // Simulate a crash mid-append: the final line is truncated and has no
    // trailing newline.
    std::fs::write(
        &path,
        concat!(
            r#"{"type":"metadata","protocol_version":"1.5","created_at":123}"#,
            "\n",
            r#"{"type":"context.clear","time":124}"#,
            "\n",
            r#"{"type":"context.append_message","message":{"role":"us"#,
        ),
    )
    .unwrap();

    let log = JsonlAppendLog::open(&path).await.unwrap();
    let ops: Vec<WireOp> = log.read_all().await.unwrap();

    assert_eq!(ops.len(), 2, "the torn final line must be dropped");
    assert!(matches!(ops[0], WireOp::Metadata { .. }));
    assert!(matches!(ops[1], WireOp::ContextClear { .. }));
}

#[tokio::test]
async fn should_skip_empty_lines_when_log_contains_blank_lines() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("wire.jsonl");

    std::fs::write(
        &path,
        r#"{"type":"metadata","protocol_version":"1.5","created_at":123}

{"type":"context.clear"}

"#,
    )
    .unwrap();

    let log = JsonlAppendLog::open(&path).await.unwrap();
    let ops: Vec<WireOp> = log.read_all().await.unwrap();

    assert_eq!(ops.len(), 2);
}

#[tokio::test]
async fn should_preserve_all_op_fields_when_roundtripping_through_log() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("wire.jsonl");
    let mut log = JsonlAppendLog::create(&path).await.unwrap();

    let original = WireOp::GoalCreate {
        goal_id: "g1".to_string(),
        objective: "test objective".to_string(),
        completion_criterion: Some("all tests pass".to_string()),
        status: Some("active".to_string()),
        actor: Some("user".to_string()),
        budget_limits: None,
    };

    log.append(&original).await.unwrap();
    drop(log);

    let log = JsonlAppendLog::open(&path).await.unwrap();
    let ops = log.read_all().await.unwrap();

    assert_eq!(ops.len(), 1);
    match &ops[0] {
        WireOp::GoalCreate {
            goal_id,
            objective,
            completion_criterion,
            status,
            ..
        } => {
            assert_eq!(goal_id, "g1");
            assert_eq!(objective, "test objective");
            assert_eq!(completion_criterion.as_deref(), Some("all tests pass"));
            assert_eq!(status.as_deref(), Some("active"));
        }
        _ => panic!("expected GoalCreate"),
    }
}

#[tokio::test]
async fn should_read_correctly_when_log_uses_ts_format_with_time_fields() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("wire.jsonl");

    // Simulate a real TS wire.jsonl with time fields.
    std::fs::write(
        &path,
        r#"{"type":"metadata","protocol_version":"1.5","created_at":1700000000000}
{"type":"turn.prompt","input":[{"type":"text","text":"hello"}],"origin":{"kind":"user"},"time":1700000001000}
{"type":"context.append_message","message":{"role":"user","content":[{"type":"text","text":"hello"}],"toolCalls":[]},"time":1700000001001}
{"type":"context.append_loop_event","event":{"type":"step.begin","uuid":"step-1","turnId":"turn-1"},"time":1700000001002}
{"type":"turn.ended","turnId":1,"reason":"completed","durationMs":500,"time":1700000002000}
"#,
    )
    .unwrap();

    let log = JsonlAppendLog::open(&path).await.unwrap();
    let ops: Vec<WireOp> = log.read_all().await.unwrap();

    assert_eq!(ops.len(), 5);
    assert!(matches!(ops[0], WireOp::Metadata { .. }));
    assert!(matches!(ops[1], WireOp::TurnPrompt { .. }));
    assert!(matches!(ops[2], WireOp::ContextAppendMessage { .. }));
    assert!(matches!(ops[3], WireOp::ContextAppendLoopEvent { .. }));
    assert!(matches!(ops[4], WireOp::TurnEnded { .. }));
}

#[tokio::test]
async fn should_return_error_when_appending_to_read_only_handle() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("wire.jsonl");

    // Create an empty file then open it read-only.
    std::fs::write(&path, "").unwrap();
    let mut log = JsonlAppendLog::open(&path).await.unwrap();

    let result = log.append(&WireOp::ContextClear {}).await;
    assert!(result.is_err(), "append on a read-only handle should fail");
}

#[tokio::test]
async fn should_return_backing_file_path_when_calling_path_accessor() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("wire.jsonl");
    let log = JsonlAppendLog::create(&path).await.unwrap();

    assert_eq!(log.path(), path);
}

}

mod atomic_doc_tests {
use mirri_persistence::AtomicDocument;
use serde::{Deserialize, Serialize};
use tempfile::TempDir;

#[derive(Serialize, Deserialize, PartialEq, Debug)]
struct TestState {
    version: u32,
    name: String,
}

#[tokio::test]
async fn should_return_same_data_when_writing_and_reading_back() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("state.json");

    let state = TestState {
        version: 1,
        name: "test".to_string(),
    };
    AtomicDocument::write(&path, &state).await.unwrap();

    let read: TestState = AtomicDocument::read(&path).await.unwrap().unwrap();
    assert_eq!(read, state);
}

#[tokio::test]
async fn should_return_none_when_reading_nonexistent_file() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("nonexistent.json");
    let result: Option<TestState> = AtomicDocument::read(&path).await.unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn should_replace_existing_content_when_writing_to_existing_file() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("state.json");

    let v1 = TestState {
        version: 1,
        name: "v1".to_string(),
    };
    AtomicDocument::write(&path, &v1).await.unwrap();

    let v2 = TestState {
        version: 2,
        name: "v2".to_string(),
    };
    AtomicDocument::write(&path, &v2).await.unwrap();

    let read: TestState = AtomicDocument::read(&path).await.unwrap().unwrap();
    assert_eq!(read, v2);
}

#[tokio::test]
async fn should_not_corrupt_existing_file_when_temp_write_is_abandoned_before_rename() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("state.json");

    // Write initial state.
    let initial = TestState {
        version: 1,
        name: "initial".to_string(),
    };
    AtomicDocument::write(&path, &initial).await.unwrap();

    // Simulate crash: write a temp file but don't rename.
    let tmp_path = path.with_extension("json.tmp");
    tokio::fs::write(&tmp_path, b"partial").await.unwrap();

    // Original should still be intact.
    let read: TestState = AtomicDocument::read(&path).await.unwrap().unwrap();
    assert_eq!(read, initial);
}

#[tokio::test]
async fn should_create_parent_directories_when_writing_to_nested_path() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("nested/dir/state.json");

    let state = TestState {
        version: 1,
        name: "nested".to_string(),
    };
    AtomicDocument::write(&path, &state).await.unwrap();

    let read: TestState = AtomicDocument::read(&path).await.unwrap().unwrap();
    assert_eq!(read, state);
}

#[tokio::test]
async fn should_remove_temp_file_when_write_completes_successfully() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("state.json");
    let tmp_path = path.with_extension("json.tmp");

    let state = TestState {
        version: 1,
        name: "test".to_string(),
    };
    AtomicDocument::write(&path, &state).await.unwrap();

    // Temp file should not exist after successful write.
    assert!(
        !tmp_path.exists(),
        "temp file should be cleaned up after rename"
    );
    // Main file should exist.
    assert!(path.exists(), "main file should exist");
}

#[tokio::test]
async fn should_return_none_when_reading_empty_file() {
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("empty.json");
    tokio::fs::write(&path, b"").await.unwrap();

    let result: Option<TestState> = AtomicDocument::read(&path).await.unwrap();
    assert!(result.is_none());
}

}

mod integration_tests {
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

}

mod migration_integration_tests {
//! Migration + persistence end-to-end integration tests.
//!
//! These tests exercise the full migration pipeline: fixture `wire.jsonl`
//! files written in older protocol versions are migrated in-place via
//! [`migrate_log_file`], then verified for correct record transformations
//! and for the torn-write vs corruption read policy on legacy shapes.

use mirri_persistence::{detect_version, migrate_log_file};
use serde_json::Value;
use tempfile::TempDir;

/// Copy a fixture file into a fresh [`TempDir`] and return the destination path.
///
/// Tests migrate the file in-place, so each run must operate on its own copy
/// to avoid mutating the committed fixture.
fn copy_fixture_to_temp(fixture_rel: &str) -> (TempDir, std::path::PathBuf) {
    let dir = TempDir::new().unwrap();
    let dest = dir.path().join("wire.jsonl");
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(fixture_rel);
    std::fs::copy(&src, &dest).unwrap();
    (dir, dest)
}

/// Read the first non-empty line of a file.
fn read_first_line(path: &std::path::Path) -> String {
    let content = std::fs::read_to_string(path).unwrap();
    content
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap()
        .to_string()
}

// ---------------------------------------------------------------------------
// Test 1: v1.0 fixture migration
// ---------------------------------------------------------------------------

/// Given a v1.0 wire log fixture with nested `function.{name,arguments}`
/// tool-call format, when `migrate_log_file` is called, the file should be
/// rewritten to the current protocol version with flattened tool-call fields.
#[tokio::test]
async fn should_migrate_v1_0_fixture_to_current_version_when_calling_migrate_log_file() {
    let (_dir, path) = copy_fixture_to_temp("tests/fixtures/v1.0-session/wire.jsonl");

    // When — migrate the file.
    let source_version = migrate_log_file(&path).await.unwrap();

    // Then — the source version is reported as "1.0".
    assert_eq!(source_version, Some("1.0".to_string()));

    // Then — the metadata record's protocolVersion is bumped to "2.0".
    let first_line = read_first_line(&path);
    let metadata: Value = serde_json::from_str(&first_line).unwrap();
    assert_eq!(
        metadata.get("protocolVersion").and_then(|v| v.as_str()),
        Some("2.0")
    );

    // Then — the toolCalls in the assistant message have flattened
    // `name` / `arguments` fields (no nested `function` object).
    let content = std::fs::read_to_string(&path).unwrap();
    let assistant_line = content
        .lines()
        .find(|l| l.contains("\"assistant\""))
        .unwrap();
    let assistant: Value = serde_json::from_str(assistant_line).unwrap();

    let tool_calls = assistant
        .get("toolCalls")
        .and_then(|v| v.as_array())
        .unwrap();
    assert_eq!(tool_calls.len(), 1);

    let call = &tool_calls[0];
    assert_eq!(
        call.get("name").and_then(|v| v.as_str()),
        Some("get_weather")
    );
    assert_eq!(
        call.get("arguments").and_then(|v| v.as_str()),
        Some("{\"city\":\"SF\"}")
    );
    assert!(
        call.get("function").is_none(),
        "nested `function` object should be removed after migration"
    );
}

// ---------------------------------------------------------------------------
// Test 2: v1.3 fixture migration
// ---------------------------------------------------------------------------

/// Given a v1.3 wire log fixture with `goal.account_usage` and
/// `goal.continuation` ops, when `migrate_log_file` is called, both ops
/// should be rewritten to `goal.update`.
#[tokio::test]
async fn should_migrate_v1_3_fixture_to_current_version_when_calling_migrate_log_file() {
    let (_dir, path) = copy_fixture_to_temp("tests/fixtures/v1.3-session/wire.jsonl");

    // When — migrate the file.
    let source_version = migrate_log_file(&path).await.unwrap();

    // Then — the source version is reported as "1.3".
    assert_eq!(source_version, Some("1.3".to_string()));

    // Then — every line that was `goal.account_usage` or `goal.continuation`
    // is now `goal.update`, under the current `type` discriminator.
    let content = std::fs::read_to_string(&path).unwrap();
    let mut goal_update_count = 0;
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let record: Value = serde_json::from_str(line).unwrap();
        let kind = record
            .get("type")
            .or_else(|| record.get("op"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if kind == "goal.update" {
            goal_update_count += 1;
        }
        // No line should still carry the old op names.
        assert_ne!(kind, "goal.account_usage");
        assert_ne!(kind, "goal.continuation");
    }

    // Two goal ops were in the fixture (account_usage + continuation).
    assert_eq!(
        goal_update_count, 2,
        "expected 2 goal.update ops after migration"
    );
}

// ---------------------------------------------------------------------------
// Test 3: detect v1.4 from envelope-less fixture
// ---------------------------------------------------------------------------

/// Given an envelope-less wire log (no metadata header), when `detect_version`
/// is called on the first line, it should return `Some("1.4")`.
#[tokio::test]
async fn should_detect_v1_4_when_opening_envelope_less_fixture() {
    let fixture_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/envelope-less/wire.jsonl");

    let first_line = read_first_line(&fixture_path);

    let version = detect_version(&first_line);

    assert_eq!(version, Some("1.4".to_string()));
}

// ---------------------------------------------------------------------------
// Test 4: migrate envelope-less fixture
// ---------------------------------------------------------------------------

/// Given an envelope-less wire log fixture, when `migrate_log_file` is called,
/// the file should be migrated and the source version reported as `"1.4"`.
#[tokio::test]
async fn should_migrate_envelope_less_fixture_to_current_version_when_calling_migrate_log_file() {
    let (_dir, path) = copy_fixture_to_temp("tests/fixtures/envelope-less/wire.jsonl");

    let source_version = migrate_log_file(&path).await.unwrap();

    assert_eq!(source_version, Some("1.4".to_string()));
}

// ---------------------------------------------------------------------------
// Test 5: no re-migration when already at current version
// ---------------------------------------------------------------------------

/// Given a wire log already at the current protocol version (1.6), when
/// `migrate_log_file` is called, it should return `None` (no migration needed).
#[tokio::test]
async fn should_not_re_migrate_when_log_is_already_at_current_version() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("wire.jsonl");
    tokio::fs::write(
        &path,
        r#"{"op":"metadata","protocolVersion":"1.7","agentId":"test","createdAt":"2024-01-01T00:00:00Z"}
"#,
    )
    .await
    .unwrap();

    let result = migrate_log_file(&path).await.unwrap();

    assert!(result.is_none(), "expected None for already-current log");
}

// ---------------------------------------------------------------------------
// Test 6: no migration for forward version
// ---------------------------------------------------------------------------

/// Given a wire log with a forward protocol version (1.7, newer than current),
/// when `migrate_log_file` is called, it should return `None`.
#[tokio::test]
async fn should_not_migrate_when_log_is_forward_version() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("wire.jsonl");
    tokio::fs::write(
        &path,
        r#"{"op":"metadata","protocolVersion":"1.7","agentId":"test","createdAt":"2024-01-01T00:00:00Z"}
"#,
    )
    .await
    .unwrap();

    let result = migrate_log_file(&path).await.unwrap();

    assert!(result.is_none(), "expected None for forward-version log");
}

// ---------------------------------------------------------------------------
// Test 7: empty log file is handled gracefully
// ---------------------------------------------------------------------------

/// Given an empty wire log file, when `migrate_log_file` is called, it should
/// return `None` without panicking.
#[tokio::test]
async fn should_return_none_when_migrating_empty_log_file() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("wire.jsonl");
    tokio::fs::write(&path, "").await.unwrap();

    let result = migrate_log_file(&path).await.unwrap();

    assert!(result.is_none(), "expected None for empty log file");
}

// ---------------------------------------------------------------------------
// Test 8: reading an op-tagged fixture after migration reports corruption
// ---------------------------------------------------------------------------

/// Given a v1.0 wire log fixture (records tagged with the legacy `"op"`
/// discriminator and pre-`message` record shapes), when it is migrated and
/// then opened with `JsonlAppendLog`, the migrated records that are not
/// deserialisable as `WireOp`s must fail the read with
/// `AppendLogError::Corrupted` instead of being silently skipped.
///
/// The fixture predates the current wire protocol by several shape changes
/// (top-level role/content instead of a `message` object), so migration
/// alone cannot make every record parseable — and under the torn-write vs
/// corruption policy such records are genuine corruption, reported loudly.
#[tokio::test]
async fn should_report_corruption_when_reading_migrated_v1_0_fixture_with_legacy_record_shapes() {
    use mirri_persistence::{AppendLogError, AppendLogStore, JsonlAppendLog};

    let (_dir, path) = copy_fixture_to_temp("tests/fixtures/v1.0-session/wire.jsonl");

    // Migrate the fixture to the current protocol version.
    let source_version = migrate_log_file(&path).await.unwrap();
    assert_eq!(source_version, Some("1.0".to_string()));

    // The migrated metadata line is now `type`-tagged and version-bumped.
    let first_line = read_first_line(&path);
    let metadata: Value = serde_json::from_str(&first_line).unwrap();
    assert_eq!(
        metadata.get("type").and_then(|v| v.as_str()),
        Some("metadata")
    );
    assert_eq!(
        metadata.get("protocolVersion").and_then(|v| v.as_str()),
        Some("2.0")
    );

    // Opening the migrated file and reading every line fails with
    // AppendLogError::Corrupted on the first legacy-shape record.
    let log = JsonlAppendLog::open(&path).await.unwrap();
    let error = log.read_all().await.unwrap_err();
    assert!(
        error.downcast_ref::<AppendLogError>().is_some(),
        "reading a migrated log with unparseable legacy records must fail with \
         AppendLogError::Corrupted, got: {error}"
    );
}

}

mod migration_tests {
//! Integration tests for wire protocol version migration.
//!
//! These tests exercise the full migration chain, version detection, and
//! the atomic file-rewrite path using temporary `wire.jsonl` files.

use mirri_persistence::{
    CURRENT_VERSION, WireMigration, detect_version, is_forward_version, migrate_log_file,
    migrate_record, resolve_migrations,
};
use serde_json::json;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// 1. v1.0 → v1.1: flatten function fields
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_flatten_function_fields_when_migrating_v1_0_to_v1_1() {
    // Given: a v1.0 record with nested function.name / function.arguments
    let mut record = json!({
        "op": "context.append_message",
        "toolCalls": [
            { "id": "1", "function": { "name": "get_weather", "arguments": "{\"city\":\"SF\"}" } }
        ]
    });

    // When: the v1.0→v1.1 migration is applied
    let migration = mirri_persistence::migration::MigrateV1_0ToV1_1;
    migration.migrate_record(&mut record).unwrap();

    // Then: function.name and function.arguments are flattened to top-level
    assert_eq!(
        record,
        json!({
            "op": "context.append_message",
            "toolCalls": [
                { "id": "1", "name": "get_weather", "arguments": "{\"city\":\"SF\"}" }
            ]
        })
    );
}

// ---------------------------------------------------------------------------
// 2. v1.1 → v1.2: derive sessionApprovalRule
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_derive_session_approval_rule_when_migrating_v1_1_to_v1_2() {
    // Given: a v1.1 permission record with action + toolName
    let mut record = json!({
        "op": "permission.record_approval_result",
        "action": "allow",
        "toolName": "bash"
    });

    // When: the v1.1→v1.2 migration is applied
    let migration = mirri_persistence::migration::MigrateV1_1ToV1_2;
    migration.migrate_record(&mut record).unwrap();

    // Then: sessionApprovalRule is derived from action + toolName
    assert_eq!(
        record.get("sessionApprovalRule"),
        Some(&json!({ "rule": "allow", "toolName": "bash" }))
    );
}

// ---------------------------------------------------------------------------
// 3. v1.2 → v1.3: no-op
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_be_noop_when_migrating_v1_2_to_v1_3() {
    // Given: an arbitrary record
    let mut record = json!({
        "op": "goal.create",
        "goalId": "g-1",
        "field": 42
    });
    let original = record.clone();

    // When: the v1.2→v1.3 migration is applied
    let migration = mirri_persistence::migration::MigrateV1_2ToV1_3;
    migration.migrate_record(&mut record).unwrap();

    // Then: the record is unchanged
    assert_eq!(record, original);
}

// ---------------------------------------------------------------------------
// 4. v1.3 → v1.4: merge goal types
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_merge_goal_types_when_migrating_v1_3_to_v1_4() {
    // Given: v1.3 records with goal.account_usage and goal.continuation
    let mut account_usage = json!({ "op": "goal.account_usage", "tokens": 100 });
    let mut continuation = json!({ "op": "goal.continuation", "text": "continue" });
    let mut update_with_goal_id = json!({
        "op": "goal.update",
        "goalId": "g-2",
        "status": "active"
    });
    let mut create = json!({ "op": "goal.create", "goalId": "g-3" });

    let migration = mirri_persistence::migration::MigrateV1_3ToV1_4;

    // When: the migration is applied to each
    migration.migrate_record(&mut account_usage).unwrap();
    migration.migrate_record(&mut continuation).unwrap();
    migration.migrate_record(&mut update_with_goal_id).unwrap();
    migration.migrate_record(&mut create).unwrap();

    // Then: account_usage and continuation become goal.update
    assert_eq!(account_usage["op"], "goal.update");
    assert_eq!(account_usage["tokens"], 100);
    assert_eq!(continuation["op"], "goal.update");
    assert_eq!(continuation["text"], "continue");

    // And: goalId is removed from goal.update but preserved on goal.create
    assert!(update_with_goal_id.get("goalId").is_none());
    assert_eq!(update_with_goal_id["status"], "active");
    assert_eq!(create["goalId"], "g-3");
}

// ---------------------------------------------------------------------------
// 5. v1.4 → v1.5: backfill wallClockResumedAt
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_backfill_wall_clock_resumed_at_when_migrating_v1_4_to_v1_5() {
    // Given: v1.4 goal records without wallClockResumedAt
    let mut create = json!({
        "op": "goal.create",
        "time": "2024-01-01T00:00:00Z"
    });
    let mut update = json!({
        "op": "goal.update",
        "time": "2024-02-01T00:00:00Z"
    });
    let mut existing = json!({
        "op": "goal.create",
        "time": "2024-01-01T00:00:00Z",
        "wallClockResumedAt": "2024-06-01T00:00:00Z"
    });

    let migration = mirri_persistence::migration::MigrateV1_4ToV1_5;

    // When: the migration is applied
    migration.migrate_record(&mut create).unwrap();
    migration.migrate_record(&mut update).unwrap();
    migration.migrate_record(&mut existing).unwrap();

    // Then: wallClockResumedAt is backfilled from time where absent
    assert_eq!(create["wallClockResumedAt"], "2024-01-01T00:00:00Z");
    assert_eq!(update["wallClockResumedAt"], "2024-02-01T00:00:00Z");
    // And: existing wallClockResumedAt is preserved
    assert_eq!(existing["wallClockResumedAt"], "2024-06-01T00:00:00Z");
}

// ---------------------------------------------------------------------------
// 6. resolve_migrations: current version
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_return_empty_migrations_when_source_is_current_version() {
    // Given: the current protocol version
    // When: migrations are resolved
    let chain = resolve_migrations(CURRENT_VERSION);

    // Then: no migrations are needed
    assert!(chain.is_empty());
}

// ---------------------------------------------------------------------------
// 7. resolve_migrations: forward version
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_return_empty_migrations_when_source_is_forward_version() {
    // Given: a version newer than current
    // When: migrations are resolved
    let chain = resolve_migrations("1.7");

    // Then: no migrations are returned (forward-version logs are not migrated)
    assert!(chain.is_empty());
}

// ---------------------------------------------------------------------------
// 8. detect_version: envelope-less log → v1.4
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_detect_v1_4_when_first_line_is_not_metadata() {
    // Given: a first line that is a regular op, not a metadata record
    let line = r#"{"op":"goal.create","goalId":"g-1"}"#;

    // When: detecting the version
    let version = detect_version(line);

    // Then: v1.4 is assumed for envelope-less logs
    assert_eq!(version, Some("1.4".to_string()));
}

// ---------------------------------------------------------------------------
// 9. detect_version: metadata record
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_detect_version_when_metadata_record_present() {
    // Given: a metadata header line with protocolVersion
    let line = r#"{"op":"metadata","protocolVersion":"1.2"}"#;

    // When: detecting the version
    let version = detect_version(line);

    // Then: the version is read from protocolVersion
    assert_eq!(version, Some("1.2".to_string()));
}

// ---------------------------------------------------------------------------
// 10. Full chain: v1.0 → v1.5
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_migrate_full_chain_when_migrating_from_v1_0_to_v1_5() {
    // Given: a v1.0 record that is affected by multiple migrations in the chain
    let mut record = json!({
        "op": "goal.create",
        "goalId": "g-chain",
        "time": "2024-03-01T00:00:00Z"
    });

    // And: a v1.0 toolCalls record
    let mut tool_call_record = json!({
        "op": "context.append_message",
        "toolCalls": [
            { "id": "tc1", "function": { "name": "search", "arguments": "{\"q\":\"hi\"}" } }
        ]
    });

    // When: the full migration chain from v1.0 is resolved and applied
    // (now seven segments: 1.0→…→1.6 plus the identity 1.6→2.0 era re-stamp)
    let chain = resolve_migrations("1.0");
    assert_eq!(chain.len(), 7);
    migrate_record(&mut record, &chain).unwrap();
    migrate_record(&mut tool_call_record, &chain).unwrap();

    // Then: the goal record keeps goalId (create), gains wallClockResumedAt
    assert_eq!(record["op"], "goal.create");
    assert_eq!(record["goalId"], "g-chain");
    assert_eq!(record["wallClockResumedAt"], "2024-03-01T00:00:00Z");

    // And: the toolCalls record has flattened function fields
    assert_eq!(tool_call_record["toolCalls"][0]["name"], "search");
    assert_eq!(
        tool_call_record["toolCalls"][0]["arguments"],
        "{\"q\":\"hi\"}"
    );
    assert!(tool_call_record["toolCalls"][0].get("function").is_none());
}

// ---------------------------------------------------------------------------
// 11. migrate_log_file: atomic rewrite
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_atomically_rewrite_file_when_migrating_log_file() {
    // Given: a wire.jsonl file at v1.0 with a metadata header and a record
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("wire.jsonl");

    let metadata_line = r#"{"op":"metadata","protocolVersion":"1.0"}"#;
    let record_line = r#"{"op":"goal.create","goalId":"g-file","time":"2024-01-01T00:00:00Z"}"#;
    let content = format!("{metadata_line}\n{record_line}\n");
    tokio::fs::write(&path, &content).await.unwrap();

    // When: the log file is migrated
    let source_version = migrate_log_file(&path).await.unwrap();

    // Then: the source version is reported
    assert_eq!(source_version, Some("1.0".to_string()));

    // And: the file has been rewritten with the current protocol version
    let migrated = tokio::fs::read_to_string(&path).await.unwrap();
    let lines: Vec<&str> = migrated.lines().collect();
    assert_eq!(lines.len(), 2);

    let metadata: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    assert_eq!(metadata["type"], "metadata");
    assert_eq!(metadata["protocolVersion"], CURRENT_VERSION);
    assert_eq!(
        metadata["protocol_version"], CURRENT_VERSION,
        "the TS spelling of the version field is written too"
    );

    let record: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(record["type"], "goal.create");
    assert_eq!(record["goalId"], "g-file");
    assert_eq!(record["wallClockResumedAt"], "2024-01-01T00:00:00Z");

    // And: the temp file is gone
    let tmp_path = path.with_extension("jsonl.tmp");
    assert!(
        !tmp_path.exists(),
        "temp file should be cleaned up after rename"
    );
}

// ---------------------------------------------------------------------------
// 12. migrate_log_file: already current → no-op
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_skip_migration_when_log_is_already_current() {
    // Given: a wire.jsonl file already at the current version
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("wire.jsonl");

    let metadata_line = format!(r#"{{"op":"metadata","protocolVersion":"{CURRENT_VERSION}"}}"#);
    let record_line = r#"{"op":"goal.create","goalId":"g-current"}"#;
    let content = format!("{metadata_line}\n{record_line}\n");
    tokio::fs::write(&path, &content).await.unwrap();

    // When: migrate_log_file is called
    let result = migrate_log_file(&path).await.unwrap();

    // Then: no migration is performed
    assert_eq!(result, None);

    // And: the file content is unchanged
    let after = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(after, content);
}

// ---------------------------------------------------------------------------
// 13. migrate_log_file: forward version → no-op
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_not_migrate_when_log_is_forward_version() {
    // Given: a wire.jsonl file with a forward protocol version
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("wire.jsonl");

    let metadata_line = r#"{"op":"metadata","protocolVersion":"2.1"}"#;
    let record_line = r#"{"op":"goal.create","goalId":"g-fwd"}"#;
    let content = format!("{metadata_line}\n{record_line}\n");
    tokio::fs::write(&path, &content).await.unwrap();

    // When: migrate_log_file is called
    let result = migrate_log_file(&path).await.unwrap();

    // Then: no migration is performed
    assert_eq!(result, None);

    // And: the file content is unchanged
    let after = tokio::fs::read_to_string(&path).await.unwrap();
    assert_eq!(after, content);

    // Sanity: is_forward_version agrees
    assert!(is_forward_version("2.1"));
}

}

mod query_store_tests {
//! Integration tests for the indexed query store.
//!
//! Verifies the [`IQueryStore`] contract against the in-memory
//! implementation: put/get round-trips, deletion, prefixed listing,
//! atomic batches, checkpoint persistence, and value overwrite
//! semantics.

use mirri_persistence::{IQueryStore, InMemoryQueryStore, QueryCheckpoint, WriteOp};
use serde_json::json;

/// Helper: build an empty in-memory query store.
fn make_store() -> InMemoryQueryStore {
    InMemoryQueryStore::new()
}

#[test]
fn should_store_and_retrieve_value_when_put_and_get_called() {
    let store = make_store();
    let value = json!({"role": "user", "content": "hello"});

    store.put("messages", "msg-1", value.clone()).unwrap();

    let retrieved = store.get("messages", "msg-1").unwrap();
    assert_eq!(retrieved, Some(value));
}

#[test]
fn should_return_none_when_key_does_not_exist() {
    let store = make_store();

    let missing_key = store.get("messages", "no-such-key").unwrap();
    assert_eq!(missing_key, None);

    let missing_collection = store.get("no-such-collection", "msg-1").unwrap();
    assert_eq!(missing_collection, None);
}

#[test]
fn should_delete_value_when_delete_called() {
    let store = make_store();
    store
        .put("messages", "msg-1", json!({"content": "hello"}))
        .unwrap();

    store.delete("messages", "msg-1").unwrap();

    let retrieved = store.get("messages", "msg-1").unwrap();
    assert_eq!(retrieved, None);
}

#[test]
fn should_list_keys_filtered_by_prefix_when_prefix_provided() {
    let store = make_store();
    store.put("sessions", "session-1", json!({})).unwrap();
    store.put("sessions", "session-2", json!({})).unwrap();
    store.put("sessions", "summary-1", json!({})).unwrap();

    let all_keys = store.list("sessions", None).unwrap();
    assert_eq!(
        all_keys,
        vec!["session-1", "session-2", "summary-1"],
        "keys should be listed in lexicographic order"
    );

    let prefixed_keys = store.list("sessions", Some("session-")).unwrap();
    assert_eq!(prefixed_keys, vec!["session-1", "session-2"]);

    let no_match = store.list("sessions", Some("nope-")).unwrap();
    assert!(no_match.is_empty());

    let unknown_collection = store.list("no-such-collection", None).unwrap();
    assert!(unknown_collection.is_empty());
}

#[test]
fn should_execute_batch_atomically_when_batch_called() {
    let store = make_store();
    store
        .put("messages", "msg-keep", json!({"content": "keep me"}))
        .unwrap();

    let ops = vec![
        WriteOp::Put {
            collection: "messages".to_string(),
            key: "msg-a".to_string(),
            value: json!({"content": "batch a"}),
        },
        WriteOp::Put {
            collection: "messages".to_string(),
            key: "msg-b".to_string(),
            value: json!({"content": "batch b"}),
        },
        WriteOp::Delete {
            collection: "messages".to_string(),
            key: "msg-keep".to_string(),
        },
    ];
    store.batch(&ops).unwrap();

    // Every operation in the batch took effect.
    assert_eq!(
        store.get("messages", "msg-a").unwrap(),
        Some(json!({"content": "batch a"}))
    );
    assert_eq!(
        store.get("messages", "msg-b").unwrap(),
        Some(json!({"content": "batch b"}))
    );
    assert_eq!(store.get("messages", "msg-keep").unwrap(), None);

    // An empty batch is a harmless no-op.
    store.batch(&[]).unwrap();
    assert_eq!(
        store.get("messages", "msg-a").unwrap(),
        Some(json!({"content": "batch a"}))
    );
}

#[test]
fn should_persist_checkpoint_when_set_checkpoint_called() {
    let store = make_store();
    let checkpoint = QueryCheckpoint { seq: 42 };

    store
        .set_checkpoint("wire-log", checkpoint.clone())
        .unwrap();

    let retrieved = store.get_checkpoint("wire-log").unwrap();
    assert_eq!(retrieved, Some(checkpoint));
}

#[test]
fn should_return_none_when_checkpoint_not_set() {
    let store = make_store();

    let checkpoint = store.get_checkpoint("wire-log").unwrap();
    assert_eq!(checkpoint, None);
}

#[test]
fn should_overwrite_value_when_same_key_put_twice() {
    let store = make_store();
    store
        .put("messages", "msg-1", json!({"version": 1}))
        .unwrap();

    store
        .put("messages", "msg-1", json!({"version": 2}))
        .unwrap();

    let retrieved = store.get("messages", "msg-1").unwrap();
    assert_eq!(retrieved, Some(json!({"version": 2})), "latest put wins");
}

}

mod snapshot_tests {
//! Snapshot store integration tests.
//!
//! Verifies that:
//! - A snapshot can be written and read back with full state fidelity.
//! - Incremental replay from a snapshot's op_index produces the same
//!   state as a continuous full replay.
//! - When no snapshot exists, the caller can fall back to a full replay.

use mirri_persistence::{AppendLogStore, JsonlAppendLog, SnapshotStore};
use mirri_proto::{AgentState, WireOp, reduce};
use serde_json::json;
use tempfile::TempDir;

/// Helper: build a `context.append_message` op with a simple text message.
fn make_message_op(content: &str) -> WireOp {
    WireOp::ContextAppendMessage {
        message: json!({
            "role": "user",
            "content": content,
        }),
    }
}

/// Helper: extract message content from an AgentState for assertions.
fn message_content(state: &AgentState, index: usize) -> String {
    state.conversation.messages[index]
        .get("content")
        .and_then(|c| c.as_str())
        .unwrap_or("")
        .to_string()
}

#[tokio::test]
async fn should_restore_full_state_when_writing_and_reading_snapshot() {
    let tmp = TempDir::new().unwrap();
    let wire_path = tmp.path().join("wire.jsonl");
    let snapshot_path = tmp.path().join("snapshot.json");

    // Build up state with 5 ops
    let mut log = JsonlAppendLog::create(&wire_path).await.unwrap();
    let mut state = AgentState::new();
    for i in 0..5 {
        let op = make_message_op(&format!("msg {i}"));
        log.append(&op).await.unwrap();
        state = reduce(&state, &op).0;
    }
    drop(log);

    // Write snapshot at op index 5
    SnapshotStore::write(&snapshot_path, &state, 5)
        .await
        .unwrap();

    // Restore from snapshot
    let (restored_state, op_index) = SnapshotStore::read(&snapshot_path).await.unwrap().unwrap();

    assert_eq!(restored_state.conversation.messages.len(), 5);
    assert_eq!(op_index, 5);
    // Verify content fidelity
    for i in 0..5 {
        assert_eq!(message_content(&restored_state, i), format!("msg {i}"));
    }
}

#[tokio::test]
async fn should_match_full_replay_state_when_restoring_from_snapshot_and_incremental_replay() {
    let tmp = TempDir::new().unwrap();
    let wire_path = tmp.path().join("wire.jsonl");
    let snapshot_path = tmp.path().join("snapshot.json");

    // Write 10 ops, snapshot at op 5
    let mut log = JsonlAppendLog::create(&wire_path).await.unwrap();
    let mut state = AgentState::new();
    for i in 0..10 {
        let op = make_message_op(&format!("msg {i}"));
        log.append(&op).await.unwrap();
        state = reduce(&state, &op).0;
        if i == 4 {
            // Snapshot after 5th op (index 4, op_index 5)
            SnapshotStore::write(&snapshot_path, &state, 5)
                .await
                .unwrap();
        }
    }
    drop(log);

    // Restore: snapshot (5 ops) + replay remaining 5 ops
    let (mut state, snapshot_op_index) =
        SnapshotStore::read(&snapshot_path).await.unwrap().unwrap();
    assert_eq!(snapshot_op_index, 5);
    assert_eq!(state.conversation.messages.len(), 5);

    let log = JsonlAppendLog::open(&wire_path).await.unwrap();
    let remaining_ops = log.read_from(snapshot_op_index as usize).await.unwrap();
    assert_eq!(remaining_ops.len(), 5);

    for op in &remaining_ops {
        state = reduce(&state, op).0;
    }

    // Should have all 10 messages
    assert_eq!(state.conversation.messages.len(), 10);
    assert_eq!(message_content(&state, 5), "msg 5");
    assert_eq!(message_content(&state, 9), "msg 9");
}

#[tokio::test]
async fn should_return_none_when_no_snapshot_exists() {
    let tmp = TempDir::new().unwrap();
    let wire_path = tmp.path().join("wire.jsonl");
    let snapshot_path = tmp.path().join("nonexistent.json");

    let mut log = JsonlAppendLog::create(&wire_path).await.unwrap();
    for i in 0..3 {
        log.append(&make_message_op(&format!("msg {i}")))
            .await
            .unwrap();
    }
    drop(log);

    // No snapshot exists — should return None
    let result = SnapshotStore::read(&snapshot_path).await.unwrap();
    assert!(result.is_none());

    // Full replay
    let log = JsonlAppendLog::open(&wire_path).await.unwrap();
    let ops = log.read_all().await.unwrap();
    let mut state = AgentState::new();
    for op in &ops {
        state = reduce(&state, op).0;
    }
    assert_eq!(state.conversation.messages.len(), 3);
}

#[tokio::test]
async fn should_not_corrupt_existing_snapshot_when_overwriting_with_new_snapshot() {
    let tmp = TempDir::new().unwrap();
    let snapshot_path = tmp.path().join("snapshot.json");

    // Write initial snapshot
    let state1 = AgentState::new();
    SnapshotStore::write(&snapshot_path, &state1, 1)
        .await
        .unwrap();
    let (_restored1, idx1) = SnapshotStore::read(&snapshot_path).await.unwrap().unwrap();
    assert_eq!(idx1, 1);

    // Overwrite with a new snapshot
    let mut state2 = AgentState::new();
    let op = make_message_op("overwrite");
    state2 = reduce(&state2, &op).0;
    SnapshotStore::write(&snapshot_path, &state2, 2)
        .await
        .unwrap();

    // Should read the new snapshot
    let (restored2, idx2) = SnapshotStore::read(&snapshot_path).await.unwrap().unwrap();
    assert_eq!(idx2, 2);
    assert_eq!(restored2.conversation.messages.len(), 1);
    assert_eq!(message_content(&restored2, 0), "overwrite");
}

}

