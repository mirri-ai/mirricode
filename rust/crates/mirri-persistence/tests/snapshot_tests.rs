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
