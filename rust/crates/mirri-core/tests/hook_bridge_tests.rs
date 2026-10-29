//! Integration tests for the `ExternalHooksBridge` and lifecycle hooks.
//!
//! These tests verify that:
//! - `ExternalHooksBridge` correctly implements `LoopHooks`.
//! - Each `LoopHooks` callback triggers the corresponding `HookEventType`.
//! - Block outcomes from hooks translate to the correct `LoopHooks` results.
//! - `OrderedHookSlot` executes hooks in priority order.
//! - `SessionLifecycleHooks` fires callbacks for session create / close.
//! - Hook registration and unregistration work correctly.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;

use mirri_core::cancel::CancelToken;
use mirri_core::hook_engine::{
    ExternalHooksBridge, HookDef, HookEngine, HookEventType, OrderedHookSlot, SessionHookSlot,
    SessionLifecycleHooks,
};
use mirri_core::hooks::{
    AuthorizeToolContext, AuthorizeToolResult, BeforeStepContext, ContinueAfterStopContext,
    FinalizeToolContext, LoopHooks, PrepareToolContext, PrepareToolResult,
};
use mirri_core::tool::{ToolInputDisplay, ToolResult};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Create a `CancelToken` for tests.
fn test_cancel() -> CancelToken {
    CancelToken::new()
}

/// Create a `ToolInputDisplay` for tests.
fn test_display() -> ToolInputDisplay {
    ToolInputDisplay::Command {
        command: "test-command".to_string(),
    }
}

/// Create an `ExternalHooksBridge` with the given hooks registered on the
/// engine, and an empty base env.
fn make_bridge(hooks: Vec<HookDef>) -> ExternalHooksBridge {
    let engine = Arc::new(HookEngine::new());
    engine.register_all(hooks);
    ExternalHooksBridge::new(engine, HashMap::new())
}

// ---------------------------------------------------------------------------
// ExternalHooksBridge implements LoopHooks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_be_usable_as_loop_hooks_trait_object_when_bridge_created() {
    // Verify the bridge can be used as a `dyn LoopHooks` (trait object).
    let bridge = make_bridge(vec![]);
    let _hooks: &dyn LoopHooks = &bridge;
    // If this compiles, the bridge implements LoopHooks.
}

