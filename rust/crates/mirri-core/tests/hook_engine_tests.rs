//! Integration tests for the external command hook engine.
//!
//! These tests exercise the full pipeline: registration, matching, command
//! execution, structured output parsing, parallel trigger, and clear.

use std::collections::HashMap;

use mirri_core::hook_engine::{
    HookBlockSource, HookDef, HookEngine, HookEventType, HookOutcome, matches, run_hook,
};

/// The real local process provider — these tests exercise the hook engine
/// against actual shell commands (the fake-provider coverage lives in
/// `hook_process_provider_tests.rs`).
fn local_process() -> mirri_os::LocalProcess {
    mirri_os::LocalProcess::new()
}

// ---------------------------------------------------------------------------
// Registration & listing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_return_empty_vec_when_engine_is_empty() {
    let engine = HookEngine::new();
    let hooks = engine.list();
    assert!(hooks.is_empty(), "a fresh engine should have no hooks");
}

#[tokio::test]
async fn should_return_all_hooks_when_hooks_are_registered() {
    let engine = HookEngine::new();
    engine.register(HookDef::new(HookEventType::PreToolUse, "echo a"));
    engine.register(HookDef::new(HookEventType::PostToolUse, "echo b"));
    engine.register(HookDef::new(HookEventType::SessionStart, "echo c"));

    let hooks = engine.list();
    assert_eq!(hooks.len(), 3, "all three hooks should be listed");
}

#[tokio::test]
async fn should_return_only_matching_hooks_when_listed_by_event() {
    let engine = HookEngine::new();
    engine.register(HookDef::new(HookEventType::PreToolUse, "echo a"));
    engine.register(HookDef::new(HookEventType::PostToolUse, "echo b"));
    engine.register(HookDef::new(HookEventType::PreToolUse, "echo c"));

    let pre_hooks = engine.list_by_event(HookEventType::PreToolUse);
    assert_eq!(
        pre_hooks.len(),
        2,
        "two hooks should match PreToolUse event"
    );

    let post_hooks = engine.list_by_event(HookEventType::PostToolUse);
    assert_eq!(post_hooks.len(), 1, "one hook should match PostToolUse");

    let stop_hooks = engine.list_by_event(HookEventType::Stop);
    assert!(stop_hooks.is_empty(), "no hooks for Stop event");
}

// ---------------------------------------------------------------------------
// Matcher
// ---------------------------------------------------------------------------

#[test]
fn should_match_any_input_when_pattern_is_none() {
    let input = serde_json::json!({"tool_name": "Read"});
    assert!(matches(&None, &input, &HookEventType::PreToolUse));
}

#[test]
fn should_match_any_input_when_pattern_is_empty() {
    let input = serde_json::json!({"tool_name": "Read"});
    assert!(matches(
        &Some(String::new()),
        &input,
        &HookEventType::PreToolUse
    ));
}

#[test]
fn should_match_when_regex_pattern_matches_tool_name() {
    let input = serde_json::json!({"tool_name": "Read"});
    assert!(matches(
        &Some("^Re.*".to_string()),
        &input,
        &HookEventType::PreToolUse
    ));
}

#[test]
fn should_not_match_when_regex_pattern_does_not_match_tool_name() {
    let input = serde_json::json!({"tool_name": "Write"});
    assert!(!matches(
        &Some("^Read$".to_string()),
        &input,
        &HookEventType::PreToolUse
    ));
}

#[test]
fn should_not_match_when_regex_pattern_is_invalid() {
    let input = serde_json::json!({"tool_name": "Read"});
    assert!(!matches(
        &Some("[invalid".to_string()),
        &input,
        &HookEventType::PreToolUse
    ));
}

#[test]
fn should_match_against_json_string_when_event_is_non_tool_event() {
    let input = serde_json::json!({"session_id": "abc-123"});
    assert!(matches(
        &Some("abc-123".to_string()),
        &input,
        &HookEventType::SessionStart
    ));
}

// ---------------------------------------------------------------------------
// run_hook — basic command execution
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_pass_with_stdout_when_echo_command_runs() {
    let hook = HookDef::new(HookEventType::PreToolUse, "echo hello-world");
    let input = serde_json::json!({});
    let outcome = run_hook(&local_process(), &hook, &input, &HashMap::new()).await;

    match outcome {
        HookOutcome::Pass(result) => {
            assert!(
                result.stdout.contains("hello-world"),
                "stdout should contain echo output"
            );
            assert_eq!(result.exit_code, Some(0));
        }
        other => panic!("expected Pass, got {other:?}"),
    }
}

