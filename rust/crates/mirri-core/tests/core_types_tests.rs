//! Tests for the core traits and types of `mirri-core`.
//!
//! These verify the load-bearing invariants that later Phase 2 modules rely on:
//! - `Message` / `ContextMessage` serialize to the TS wire format.
//! - `ToolAccess::conflicts` matches the TypeScript tool-access rules.
//! - `PromptOrigin::is_real_user_input` matches the compaction keep-set.
//! - `CancelToken` cancellation propagates to children.
//! - `StopReason::is_loop_continuing` is true only for `ToolUse`.

use mirri_core::{
    AccessKind, AccessOperation, CancelToken, ContextMessage, Message, PromptOrigin, Role,
    StopReason, ToolAccess,
};
use serde_json::json;

// ---------------------------------------------------------------------------
// Message serialization
// ---------------------------------------------------------------------------

#[test]
fn should_serialize_role_as_lowercase_when_user_message_created() {
    let msg = Message::user("hello");
    let v: serde_json::Value = serde_json::to_value(&msg).unwrap();
    assert_eq!(v["role"], "user");
    assert_eq!(v["content"][0]["type"], "text");
    assert_eq!(v["content"][0]["text"], "hello");
    // tool_calls default empty -> skipped
    assert!(v.get("tool_calls").is_none() || v["tool_calls"].as_array().unwrap().is_empty());
    assert!(v.get("tool_call_id").is_none());
    assert!(v.get("partial").is_none());
}