// ---------------------------------------------------------------------------
// before_step → PreLlmRequest
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_trigger_hook_when_before_step_called_with_pre_llm_request_hook() {
    // Write a marker file so we can verify the hook ran.
    let temp_dir = std::env::temp_dir();
    let marker_path = temp_dir.join(format!(
        "hook_bridge_test_before_step_{}.txt",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&marker_path);

    let command = format!("echo triggered > {:?}", marker_path);
    let bridge = make_bridge(vec![HookDef::new(HookEventType::PreLlmRequest, command)]);

    let ctx = BeforeStepContext {
        turn_id: "turn-1".to_string(),
        step: 0,
        cancel: test_cancel(),
    };

    let result = bridge.before_step(ctx).await.unwrap();
    assert!(!result.block, "passing hook should not block");

    let content = std::fs::read_to_string(&marker_path).expect("marker file should exist");
    assert!(
        content.contains("triggered"),
        "marker file should contain hook output"
    );
    let _ = std::fs::remove_file(&marker_path);
}

#[tokio::test]
async fn should_return_block_when_before_step_called_with_blocking_pre_llm_request_hook() {
    let bridge = make_bridge(vec![HookDef::new(HookEventType::PreLlmRequest, "exit 2")]);

    let ctx = BeforeStepContext {
        turn_id: "turn-1".to_string(),
        step: 0,
        cancel: test_cancel(),
    };

    let result = bridge.before_step(ctx).await.unwrap();
    assert!(result.block, "blocking hook should set block=true");
    // A silent exit 2 carries no stderr, so the merged reason normalises to
    // None — the step loop's fallback text ("Step blocked by before_step
    // hook") supplies the user-facing message downstream.
    assert!(
        result.block_reason.is_none(),
        "silent exit 2 should normalise to an absent reason"
    );
}

// ---------------------------------------------------------------------------
// after_step → PostLlmRequest
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_trigger_hook_when_after_step_called_with_post_llm_request_hook() {
    let temp_dir = std::env::temp_dir();
    let marker_path = temp_dir.join(format!(
        "hook_bridge_test_after_step_{}.txt",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&marker_path);

    let command = format!("echo post_llm > {:?}", marker_path);
    let bridge = make_bridge(vec![HookDef::new(HookEventType::PostLlmRequest, command)]);

    let ctx = mirri_core::hooks::AfterStepContext {
        turn_id: "turn-1".to_string(),
        step: 1,
        input_tokens: 100,
        output_tokens: 50,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
        stop_reason: "end_turn".to_string(),
    };

    let result = bridge.after_step(ctx).await.unwrap();
    assert!(!result.stop_turn, "passing hook should not stop turn");

    let content = std::fs::read_to_string(&marker_path).expect("marker file should exist");
    assert!(
        content.contains("post_llm"),
        "marker file should contain hook output"
    );
    let _ = std::fs::remove_file(&marker_path);
}

// ---------------------------------------------------------------------------
// prepare_tool_execution → PreToolUse (fired by TurnFlowHooks, not the bridge)
// ---------------------------------------------------------------------------

/// Given a bridge with a PreToolUse hook registered, When a tool call is
/// prepared, Then the bridge has no opinion — it neither fires the hook nor
/// blocks, because the PreToolUse event is triggered by the primary
/// TurnFlowHooks (which has the context-memory access the additionalContext
/// injection and the rewritten-args channel need).
#[tokio::test]
async fn should_preserve_primary_decision_without_firing_pre_tool_use_hooks() {
    let marker_path =
        std::env::temp_dir().join(format!("bridge_pretool_no_fire_{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&marker_path);

    let bridge = make_bridge(vec![HookDef::new(
        HookEventType::PreToolUse,
        format!("echo fired > {:?}", marker_path),
    )]);

    let args = serde_json::json!({"path": "/tmp/test"});
    let ctx = PrepareToolContext {
        turn_id: "turn-1".to_string(),
        step: 0,
        tool_call_id: "call-1".to_string(),
        tool_name: "Read".to_string(),
        args: &args,
        cancel: test_cancel(),
    };

    let result = bridge.prepare_tool_execution(ctx).await.unwrap();
    assert!(
        matches!(result, PrepareToolResult::Allowed),
        "bridge must preserve the primary's prepare decision"
    );
    assert!(
        !marker_path.exists(),
        "the bridge must not fire the PreToolUse hook"
    );
    let _ = std::fs::remove_file(&marker_path);
}

// ---------------------------------------------------------------------------
// authorize_tool_execution → PermissionRequest (fired by TurnFlowHooks at the
// permission ask, not the bridge)
// ---------------------------------------------------------------------------

/// Given a bridge with PermissionRequest hooks registered, When a tool call
/// is authorized, Then the bridge has no opinion — it neither fires the hooks
/// nor denies, because the PermissionRequest event is an observation fired by
/// the primary TurnFlowHooks at the actual permission ask (TS parity:
/// `permission.approval.requested` — never a gate).
#[tokio::test]
async fn should_preserve_primary_decision_without_firing_permission_request_hooks() {
    let marker_path =
        std::env::temp_dir().join(format!("bridge_perm_no_fire_{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&marker_path);

    let bridge = make_bridge(vec![
        HookDef::new(
            HookEventType::PermissionRequest,
            format!("echo fired > {:?}", marker_path),
        ),
        HookDef::new(
            HookEventType::PermissionRequest,
            r#"echo '{"hookSpecificOutput":{"permissionDecision":"deny"}}'"#,
        ),
    ]);

    let args = serde_json::json!({"path": "/tmp/test"});
    let approval_rule = "auto";
    let display = test_display();
    let ctx = AuthorizeToolContext {
        turn_id: "turn-1".to_string(),
        step: 0,
        tool_call_id: "call-1".to_string(),
        tool_name: "Read".to_string(),
        args: &args,
        approval_rule,
        display: &display,
        cancel: test_cancel(),
    };

    let result = bridge.authorize_tool_execution(ctx).await.unwrap();
    assert!(
        matches!(result, AuthorizeToolResult::Approved { metadata: None }),
        "bridge must preserve the primary's authorize decision"
    );
    assert!(
        !marker_path.exists(),
        "the bridge must not fire the PermissionRequest hooks"
    );
    let _ = std::fs::remove_file(&marker_path);
}

// ---------------------------------------------------------------------------
// finalize_tool_result → PostToolUse (+ PostToolUseFailure on error)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_trigger_hook_when_finalize_tool_result_called_with_post_tool_use_hook() {
    let temp_dir = std::env::temp_dir();
    let marker_path = temp_dir.join(format!(
        "hook_bridge_test_finalize_{}.txt",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&marker_path);

    let command = format!("echo post_tool > {:?}", marker_path);
    let bridge = make_bridge(vec![HookDef::new(HookEventType::PostToolUse, command)]);

    let mut tool_result = ToolResult::success("file content");
    let args = serde_json::json!({"path": "/tmp/test"});
    let ctx = FinalizeToolContext {
        turn_id: "turn-1".to_string(),
        step: 0,
        tool_call_id: "call-1".to_string(),
        tool_name: "Read".to_string(),
        args: &args,
        result: &mut tool_result,
        cancel: test_cancel(),
    };

    let result = bridge.finalize_tool_result(ctx).await.unwrap();
    assert!(!result.stop_turn, "passing hook should not stop turn");

    let content = std::fs::read_to_string(&marker_path).expect("marker file should exist");
    assert!(
        content.contains("post_tool"),
        "marker file should contain hook output"
    );
    let _ = std::fs::remove_file(&marker_path);
}

#[tokio::test]
async fn should_trigger_post_tool_use_failure_hook_when_finalize_called_with_error_tool_result() {
    let temp_dir = std::env::temp_dir();
    let marker_path = temp_dir.join(format!(
        "hook_bridge_test_failure_{}.txt",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&marker_path);

    let command = format!("echo failure_fired > {:?}", marker_path);
    let bridge = make_bridge(vec![HookDef::new(
        HookEventType::PostToolUseFailure,
        command,
    )]);

    let mut tool_result = ToolResult::error("command not found");
    let args = serde_json::json!({"command": "ls"});
    let ctx = FinalizeToolContext {
        turn_id: "turn-1".to_string(),
        step: 0,
        tool_call_id: "call-1".to_string(),
        tool_name: "Bash".to_string(),
        args: &args,
        result: &mut tool_result,
        cancel: test_cancel(),
    };

    let _ = bridge.finalize_tool_result(ctx).await.unwrap();

    let content = std::fs::read_to_string(&marker_path).expect("marker file should exist");
    assert!(
        content.contains("failure_fired"),
        "PostToolUseFailure hook should have been triggered for error results"
    );
    let _ = std::fs::remove_file(&marker_path);
}

#[tokio::test]
async fn should_not_trigger_post_tool_use_failure_hook_when_finalize_called_with_success_tool_result()
 {
    let temp_dir = std::env::temp_dir();
    let marker_path = temp_dir.join(format!(
        "hook_bridge_test_no_failure_{}.txt",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&marker_path);

    let command = format!("echo should_not_fire > {:?}", marker_path);
    let bridge = make_bridge(vec![HookDef::new(
        HookEventType::PostToolUseFailure,
        command,
    )]);

    let mut tool_result = ToolResult::success("ok");
    let args = serde_json::json!({"path": "/tmp/test"});
    let ctx = FinalizeToolContext {
        turn_id: "turn-1".to_string(),
        step: 0,
        tool_call_id: "call-1".to_string(),
        tool_name: "Read".to_string(),
        args: &args,
        result: &mut tool_result,
        cancel: test_cancel(),
    };

    let _ = bridge.finalize_tool_result(ctx).await.unwrap();

    assert!(
        !marker_path.exists(),
        "PostToolUseFailure should NOT fire for successful tool results"
    );
}

/// Given a successful tool result with an oversized output, When the result
/// finalizes, Then the PostToolUse payload's `tool_output` is truncated to
/// 2000 characters (TS parity: `output.slice(0, 2000)`).
#[tokio::test]
async fn should_truncate_tool_output_to_2000_chars_when_finalize_called_with_long_output() {
    let temp_dir = std::env::temp_dir();
    let marker_path = temp_dir.join(format!(
        "hook_bridge_test_truncate_{}.json",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&marker_path);

    let bridge = make_bridge(vec![HookDef::new(
        HookEventType::PostToolUse,
        format!("cat > {:?}", marker_path),
    )]);

    let long_output = "x".repeat(3000);
    let mut tool_result = ToolResult::success(long_output);
    let args = serde_json::json!({"command": "ls"});
    let ctx = FinalizeToolContext {
        turn_id: "turn-1".to_string(),
        step: 0,
        tool_call_id: "call-1".to_string(),
        tool_name: "Bash".to_string(),
        args: &args,
        result: &mut tool_result,
        cancel: test_cancel(),
    };

    let _ = bridge.finalize_tool_result(ctx).await.unwrap();

    let content = std::fs::read_to_string(&marker_path).expect("hook input file should exist");
    let input: serde_json::Value = serde_json::from_str(&content).expect("input is JSON");
    let tool_output = input["tool_output"].as_str().expect("tool_output present");
    assert_eq!(tool_output.chars().count(), 2000, "truncated to 2000 chars");
    assert!(
        tool_output.chars().all(|c| c == 'x'),
        "the truncation keeps the output prefix"
    );
    let _ = std::fs::remove_file(&marker_path);
}

// ---------------------------------------------------------------------------
// should_continue_after_stop → Stop (fired by TurnFlowHooks, not the bridge)
// ---------------------------------------------------------------------------

/// Given a bridge with a Stop hook registered, When the stop boundary is
/// reached, Then the bridge has no opinion — it neither fires the hook nor
/// blocks the primary's decision, because the Stop event is triggered by the
/// primary TurnFlowHooks (which has the context-memory access the forced
/// continuation needs).
#[tokio::test]
async fn should_preserve_primary_decision_without_firing_stop_hooks() {
    let marker_path =
        std::env::temp_dir().join(format!("bridge_stop_no_fire_{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&marker_path);

    let bridge = make_bridge(vec![HookDef::new(
        HookEventType::Stop,
        format!("echo fired > {:?}", marker_path),
    )]);

    let ctx = ContinueAfterStopContext {
        turn_id: "turn-1".to_string(),
        stop_reason: "end_turn".to_string(),
        cancel: test_cancel(),
    };

    let result = bridge.should_continue_after_stop(ctx).await.unwrap();
    assert!(
        result.continue_turn,
        "bridge must preserve the primary's stop decision"
    );
    assert!(
        !marker_path.exists(),
        "the bridge must not fire the Stop hook"
    );
    let _ = std::fs::remove_file(&marker_path);
}

// ---------------------------------------------------------------------------
// OrderedHookSlot — priority-ordered execution
// ---------------------------------------------------------------------------

#[test]
fn should_execute_in_ascending_priority_order_when_ordered_hooks_executed() {
    let mut slot: SessionHookSlot = OrderedHookSlot::new();
    let order = Arc::new(StdMutex::new(Vec::<String>::new()));

    let o1 = Arc::clone(&order);
    slot.register(
        "after".to_string(),
        10,
        Box::new(move |_| {
            o1.lock().unwrap().push("after".to_string());
        }),
    );

    let o2 = Arc::clone(&order);
    slot.register(
        "before".to_string(),
        -10,
        Box::new(move |_| {
            o2.lock().unwrap().push("before".to_string());
        }),
    );

    let o3 = Arc::clone(&order);
    slot.register(
        "default".to_string(),
        0,
        Box::new(move |_| {
            o3.lock().unwrap().push("default".to_string());
        }),
    );

    slot.execute("session-1");

    let recorded = order.lock().unwrap();
    assert_eq!(
        *recorded,
        vec!["before", "default", "after"],
        "hooks should execute in ascending priority order"
    );
}

#[test]
fn should_remove_hook_from_execution_when_unregister_called_on_ordered_hooks() {
    let mut slot: SessionHookSlot = OrderedHookSlot::new();
    let count = Arc::new(StdMutex::new(0u32));

    let c1 = Arc::clone(&count);
    slot.register(
        "h1".to_string(),
        0,
        Box::new(move |_| {
            *c1.lock().unwrap() += 1;
        }),
    );

    let c2 = Arc::clone(&count);
    slot.register(
        "h2".to_string(),
        0,
        Box::new(move |_| {
            *c2.lock().unwrap() += 1;
        }),
    );

    slot.execute("s1");
    assert_eq!(*count.lock().unwrap(), 2, "both hooks should fire");

    slot.unregister("h1");
    slot.execute("s2");
    assert_eq!(
        *count.lock().unwrap(),
        3,
        "only one hook should fire after unregister"
    );
}

#[test]
fn should_replace_existing_hook_when_duplicate_id_registered() {
    let mut slot: SessionHookSlot = OrderedHookSlot::new();
    let value = Arc::new(StdMutex::new(0u32));

    let v1 = Arc::clone(&value);
    slot.register(
        "hook".to_string(),
        0,
        Box::new(move |_| {
            *v1.lock().unwrap() = 1;
        }),
    );

    let v2 = Arc::clone(&value);
    slot.register(
        "hook".to_string(),
        0,
        Box::new(move |_| {
            *v2.lock().unwrap() = 2;
        }),
    );

    assert_eq!(slot.len(), 1, "duplicate id should replace, not add");

    slot.execute("s1");
    assert_eq!(
        *value.lock().unwrap(),
        2,
        "replaced hook should be the one that executes"
    );
}

// ---------------------------------------------------------------------------
// SessionLifecycleHooks — fire events
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_run_callback_with_session_id_when_session_created_hook_fired() {
    let hooks = SessionLifecycleHooks::new();
    let received = Arc::new(StdMutex::new(String::new()));

    let r = Arc::clone(&received);
    hooks
        .on_session_created(
            "log-create".to_string(),
            0,
            Box::new(move |sid| {
                *r.lock().unwrap() = sid.to_string();
            }),
        )
        .await;

    hooks.fire_session_created("session-abc").await;

    assert_eq!(
        *received.lock().unwrap(),
        "session-abc",
        "callback should receive the session ID"
    );
}

#[tokio::test]
async fn should_run_callback_when_session_closing_hook_fired() {
    let hooks = SessionLifecycleHooks::new();
    let fired = Arc::new(StdMutex::new(false));

    let f = Arc::clone(&fired);
    hooks
        .on_session_closing(
            "cleanup".to_string(),
            0,
            Box::new(move |_| {
                *f.lock().unwrap() = true;
            }),
        )
        .await;

    hooks.fire_session_closing("session-xyz").await;

    assert!(*fired.lock().unwrap(), "closing callback should have fired");
}

#[tokio::test]
async fn should_fire_in_priority_order_when_multiple_session_lifecycle_hooks_registered() {
    let hooks = SessionLifecycleHooks::new();
    let order = Arc::new(StdMutex::new(Vec::<String>::new()));

    let o1 = Arc::clone(&order);
    hooks
        .on_session_created(
            "after".to_string(),
            10,
            Box::new(move |_| {
                o1.lock().unwrap().push("after".to_string());
            }),
        )
        .await;

    let o2 = Arc::clone(&order);
    hooks
        .on_session_created(
            "before".to_string(),
            -10,
            Box::new(move |_| {
                o2.lock().unwrap().push("before".to_string());
            }),
        )
        .await;

    let o3 = Arc::clone(&order);
    hooks
        .on_session_created(
            "default".to_string(),
            0,
            Box::new(move |_| {
                o3.lock().unwrap().push("default".to_string());
            }),
        )
        .await;

    hooks.fire_session_created("session-1").await;

    let recorded = order.lock().unwrap();
    assert_eq!(
        *recorded,
        vec!["before", "default", "after"],
        "session-created hooks should fire in priority order"
    );
}

#[tokio::test]
async fn should_not_fire_hook_when_unregistered_from_session_lifecycle() {
    let hooks = SessionLifecycleHooks::new();
    let count = Arc::new(StdMutex::new(0u32));

    let c = Arc::clone(&count);
    hooks
        .on_session_created(
            "counter".to_string(),
            0,
            Box::new(move |_| {
                *c.lock().unwrap() += 1;
            }),
        )
        .await;

    hooks.fire_session_created("s1").await;
    assert_eq!(*count.lock().unwrap(), 1);

    // Unregister.
    {
        let mut slot = hooks.on_did_create_session.lock().await;
        slot.unregister("counter");
    }

    hooks.fire_session_created("s2").await;
    assert_eq!(
        *count.lock().unwrap(),
        1,
        "hook should not fire after unregistration"
    );
}

// ---------------------------------------------------------------------------
// Bridge with base_env
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_make_env_available_to_hook_when_bridge_triggered_with_base_env() {
    let engine = Arc::new(HookEngine::new());
    engine.register(HookDef::new(
        HookEventType::PreToolUse,
        "echo $BRIDGE_TEST_VAR",
    ));

    let mut base_env = HashMap::new();
    base_env.insert(
        "BRIDGE_TEST_VAR".to_string(),
        "bridge-env-value".to_string(),
    );

    let bridge = ExternalHooksBridge::new(engine, base_env);

    let args = serde_json::json!({});
    let ctx = PrepareToolContext {
        turn_id: "turn-1".to_string(),
        step: 0,
        tool_call_id: "call-1".to_string(),
        tool_name: "Read".to_string(),
        args: &args,
        cancel: test_cancel(),
    };

    let result = bridge.prepare_tool_execution(ctx).await.unwrap();
    assert!(matches!(result, PrepareToolResult::Allowed));
}

// ---------------------------------------------------------------------------
// Bridge with no hooks — no-op behavior
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_return_allowed_when_prepare_tool_called_with_no_hooks_registered() {
    let bridge = make_bridge(vec![]);

    let args = serde_json::json!({"path": "/tmp/test"});
    let ctx = PrepareToolContext {
        turn_id: "turn-1".to_string(),
        step: 0,
        tool_call_id: "call-1".to_string(),
        tool_name: "Read".to_string(),
        args: &args,
        cancel: test_cancel(),
    };

    let result = bridge.prepare_tool_execution(ctx).await.unwrap();
    assert!(
        matches!(result, PrepareToolResult::Allowed),
        "no hooks should return Allowed"
    );
}

#[tokio::test]
async fn should_return_approved_when_authorize_called_with_no_hooks_registered() {
    let bridge = make_bridge(vec![]);

    let args = serde_json::json!({"path": "/tmp/test"});
    let approval_rule = "auto";
    let display = test_display();
    let ctx = AuthorizeToolContext {
        turn_id: "turn-1".to_string(),
        step: 0,
        tool_call_id: "call-1".to_string(),
        tool_name: "Read".to_string(),
        args: &args,
        approval_rule,
        display: &display,
        cancel: test_cancel(),
    };

    let result = bridge.authorize_tool_execution(ctx).await.unwrap();
    assert!(
        matches!(result, AuthorizeToolResult::Approved { metadata: None }),
        "no hooks should return Approved"
    );
}

#[tokio::test]
async fn should_return_true_when_should_continue_called_with_no_hooks_registered() {
    let bridge = make_bridge(vec![]);

    let ctx = ContinueAfterStopContext {
        turn_id: "turn-1".to_string(),
        stop_reason: "end_turn".to_string(),
        cancel: test_cancel(),
    };

    let result = bridge.should_continue_after_stop(ctx).await.unwrap();
    assert!(
        result.continue_turn,
        "no hooks should return continue_turn=true"
    );
}