#[tokio::test]
async fn should_pass_when_command_exits_zero() {
    let hook = HookDef::new(HookEventType::PreToolUse, "exit 0");
    let input = serde_json::json!({});
    let outcome = run_hook(&local_process(), &hook, &input, &HashMap::new()).await;

    assert!(
        matches!(outcome, HookOutcome::Pass(_)),
        "exit 0 should Pass"
    );
}

#[tokio::test]
async fn should_block_with_exit_code_2_when_command_exits_two() {
    let hook = HookDef::new(HookEventType::PreToolUse, "exit 2");
    let input = serde_json::json!({});
    let outcome = run_hook(&local_process(), &hook, &input, &HashMap::new()).await;

    match outcome {
        HookOutcome::Block {
            source,
            reason,
            message,
            ..
        } => {
            assert!(
                matches!(source, HookBlockSource::ExitCode2),
                "exit 2 → ExitCode2 source"
            );
            // A silent exit 2 carries no stderr, so both the reason and the
            // display message are empty (the TS resultFromExitCode shape);
            // the default "Blocked by <event> hook" text surfaces in the
            // render layer, not here.
            assert!(reason.is_empty(), "silent exit 2 reason should be empty");
            assert_eq!(message.as_deref(), Some(""));
        }
        other => panic!("expected Block, got {other:?}"),
    }
}

#[tokio::test]
async fn should_pass_not_block_when_command_exits_one() {
    let hook = HookDef::new(HookEventType::PreToolUse, "exit 1");
    let input = serde_json::json!({});
    let outcome = run_hook(&local_process(), &hook, &input, &HashMap::new()).await;

    assert!(
        matches!(outcome, HookOutcome::Pass(_)),
        "exit 1 (non-zero, non-2) should Pass, not Block"
    );
}