#[test]
fn should_serialize_tool_calls_correctly_when_assistant_message_has_tool_calls() {
    let mut msg = Message::assistant("thinking...");
    msg.tool_calls.push(mirri_core::ToolCall {
        id: "call_abc".into(),
        call_type: "function".into(),
        function: mirri_core::ToolCallFunction {
            name: "Read".into(),
            arguments: r#"{"path":"/tmp/x"}"#.into(),
        },
    });
    let v: serde_json::Value = serde_json::to_value(&msg).unwrap();
    assert_eq!(v["role"], "assistant");
    let tc = &v["tool_calls"][0];
    assert_eq!(tc["id"], "call_abc");
    assert_eq!(tc["type"], "function");
    assert_eq!(tc["function"]["name"], "Read");
    assert_eq!(tc["function"]["arguments"], r#"{"path":"/tmp/x"}"#);
}

#[test]
fn should_serialize_tool_call_id_when_tool_result_message_created() {
    let msg = Message::tool_result("call_abc", "file contents");
    let v: serde_json::Value = serde_json::to_value(&msg).unwrap();
    assert_eq!(v["role"], "tool");
    assert_eq!(v["tool_call_id"], "call_abc");
    assert_eq!(v["content"][0]["text"], "file contents");
}

#[test]
fn should_preserve_fields_when_message_round_trips_through_json() {
    let msg = Message::assistant("hi");
    let json = serde_json::to_string(&msg).unwrap();
    let back: Message = serde_json::from_str(&json).unwrap();
    assert_eq!(back.role, Role::Assistant);
    assert_eq!(back.text_content(), "hi");
}

#[test]
fn should_join_text_parts_when_text_content_called() {
    let mut msg = Message::user("first");
    msg.content.push(mirri_core::ContentPart::Text {
        text: "second".into(),
    });
    msg.content.push(mirri_core::ContentPart::Think {
        text: "ignored".into(),
    });
    assert_eq!(msg.text_content(), "first\nsecond");
}

// ---------------------------------------------------------------------------
// ContextMessage serialization (flatten + origin)
// ---------------------------------------------------------------------------

#[test]
fn should_serialize_origin_as_kind_tag_when_context_message_serialized() {
    let msg = Message::user("do something");
    let cm = ContextMessage::from_message(msg).with_origin(PromptOrigin::User);
    let v: serde_json::Value = serde_json::to_value(&cm).unwrap();

    // Flattened message fields are top-level siblings.
    assert_eq!(v["role"], "user");
    assert_eq!(v["content"][0]["text"], "do something");
    // origin is a tagged enum: {"kind": "user"}
    assert_eq!(v["origin"], json!({"kind": "user"}));
    // is_error / note default -> skipped
    assert!(v.get("is_error").is_none());
    assert!(v.get("note").is_none());
}

#[test]
fn should_preserve_origin_when_context_message_round_trips_through_json() {
    let msg = Message::assistant("result");
    let cm = ContextMessage::from_message(msg).with_origin(PromptOrigin::CompactionSummary);
    let json = serde_json::to_string(&cm).unwrap();
    let back: ContextMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(back.message.role, Role::Assistant);
    assert_eq!(back.origin, Some(PromptOrigin::CompactionSummary));
    assert!(!back.is_error);
}

#[test]
fn should_omit_origin_field_when_context_message_has_no_origin() {
    let cm = ContextMessage::from_message(Message::user("hi"));
    let v: serde_json::Value = serde_json::to_value(&cm).unwrap();
    assert!(v.get("origin").is_none());
}

// ---------------------------------------------------------------------------
// ToolAccess::conflicts
// ---------------------------------------------------------------------------

fn file_read(path: &str) -> ToolAccess {
    ToolAccess {
        kind: AccessKind::File,
        path: Some(path.to_string()),
        operation: AccessOperation::Read,
    }
}

fn file_write(path: &str) -> ToolAccess {
    ToolAccess {
        kind: AccessKind::File,
        path: Some(path.to_string()),
        operation: AccessOperation::Write,
    }
}

#[test]
fn should_not_conflict_when_concurrent_reads_on_same_path() {
    let a = [file_read("/a/b")];
    let b = [file_read("/a/b")];
    assert!(!ToolAccess::conflicts(&a, &b));
}

#[test]
fn should_conflict_when_write_and_read_on_same_path() {
    let a = [file_write("/a/b")];
    let b = [file_read("/a/b")];
    assert!(ToolAccess::conflicts(&a, &b));
}

#[test]
fn should_conflict_when_writes_on_overlapping_paths() {
    let a = [file_write("/a/b")];
    let b = [file_write("/a/b/c")];
    assert!(ToolAccess::conflicts(&a, &b));
}

#[test]
fn should_not_conflict_when_writes_on_disjoint_paths() {
    let a = [file_write("/a/b")];
    let b = [file_write("/x/y")];
    assert!(!ToolAccess::conflicts(&a, &b));
}

#[test]
fn should_conflict_with_everything_when_access_kind_is_all() {
    let all_access = [ToolAccess {
        kind: AccessKind::All,
        path: None,
        operation: AccessOperation::ReadWrite,
    }];
    let file_read_access = [file_read("/anything")];
    assert!(ToolAccess::conflicts(&all_access, &file_read_access));
}

#[test]
fn should_not_conflict_when_access_kinds_are_file_and_search() {
    let file = [file_read("/a/b")];
    let search = [ToolAccess {
        kind: AccessKind::Search,
        path: Some("/a/b".into()),
        operation: AccessOperation::Read,
    }];
    assert!(!ToolAccess::conflicts(&file, &search));
}

// ---------------------------------------------------------------------------
// PromptOrigin::is_real_user_input
// ---------------------------------------------------------------------------

#[test]
fn should_return_true_when_prompt_origin_is_real_user_input() {
    assert!(PromptOrigin::User.is_real_user_input());
    assert!(
        PromptOrigin::SkillActivation {
            activation_id: None,
            skill_name: None,
            trigger: None,
            skill_args: None,
            skill_path: None,
            skill_source: None,
        }
        .is_real_user_input()
    );
    assert!(PromptOrigin::PluginCommand.is_real_user_input());
}

#[test]
fn should_return_false_when_prompt_origin_is_not_real_user_input() {
    assert!(!PromptOrigin::Injection.is_real_user_input());
    assert!(!PromptOrigin::CompactionSummary.is_real_user_input());
    assert!(!PromptOrigin::SystemTrigger.is_real_user_input());
    assert!(
        !PromptOrigin::BackgroundTask {
            task_id: "t".to_string(),
            status: "completed".to_string(),
            notification_id: "task:t:completed".to_string(),
        }
        .is_real_user_input()
    );
    assert!(!PromptOrigin::ShellCommand.is_real_user_input());
}

#[test]
fn should_serialize_as_kind_tag_when_prompt_origin_serialized() {
    let v = serde_json::to_value(PromptOrigin::User).unwrap();
    assert_eq!(v, json!({"kind": "user"}));
    let v = serde_json::to_value(PromptOrigin::CompactionSummary).unwrap();
    assert_eq!(v, json!({"kind": "compaction_summary"}));
}

// ---------------------------------------------------------------------------
// CancelToken
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_propagate_cancellation_to_children_when_parent_cancelled() {
    let parent = CancelToken::new();
    let child = parent.child();
    let grandchild = child.child();

    assert!(!parent.is_cancelled());
    assert!(!child.is_cancelled());
    assert!(!grandchild.is_cancelled());

    parent.cancel();

    assert!(parent.is_cancelled());
    assert!(child.is_cancelled());
    assert!(grandchild.is_cancelled());
}

#[tokio::test]
async fn should_not_propagate_cancellation_to_parent_when_child_cancelled() {
    let parent = CancelToken::new();
    let child = parent.child();

    child.cancel();

    assert!(child.is_cancelled());
    assert!(!parent.is_cancelled());
}

#[test]
fn should_return_aborted_error_when_check_called_after_cancel() {
    let token = CancelToken::new();
    assert!(token.check().is_ok());
    token.cancel();
    let err = token.check().unwrap_err();
    assert!(err.is_abort());
}

#[tokio::test]
async fn should_complete_immediately_when_cancelled_called_after_token_cancelled() {
    let token = CancelToken::new();
    let child = token.child();
    token.cancel();
    // Should return immediately since already cancelled.
    child.cancelled().await;
}

// ---------------------------------------------------------------------------
// StopReason
// ---------------------------------------------------------------------------

#[test]
fn should_return_true_only_for_tool_use_when_is_loop_continuing_called() {
    assert!(StopReason::ToolUse.is_loop_continuing());
    assert!(!StopReason::EndTurn.is_loop_continuing());
    assert!(!StopReason::MaxTokens.is_loop_continuing());
    assert!(!StopReason::Filtered.is_loop_continuing());
    assert!(!StopReason::Paused.is_loop_continuing());
    assert!(!StopReason::Unknown.is_loop_continuing());
}

// ---------------------------------------------------------------------------
// EngineError / ErrorCode
// ---------------------------------------------------------------------------

#[test]
fn should_map_to_correct_error_code_when_engine_error_converted() {
    use mirri_core::{EngineError, ErrorCode};

    assert_eq!(
        EngineError::MaxStepsExceeded("x".into()).code(),
        ErrorCode::LoopMaxStepsExceeded
    );
    assert_eq!(
        EngineError::ContextOverflow { attempts: 3 }.code(),
        ErrorCode::ContextOverflow
    );
    assert_eq!(
        EngineError::UserCancelled.code(),
        ErrorCode::AbortUserCancelled
    );
    assert_eq!(
        EngineError::Aborted("boom".into()).code(),
        ErrorCode::AbortCancelled
    );
    // Internal errors must map to their own code, never AbortCancelled.
    assert_eq!(
        EngineError::Internal("boom".into()).code(),
        ErrorCode::Internal
    );
}

#[test]
fn should_serialize_error_code_to_stable_wire_string_when_converted() {
    use mirri_core::ErrorCode;

    assert_eq!(ErrorCode::Internal.as_str(), "internal");
    assert_eq!(ErrorCode::ProviderApiError.as_str(), "provider.api_error");
    assert_eq!(ErrorCode::AbortCancelled.as_str(), "abort.cancelled");
    assert_eq!(
        ErrorCode::ToolExecutionFailed.as_str(),
        "tool.execution_failed"
    );
    // Dot-notation for session/agent domain codes.
    assert_eq!(ErrorCode::SessionNotFound.as_str(), "session.not_found");
    assert_eq!(ErrorCode::AgentNotFound.as_str(), "agent.not_found");
    assert_eq!(
        ErrorCode::LoopMaxStepsExceeded.as_str(),
        "loop.max_steps_exceeded"
    );
    assert_eq!(ErrorCode::ContextOverflow.as_str(), "context.overflow");
}

#[test]
fn should_classify_abort_correctly_when_engine_error_checked() {
    use mirri_core::EngineError;

    assert!(EngineError::UserCancelled.is_abort());
    assert!(EngineError::Aborted("x".into()).is_abort());
    assert!(!EngineError::MaxStepsExceeded("x".into()).is_abort());

    assert!(EngineError::UserCancelled.is_user_cancellation());
    assert!(!EngineError::Aborted("x".into()).is_user_cancellation());
}

#[test]
fn should_preserve_source_chain_when_io_error_converted_to_engine_error() {
    use std::error::Error;

    use mirri_core::{EngineError, ErrorCode};

    // Given an io::Error propagated via the #[from] conversion
    let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "no such session file");
    let engine_err = EngineError::from(io_err);

    // Then the original error is retained as the source, and the error code
    // keeps the ToolExecutionFailed mapping of the old manual From impl.
    assert!(matches!(engine_err, EngineError::Io(_)));
    assert_eq!(engine_err.code(), ErrorCode::ToolExecutionFailed);
    let source = engine_err.source().expect("io::Error preserved as source");
    assert!(source.to_string().contains("no such session file"));
}

// ---------------------------------------------------------------------------
// ProjectOptions presets
// ---------------------------------------------------------------------------

#[test]
fn should_only_drop_orphans_when_normal_preset_used() {
    use mirri_core::ProjectOptions;

    let opts = ProjectOptions::normal();
    assert!(opts.drop_orphan_results);
    assert!(!opts.synthesize_missing);
    assert!(!opts.drop_leading_non_user);
    assert!(!opts.merge_consecutive_assistants);
    assert!(!opts.dedupe_duplicate_tool_calls);
}

#[test]
fn should_enable_all_repairs_when_strict_preset_used() {
    use mirri_core::ProjectOptions;

    let opts = ProjectOptions::strict();
    assert!(opts.drop_orphan_results);
    assert!(opts.drop_leading_non_user);
    assert!(opts.merge_consecutive_assistants);
    assert!(opts.dedupe_duplicate_tool_calls);
    assert!(!opts.synthesize_missing);
}

#[test]
fn should_synthesize_missing_when_compaction_preset_used() {
    use mirri_core::ProjectOptions;

    let opts = ProjectOptions::compaction();
    assert!(opts.synthesize_missing);
    assert!(opts.drop_orphan_results);
}
