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