// ---------------------------------------------------------------------------
// run_hook — timeout handling
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_error_when_command_exceeds_timeout() {
    // `sleep 5` with a 200ms timeout should time out.
    let mut hook = HookDef::new(HookEventType::PreToolUse, "sleep 5");
    hook.timeout_ms = 200;

    let input = serde_json::json!({});
    let outcome = run_hook(&local_process(), &hook, &input, &HashMap::new()).await;

    match outcome {
        HookOutcome::Error(msg) => {
            assert!(
                msg.contains("timed out"),
                "error message should mention timeout: {msg}"
            );
        }
        other => panic!("expected Error (timeout), got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// run_hook — structured output parsing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_block_structured_when_hook_outputs_deny_decision() {
    let command = r#"echo '{"hookSpecificOutput":{"permissionDecision":"deny"}}'"#;
    let hook = HookDef::new(HookEventType::PermissionRequest, command);
    let input = serde_json::json!({"tool_name": "Write"});

    let outcome = run_hook(&local_process(), &hook, &input, &HashMap::new()).await;

    match outcome {
        HookOutcome::Block { source, .. } => {
            assert!(
                matches!(source, HookBlockSource::StructuredOutput),
                "permissionDecision=deny → StructuredOutput source"
            );
        }
        other => panic!("expected Block (structured), got {other:?}"),
    }
}

#[tokio::test]
async fn should_pass_when_hook_outputs_additional_context() {
    let command = r#"echo '{"additionalContext":"extra-info"}'"#;
    let hook = HookDef::new(HookEventType::PreToolUse, command);
    let input = serde_json::json!({});

    let outcome = run_hook(&local_process(), &hook, &input, &HashMap::new()).await;

    match outcome {
        HookOutcome::Pass(result) => {
            assert!(
                result.stdout.contains("additionalContext"),
                "stdout should contain the structured JSON"
            );
        }
        other => panic!("expected Pass, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// run_hook — environment variables
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_make_env_available_to_command_when_extra_env_provided() {
    // Print the env var so we can verify it was passed through.
    let hook = HookDef::new(HookEventType::PreToolUse, "echo $MY_HOOK_VAR");
    let input = serde_json::json!({});
    let mut extra_env = HashMap::new();
    extra_env.insert("MY_HOOK_VAR".to_string(), "hook-value-42".to_string());

    let outcome = run_hook(&local_process(), &hook, &input, &extra_env).await;

    match outcome {
        HookOutcome::Pass(result) => {
            assert!(
                result.stdout.contains("hook-value-42"),
                "stdout should contain the env var value: {}",
                result.stdout
            );
        }
        other => panic!("expected Pass, got {other:?}"),
    }
}

#[tokio::test]
async fn should_make_hook_env_available_when_hook_defines_env() {
    let mut hook = HookDef::new(HookEventType::PreToolUse, "echo $HOOK_DEFINED_VAR");
    hook.env = Some({
        let mut m = HashMap::new();
        m.insert("HOOK_DEFINED_VAR".to_string(), "from-hook-def".to_string());
        m
    });

    let input = serde_json::json!({});
    let outcome = run_hook(&local_process(), &hook, &input, &HashMap::new()).await;

    match outcome {
        HookOutcome::Pass(result) => {
            assert!(
                result.stdout.contains("from-hook-def"),
                "stdout should contain the hook-defined env var"
            );
        }
        other => panic!("expected Pass, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// run_hook — stdin piping
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_receive_json_on_stdin_when_json_input_provided() {
    // `cat` echoes stdin back to stdout.
    let hook = HookDef::new(HookEventType::PreToolUse, "cat");
    let input = serde_json::json!({"tool_name": "Read", "step": 42});

    let outcome = run_hook(&local_process(), &hook, &input, &HashMap::new()).await;

    match outcome {
        HookOutcome::Pass(result) => {
            assert!(
                result.stdout.contains("tool_name"),
                "stdout should contain the piped JSON: {}",
                result.stdout
            );
            assert!(result.stdout.contains("Read"));
        }
        other => panic!("expected Pass, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// trigger — parallel execution & merging
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_execute_all_hooks_when_multiple_hooks_match() {
    let engine = HookEngine::new();
    engine.register(HookDef::new(HookEventType::PreToolUse, "echo hook-a"));
    engine.register(HookDef::new(HookEventType::PreToolUse, "echo hook-b"));

    let input = serde_json::json!({"tool_name": "Read"});
    let result = engine
        .trigger(HookEventType::PreToolUse, input, &HashMap::new())
        .await;

    assert!(!result.blocked, "neither hook should block");
    assert_eq!(
        result.results.len(),
        2,
        "both matching hooks should have results"
    );
}

#[tokio::test]
async fn should_skip_hook_when_pattern_does_not_match() {
    let engine = HookEngine::new();
    let mut hook = HookDef::new(HookEventType::PreToolUse, "echo should-not-run");
    hook.pattern = Some("^Write$".to_string());
    engine.register(hook);

    let input = serde_json::json!({"tool_name": "Read"});
    let result = engine
        .trigger(HookEventType::PreToolUse, input, &HashMap::new())
        .await;

    assert!(
        result.results.is_empty(),
        "non-matching pattern should not execute the hook"
    );
}

#[tokio::test]
async fn should_execute_hook_when_pattern_matches() {
    let engine = HookEngine::new();
    let mut hook = HookDef::new(HookEventType::PreToolUse, "echo matching");
    hook.pattern = Some("^Read$".to_string());
    engine.register(hook);

    let input = serde_json::json!({"tool_name": "Read"});
    let result = engine
        .trigger(HookEventType::PreToolUse, input, &HashMap::new())
        .await;

    assert_eq!(result.results.len(), 1, "matching hook should execute");
}

#[tokio::test]
async fn should_block_entire_result_when_one_hook_blocks() {
    let engine = HookEngine::new();
    engine.register(HookDef::new(HookEventType::PreToolUse, "echo pass-me"));
    engine.register(HookDef::new(HookEventType::PreToolUse, "exit 2"));

    let input = serde_json::json!({"tool_name": "Read"});
    let result = engine
        .trigger(HookEventType::PreToolUse, input, &HashMap::new())
        .await;

    assert!(result.blocked, "one blocking hook should block the trigger");
    // The silent exit-2 hook carries no stderr, so the reason normalises to
    // None (consumer fallbacks supply the user-facing text downstream).
    assert!(
        result.block_reason.is_none(),
        "silent exit 2 normalises to an absent reason"
    );
}

#[tokio::test]
async fn should_not_block_when_all_hooks_pass() {
    let engine = HookEngine::new();
    engine.register(HookDef::new(HookEventType::PreToolUse, "echo a"));
    engine.register(HookDef::new(HookEventType::PreToolUse, "echo b"));

    let input = serde_json::json!({"tool_name": "Read"});
    let result = engine
        .trigger(HookEventType::PreToolUse, input, &HashMap::new())
        .await;

    assert!(!result.blocked, "all-passing hooks should not block");
}

#[tokio::test]
async fn should_merge_contexts_when_hooks_provide_additional_context() {
    let engine = HookEngine::new();
    engine.register(HookDef::new(
        HookEventType::PreToolUse,
        r#"echo '{"additionalContext":"context-alpha"}'"#,
    ));
    engine.register(HookDef::new(
        HookEventType::PreToolUse,
        r#"echo '{"additionalContext":"context-beta"}'"#,
    ));

    let input = serde_json::json!({"tool_name": "Read"});
    let result = engine
        .trigger(HookEventType::PreToolUse, input, &HashMap::new())
        .await;

    assert!(
        result.additional_context.contains("context-alpha"),
        "merged context should contain first hook's context"
    );
    assert!(
        result.additional_context.contains("context-beta"),
        "merged context should contain second hook's context"
    );
}

#[tokio::test]
async fn should_propagate_input_when_hook_provides_updated_input() {
    let engine = HookEngine::new();
    engine.register(HookDef::new(
        HookEventType::PreToolUse,
        r#"echo '{"updatedInput":{"tool_name":"Read","modified":true}}'"#,
    ));

    let input = serde_json::json!({"tool_name": "Read"});
    let result = engine
        .trigger(HookEventType::PreToolUse, input, &HashMap::new())
        .await;

    let updated = result
        .updated_input
        .as_ref()
        .expect("updated_input should be set");
    assert_eq!(
        updated.get("modified").and_then(|v| v.as_bool()),
        Some(true),
        "updated_input should contain the modified field"
    );
}

#[tokio::test]
async fn should_return_empty_result_when_no_hooks_match() {
    let engine = HookEngine::new();
    engine.register(HookDef::new(HookEventType::PostToolUse, "echo nope"));

    let input = serde_json::json!({"tool_name": "Read"});
    let result = engine
        .trigger(HookEventType::PreToolUse, input, &HashMap::new())
        .await;

    assert!(!result.blocked);
    assert!(result.results.is_empty());
    assert!(result.additional_context.is_empty());
    assert!(result.updated_input.is_none());
}

// ---------------------------------------------------------------------------
// clear
// ---------------------------------------------------------------------------

/// Given a hook with a `cwd`, When the hook runs, Then the command executes
/// in that working directory (TS parity: the `cwd` field on `HookDef`).
#[tokio::test]
async fn should_execute_hook_in_its_working_directory_when_cwd_is_set() {
    let dir = tempfile::tempdir().expect("tempdir must be creatable");
    let mut hook = HookDef::new(
        HookEventType::PreToolUse,
        format!("pwd > {:?}", dir.path().join("pwd.txt")),
    );
    hook.cwd = Some(dir.path().to_string_lossy().to_string());

    let outcome = run_hook(
        &local_process(),
        &hook,
        &serde_json::json!({}),
        &HashMap::new(),
    )
    .await;
    assert!(
        matches!(outcome, HookOutcome::Pass(_)),
        "the hook should pass, got {outcome:?}"
    );

    let content = std::fs::read_to_string(dir.path().join("pwd.txt"))
        .expect("the marker should be written inside the hook's cwd");
    let canonical = |p: &str| std::path::Path::new(p).canonicalize().ok();
    assert_eq!(
        canonical(content.trim()),
        canonical(&dir.path().to_string_lossy()),
        "pwd must resolve to the hook's working directory, got {content}"
    );
}

#[tokio::test]
async fn should_remove_all_hooks_when_cleared() {
    let engine = HookEngine::new();
    engine.register(HookDef::new(HookEventType::PreToolUse, "echo a"));
    engine.register(HookDef::new(HookEventType::PostToolUse, "echo b"));
    assert_eq!(engine.list().len(), 2);

    engine.clear();
    assert!(engine.list().is_empty(), "clear should remove all hooks");
    assert!(
        engine.list_by_event(HookEventType::PreToolUse).is_empty(),
        "clear should remove event-indexed hooks too"
    );

    // After clear, trigger should produce an empty result.
    let input = serde_json::json!({"tool_name": "Read"});
    let result = engine
        .trigger(HookEventType::PreToolUse, input, &HashMap::new())
        .await;
    assert!(
        result.results.is_empty(),
        "trigger after clear should run nothing"
    );
}

// ---------------------------------------------------------------------------
// Structured deny via trigger
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_block_structured_when_trigger_runs_deny_hook() {
    let engine = HookEngine::new();
    let command = r#"echo '{"hookSpecificOutput":{"permissionDecision":"deny"}}'"#;
    engine.register(HookDef::new(HookEventType::PermissionRequest, command));

    let input = serde_json::json!({"tool_name": "Write"});
    let result = engine
        .trigger(HookEventType::PermissionRequest, input, &HashMap::new())
        .await;

    assert!(result.blocked, "structured deny should block");

    // Verify the block source is StructuredOutput (not ExitCode2).
    let block_outcome = result
        .results
        .iter()
        .find(|(_, o)| matches!(o, HookOutcome::Block { .. }))
        .map(|(_, o)| o)
        .expect("should have a blocking outcome");

    match block_outcome {
        HookOutcome::Block { source, .. } => {
            assert!(
                matches!(source, HookBlockSource::StructuredOutput),
                "block should come from structured output"
            );
        }
        _ => panic!("expected Block outcome"),
    }
}

// ---------------------------------------------------------------------------
// bridge — PermissionResult
// ---------------------------------------------------------------------------
