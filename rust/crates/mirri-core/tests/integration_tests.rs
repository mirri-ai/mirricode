//! Cross-module integration tests (Task 23).
//!
//! These tests verify that all Phase 2 engine modules work together correctly.
//! They exercise the full pipeline: scope hierarchy → TurnFlow → loop engine →
//! tool registry → permission gate → context memory → compaction.
//!
//! Test naming follows the Given-When-Then convention.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use futures::stream;
use serde_json::Value;

use mirri_core::cancel::CancelToken;
use mirri_core::compaction::{
    CompactionRoundInput, CompactionStrategy, FullCompaction, compaction_round,
};
use mirri_core::context_memory::{ContextMemory, LoopEvent};
use mirri_core::error::{EngineError, EngineResult};
use mirri_core::hook_engine::{HookDef, HookEngine, HookEventType};
use mirri_core::hooks::*;
use mirri_core::llm::*;
use mirri_core::loop_engine::*;
use mirri_core::message::{ContentPart, Message, Role};
use mirri_core::origin::PromptOrigin;
use mirri_core::permission::{
    PermissionManager, PermissionMode, PermissionPolicyContext, PermissionPolicyResult,
    PermissionRule, PermissionServices, RuleDecision, RuleScope,
};
use mirri_core::tool::*;
use mirri_core::tool_registry::{ToolManager, ValidatorCache, run_tool_batch};
use mirri_core::turn_flow::*;

// ===========================================================================
// Mock LLM (copied from loop_engine_tests pattern)
// ===========================================================================

#[derive(Clone)]
struct MockResponse {
    text: String,
    think_text: String,
    tool_calls: Vec<LlmToolCall>,
    stop_reason: String,
    usage: LlmUsage,
    error: Option<String>,
}

impl MockResponse {
    fn end_turn(text: &str) -> Self {
        Self {
            text: text.to_string(),
            think_text: String::new(),
            tool_calls: Vec::new(),
            stop_reason: "completed".to_string(),
            usage: LlmUsage {
                input_tokens: 100,
                output_tokens: 50,
                ..Default::default()
            },
            error: None,
        }
    }

    fn tool_use(calls: Vec<LlmToolCall>) -> Self {
        Self {
            text: String::new(),
            think_text: String::new(),
            tool_calls: calls,
            stop_reason: "tool_calls".to_string(),
            usage: LlmUsage {
                input_tokens: 100,
                output_tokens: 50,
                ..Default::default()
            },
            error: None,
        }
    }

    fn with_usage(mut self, input: u64, output: u64) -> Self {
        self.usage = LlmUsage {
            input_tokens: input,
            output_tokens: output,
            ..Default::default()
        };
        self
    }

    #[allow(dead_code)]
    fn error(message: &str) -> Self {
        Self {
            text: String::new(),
            think_text: String::new(),
            tool_calls: Vec::new(),
            stop_reason: String::new(),
            usage: LlmUsage::default(),
            error: Some(message.to_string()),
        }
    }
}

struct MockLlm {
    system_prompt: String,
    model_name: String,
    responses: Arc<Mutex<Vec<MockResponse>>>,
    retryable: bool,
}

impl MockLlm {
    fn new(responses: Vec<MockResponse>) -> Self {
        Self {
            system_prompt: "You are a test assistant.".to_string(),
            model_name: "test-model".to_string(),
            responses: Arc::new(Mutex::new(responses)),
            retryable: false,
        }
    }

    #[allow(dead_code)]
    fn with_retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }
}

impl Llm for MockLlm {
    fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    fn model_name(&self) -> &str {
        &self.model_name
    }

    fn capability(&self) -> &ModelCapability {
        static CAP: ModelCapability = ModelCapability {
            image_in: false,
            video_in: false,
            audio_in: false,
            dynamically_loaded_tools: false,
        };
        &CAP
    }

    fn is_retryable_error(&self, _error: &ProviderErrorInfo) -> bool {
        self.retryable
    }

    fn chat(&self, _params: LlmChatParams) -> EngineResult<LlmEventStream> {
        let mut queue = self.responses.lock().unwrap();
        if queue.is_empty() {
            return Err(EngineError::ProviderError(
                "No more mock responses".to_string(),
            ));
        }
        let resp = queue.remove(0);
        drop(queue);

        let events = build_events(&resp);
        let stream = stream::iter(events);
        Ok(Box::pin(stream))
    }
}

fn build_events(resp: &MockResponse) -> Vec<LlmEvent> {
    let mut events = Vec::new();

    if let Some(ref err) = resp.error {
        events.push(LlmEvent::Error {
            message: err.clone(),
        });
        return events;
    }

    if !resp.text.is_empty() {
        events.push(LlmEvent::TextDelta {
            text: resp.text.clone(),
        });
        events.push(LlmEvent::TextPart {
            text: resp.text.clone(),
        });
    }

    if !resp.think_text.is_empty() {
        events.push(LlmEvent::ThinkPart {
            text: resp.think_text.clone(),
        });
    }

    for tc in &resp.tool_calls {
        events.push(LlmEvent::ToolCall {
            tool: tc.name.clone(),
            args: tc.args.clone(),
            call_id: tc.id.clone(),
        });
    }

    events.push(LlmEvent::Usage {
        input_tokens: resp.usage.input_tokens,
        output_tokens: resp.usage.output_tokens,
        cache_read_tokens: resp.usage.cache_read_tokens,
        cache_creation_tokens: resp.usage.cache_creation_tokens,
    });

    events.push(LlmEvent::Done {
        stop_reason: resp.stop_reason.clone(),
    });

    events
}

// ===========================================================================
// Mock Tools
// ===========================================================================

/// A simple echo tool for testing.
struct EchoTool;

impl Tool for EchoTool {
    fn name(&self) -> &str {
        "Echo"
    }

    fn description(&self) -> &str {
        "Echoes back the input"
    }

    fn parameters(&self) -> &Value {
        static SCHEMA: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "message": { "type": "string" }
                },
                "required": ["message"]
            })
        })
    }

    fn resolve_execution(&self, input: Value) -> EngineResult<ToolExecution> {
        let msg = input
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("no message")
            .to_string();
        Ok(ToolExecution::Runnable {
            plan: ExecutionPlan {
                accesses: Vec::new(),
                display: ToolInputDisplay::Generic {
                    summary: format!("Echo: {}", msg),
                },
                approval_rule: "Echo".to_string(),
                stop_batch_after_this: false,
            },
            execute: Box::new(move |_ctx| Box::pin(async move { ToolResult::success(msg) })),
        })
    }
}

/// A tool that always returns an error.
struct ErrorTool;

impl Tool for ErrorTool {
    fn name(&self) -> &str {
        "ErrorTool"
    }

    fn description(&self) -> &str {
        "Always returns an error"
    }

    fn parameters(&self) -> &Value {
        static SCHEMA: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| serde_json::json!({"type": "object", "properties": {}}))
    }

    fn resolve_execution(&self, _input: Value) -> EngineResult<ToolExecution> {
        Ok(ToolExecution::Runnable {
            plan: ExecutionPlan {
                accesses: Vec::new(),
                display: ToolInputDisplay::Generic {
                    summary: "ErrorTool".to_string(),
                },
                approval_rule: "ErrorTool".to_string(),
                stop_batch_after_this: false,
            },
            execute: Box::new(|_ctx| Box::pin(async { ToolResult::error("Something went wrong") })),
        })
    }
}

// ===========================================================================
// Mock Hooks
// ===========================================================================

#[derive(Clone, Default)]
struct TestHooks {
    before_step_block: bool,
    after_step_stop_turn: bool,
    should_continue: bool,
}

impl LoopHooks for TestHooks {
    fn before_step(&self, _ctx: BeforeStepContext) -> HookFuture<EngineResult<BeforeStepResult>> {
        let block = self.before_step_block;
        Box::pin(async move {
            Ok(BeforeStepResult {
                block,
                block_reason: None,
            })
        })
    }

    fn after_step(&self, _ctx: AfterStepContext) -> HookFuture<EngineResult<AfterStepResult>> {
        let stop = self.after_step_stop_turn;
        Box::pin(async move { Ok(AfterStepResult { stop_turn: stop }) })
    }

    fn should_continue_after_stop(
        &self,
        _ctx: ContinueAfterStopContext,
    ) -> HookFuture<EngineResult<ContinueAfterStopResult>> {
        let cont = self.should_continue;
        Box::pin(async move {
            Ok(ContinueAfterStopResult {
                continue_turn: cont,
            })
        })
    }
}

// ===========================================================================
// Shared helpers
// ===========================================================================

fn make_message_builders() -> MessageBuilders {
    MessageBuilders {
        normal: Box::new(|| Ok(vec![Message::user("Hello")])),
        media_degraded: None,
        media_stripped: None,
        strict: None,
        format_rejected_flag: Arc::new(AtomicBool::new(false)),
    }
}

fn make_tool_manager_with_echo() -> ToolManager {
    let mut tm = ToolManager::new();
    tm.set_active_tools(vec!["Echo".to_string()]);
    tm.register_builtin(Arc::new(EchoTool));
    tm
}

fn make_tool_call(id: &str, name: &str, args: Value) -> LlmToolCall {
    LlmToolCall {
        id: id.to_string(),
        name: name.to_string(),
        args,
    }
}

fn make_tool_manager_with_echo_and_error() -> ToolManager {
    let mut tm = ToolManager::new();
    tm.set_active_tools(vec!["Echo".to_string(), "ErrorTool".to_string()]);
    tm.register_builtin(Arc::new(EchoTool));
    tm.register_builtin(Arc::new(ErrorTool));
    tm
}

// ===========================================================================
// 1. Context Memory + Tool Registry Integration
// ===========================================================================

#[tokio::test]
async fn should_record_loop_events_when_appending_tool_results() {
    // Given: a ToolManager with an Echo tool registered and a ContextMemory.
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let cancel = CancelToken::new();
    let mut memory = ContextMemory::new();

    // Open an assistant step and record a tool call.
    memory.append_loop_event(LoopEvent::StepBegin {
        step_id: "step-1".to_string(),
        entry_seq: None,
    });
    memory.append_loop_event(LoopEvent::ToolCall {
        step_id: "step-1".to_string(),
        tool_call_id: "call-1".to_string(),
        tool_name: "Echo".to_string(),
        args: serde_json::json!({"message": "hello"}),
    });

    // When: execute the tool call and record the result into memory.
    let results = run_tool_batch(
        &[(
            "Echo".to_string(),
            r#"{"message":"hello"}"#.to_string(),
            "call-1".to_string(),
        )],
        &tools,
        &mut validators,
        "turn-1",
        1,
        &cancel,
        None,
        &NoopHooks,
        None,
        None,
    )
    .await;

    assert_eq!(results.len(), 1);
    let (call_id, result) = &results[0];
    assert_eq!(call_id, "call-1");
    assert!(!result.is_error);

    // Record the tool result into ContextMemory.
    memory.append_loop_event(LoopEvent::ToolResult {
        tool_call_id: "call-1".to_string(),
        output: result.output.clone(),
        is_error: result.is_error,
        note: result.note.clone(),
        entry_seq: None,
    });

    // Close the step.
    memory.append_loop_event(LoopEvent::StepEnd {
        step_id: "step-1".to_string(),
        input_tokens: 100,
        output_tokens: 50,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
    });

    // Then: ContextMemory history has the assistant tool call and the tool result.
    let history = memory.history().to_vec();
    assert_eq!(history.len(), 2, "expected assistant + tool message");

    // First message: assistant with tool_calls.
    assert_eq!(history[0].message.role, Role::Assistant);
    assert_eq!(history[0].message.tool_calls.len(), 1);
    assert_eq!(history[0].message.tool_calls[0].function.name, "Echo");

    // Second message: tool result for call-1.
    assert_eq!(history[1].message.role, Role::Tool);
    assert_eq!(history[1].message.tool_call_id.as_deref(), Some("call-1"));
    assert!(history[1].message.text_content().contains("hello"));
}

// ===========================================================================
// 2. Context Memory + Permission Gate Integration
// ===========================================================================

#[test]
fn should_approve_tool_when_permission_manager_in_yolo_mode() {
    // Given: a PermissionManager in Yolo mode with no rules.
    let manager = PermissionManager::new(PermissionMode::Yolo, Vec::new());
    let cancel = CancelToken::new();
    let args = serde_json::Value::Null;
    let display = ToolInputDisplay::Generic {
        summary: "test".to_string(),
    };
    let effective_rules = manager.effective_rules();
    let session_approvals = std::collections::HashSet::new();

    let ctx = PermissionPolicyContext {
        tool_name: "Bash",
        tool_call_id: "call-1",
        args: &args,
        approval_rule: "",
        display: &display,
        accesses: &[],
        mode: PermissionMode::Yolo,
        rules: &effective_rules,
        session_approvals: &session_approvals,
        cancel: &cancel,
    };

    // When: evaluate the policy chain.
    let result = manager.evaluate(&ctx);

    // Then: Yolo mode approves everything (no deny rule).
    assert!(
        matches!(result, PermissionPolicyResult::Approve { .. }),
        "Yolo mode should approve, got {:?}",
        result
    );
}

#[test]
fn should_deny_tool_when_deny_rule_matches() {
    // Given: a PermissionManager in Auto mode with a deny rule for "Bash".
    let rules = vec![PermissionRule {
        decision: RuleDecision::Deny,
        scope: RuleScope::User,
        pattern: "Bash".to_string(),
        reason: None,
    }];
    let manager = PermissionManager::new(PermissionMode::Auto, rules);
    let cancel = CancelToken::new();
    let args = serde_json::Value::Null;
    let display = ToolInputDisplay::Generic {
        summary: "test".to_string(),
    };
    let effective_rules = manager.effective_rules();
    let session_approvals = std::collections::HashSet::new();

    let ctx = PermissionPolicyContext {
        tool_name: "Bash",
        tool_call_id: "call-1",
        args: &args,
        approval_rule: "",
        display: &display,
        accesses: &[],
        mode: PermissionMode::Auto,
        rules: &effective_rules,
        session_approvals: &session_approvals,
        cancel: &cancel,
    };

    // When: evaluate the policy chain.
    let result = manager.evaluate(&ctx);

    // Then: deny rule beats auto-approve.
    assert!(
        matches!(result, PermissionPolicyResult::Deny { .. }),
        "deny rule should block, got {:?}",
        result
    );
}

#[test]
fn should_approve_when_before_tool_call_in_headless_mode() {
    // Given: a PermissionManager in Manual mode (default), no rules, headless.
    let mut manager = PermissionManager::new(PermissionMode::Manual, Vec::new());
    manager.set_headless(true);
    let cancel = CancelToken::new();
    let args = serde_json::Value::Null;
    let display = ToolInputDisplay::Generic {
        summary: "test".to_string(),
    };
    let effective_rules = manager.effective_rules();
    let session_approvals = std::collections::HashSet::new();

    let ctx = PermissionPolicyContext {
        tool_name: "Read",
        tool_call_id: "call-1",
        args: &args,
        approval_rule: "",
        display: &display,
        accesses: &[],
        mode: PermissionMode::Manual,
        rules: &effective_rules,
        session_approvals: &session_approvals,
        cancel: &cancel,
    };

    // When: call before_tool_call (headless mode — Ask defaults to Approved).
    let result = manager.before_tool_call(&ctx);

    // Then: in headless mode, Ask falls back to Approved.
    assert!(
        matches!(result, AuthorizeToolResult::Approved),
        "headless Ask should translate to Approved, got {:?}",
        result
    );
}

#[test]
fn should_block_when_before_tool_call_in_non_headless_mode() {
    // Given: a PermissionManager in Manual mode (default), no rules, NOT headless.
    // "Read" is in the default-approve list, so we use a tool that will hit
    // FallbackAskPolicy.
    let manager = PermissionManager::new(PermissionMode::Manual, Vec::new());
    let cancel = CancelToken::new();
    let args = serde_json::Value::Null;
    let display = ToolInputDisplay::Generic {
        summary: "test".to_string(),
    };
    let effective_rules = manager.effective_rules();
    let session_approvals = std::collections::HashSet::new();

    let ctx = PermissionPolicyContext {
        tool_name: "SomeRandomTool",
        tool_call_id: "call-1",
        args: &args,
        approval_rule: "",
        display: &display,
        accesses: &[],
        mode: PermissionMode::Manual,
        rules: &effective_rules,
        session_approvals: &session_approvals,
        cancel: &cancel,
    };

    // When: call before_tool_call (non-headless — Ask should be Blocked).
    let result = manager.before_tool_call(&ctx);

    // Then: in non-headless mode, Ask should be Blocked.
    assert!(
        matches!(result, AuthorizeToolResult::Blocked { .. }),
        "non-headless Ask should translate to Blocked, got {:?}",
        result
    );
}

/// A unique marker file path for observing that a hook command ran.
fn permission_result_marker(label: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "mirri_{label}_{}_{nanos}.marker",
        std::process::id()
    ))
}

/// Poll briefly for the marker file. `PermissionManager::before_tool_call`
/// fires the `PermissionResult` hook fire-and-forget on a worker thread, so
/// the write may land slightly after the call returns.
fn wait_for_marker(marker: &std::path::Path) -> bool {
    for _ in 0..50 {
        if marker.exists() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    false
}

#[test]
fn should_fire_permission_result_when_before_tool_call_approves_in_yolo_mode() {
    // Given: a PermissionManager wired with services carrying a hook engine
    // that registers a PermissionResult hook touching a marker file.
    let marker = permission_result_marker("pr_approved");
    let engine = std::sync::Arc::new(HookEngine::new());
    engine.register(HookDef::new(
        HookEventType::PermissionResult,
        format!("touch {}", marker.display()),
    ));
    let manager = PermissionManager::with_services(
        PermissionMode::Yolo,
        Vec::new(),
        PermissionServices {
            hook_engine: Some(std::sync::Arc::clone(&engine)),
            ..PermissionServices::default()
        },
    );
    let cancel = CancelToken::new();
    let args = serde_json::Value::Null;
    let display = ToolInputDisplay::Generic {
        summary: "test".to_string(),
    };
    let effective_rules = manager.effective_rules();
    let session_approvals = std::collections::HashSet::new();
    let ctx = PermissionPolicyContext {
        tool_name: "Read",
        tool_call_id: "call-1",
        args: &args,
        approval_rule: "",
        display: &display,
        accesses: &[],
        mode: PermissionMode::Yolo,
        rules: &effective_rules,
        session_approvals: &session_approvals,
        cancel: &cancel,
    };

    // When: before_tool_call approves the tool call (Yolo auto-approve).
    let result = manager.before_tool_call(&ctx);

    // Then: the tool is approved and the PermissionResult hook fires with the
    // approval outcome.
    assert!(
        matches!(result, AuthorizeToolResult::Approved),
        "Yolo mode should approve, got {:?}",
        result
    );
    assert!(
        wait_for_marker(&marker),
        "PermissionResult hook should fire after an approved decision"
    );
    let _ = std::fs::remove_file(&marker);
}

#[test]
fn should_fire_permission_result_when_before_tool_call_rejects_with_deny_rule() {
    // Given: a PermissionManager in Auto mode with a deny rule for "Bash" and
    // a hook engine registering a PermissionResult hook touching a marker.
    let marker = permission_result_marker("pr_rejected");
    let engine = std::sync::Arc::new(HookEngine::new());
    engine.register(HookDef::new(
        HookEventType::PermissionResult,
        format!("touch {}", marker.display()),
    ));
    let rules = vec![PermissionRule {
        decision: RuleDecision::Deny,
        scope: RuleScope::User,
        pattern: "Bash".to_string(),
        reason: None,
    }];
    let manager = PermissionManager::with_services(
        PermissionMode::Auto,
        rules,
        PermissionServices {
            hook_engine: Some(std::sync::Arc::clone(&engine)),
            ..PermissionServices::default()
        },
    );
    let cancel = CancelToken::new();
    let args = serde_json::Value::Null;
    let display = ToolInputDisplay::Generic {
        summary: "test".to_string(),
    };
    let effective_rules = manager.effective_rules();
    let session_approvals = std::collections::HashSet::new();
    let ctx = PermissionPolicyContext {
        tool_name: "Bash",
        tool_call_id: "call-2",
        args: &args,
        approval_rule: "",
        display: &display,
        accesses: &[],
        mode: PermissionMode::Auto,
        rules: &effective_rules,
        session_approvals: &session_approvals,
        cancel: &cancel,
    };

    // When: before_tool_call rejects the tool call via the deny rule.
    let result = manager.before_tool_call(&ctx);

    // Then: the tool is blocked and the PermissionResult hook fires with the
    // rejection outcome.
    assert!(
        matches!(result, AuthorizeToolResult::Blocked { .. }),
        "deny rule should block, got {:?}",
        result
    );
    assert!(
        wait_for_marker(&marker),
        "PermissionResult hook should fire after a rejected decision"
    );
    let _ = std::fs::remove_file(&marker);
}

// ===========================================================================
// 3. Tool Registry + Permission + Execution Pipeline Integration
// ===========================================================================

/// Hooks that deny a specific tool name.
struct DenyToolHooks {
    denied_tool: String,
}

impl DenyToolHooks {
    fn new(denied_tool: &str) -> Self {
        Self {
            denied_tool: denied_tool.to_string(),
        }
    }
}

impl LoopHooks for DenyToolHooks {
    fn authorize_tool_execution(
        &self,
        ctx: AuthorizeToolContext<'_>,
    ) -> HookFuture<EngineResult<AuthorizeToolResult>> {
        let denied = self.denied_tool.clone();
        let tool_name = ctx.tool_name.clone();
        Box::pin(async move {
            if tool_name == denied {
                Ok(AuthorizeToolResult::Blocked {
                    reason: format!("Tool {} is denied by policy", denied),
                })
            } else {
                Ok(AuthorizeToolResult::Approved)
            }
        })
    }
}

#[tokio::test]
async fn should_block_denied_and_execute_allowed_when_mixed_tool_batch_with_permission_hooks() {
    // Given: ToolManager with Echo and ErrorTool, hooks that deny ErrorTool.
    let tools = make_tool_manager_with_echo_and_error();
    let mut validators = ValidatorCache::new();
    let cancel = CancelToken::new();
    let hooks = DenyToolHooks::new("ErrorTool");

    // When: execute a batch with both an allowed and a denied tool.
    let results = run_tool_batch(
        &[
            (
                "Echo".to_string(),
                r#"{"message":"allowed"}"#.to_string(),
                "call-allowed".to_string(),
            ),
            (
                "ErrorTool".to_string(),
                "{}".to_string(),
                "call-denied".to_string(),
            ),
        ],
        &tools,
        &mut validators,
        "turn-1",
        1,
        &cancel,
        None,
        &hooks,
        None,
        None,
    )
    .await;

    // Then: both results returned — allowed executed, denied blocked.
    assert_eq!(results.len(), 2);

    let echo_result = results
        .iter()
        .find(|(id, _)| id == "call-allowed")
        .expect("echo result should exist");
    assert!(!echo_result.1.is_error, "Echo should succeed");
    assert!(
        echo_result
            .1
            .output
            .as_text()
            .unwrap_or_default()
            .contains("allowed"),
        "Echo output should contain the message"
    );

    let denied_result = results
        .iter()
        .find(|(id, _)| id == "call-denied")
        .expect("denied result should exist");
    assert!(
        denied_result.1.is_error,
        "denied tool should return error result"
    );
    assert!(
        denied_result
            .1
            .output
            .as_text()
            .unwrap_or_default()
            .contains("denied"),
        "denied result should mention denial, got: {:?}",
        denied_result.1.output
    );
}

// ===========================================================================
// 4. Loop Engine + Context Memory Integration
// ===========================================================================

#[tokio::test]
async fn should_update_history_correctly_when_executing_loop_step() {
    // Given: mock LLM that returns a tool call then a final response,
    // a ToolManager with Echo, and an empty ContextMemory.
    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![make_tool_call(
            "call-1",
            "Echo",
            serde_json::json!({"message": "world"}),
        )]),
        MockResponse::end_turn("All done!"),
    ]);
    let hooks = TestHooks::default();
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    // When: run a full turn (user message → tool call → tool result → final response).
    let result = run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        None,
        None,
        None,
    )
    .await
    .expect("turn should succeed");

    // Then: turn completed with 2 steps.
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    assert_eq!(result.steps, 2);
    assert_eq!(result.usage.input_tokens, 200);
    assert_eq!(result.usage.output_tokens, 100);

    // ContextMemory history should contain:
    // 1. User message (from message builder)
    // 2. Assistant message with tool_call
    // 3. Tool result message
    // 4. Assistant message with final text
    let history = {
        let guard = memory.lock().unwrap();
        guard.history().to_vec()
    };

    // The message builders always produce a user "Hello" message, so we expect:
    // [user, assistant(tool_call), tool(result), assistant(text)]
    assert!(
        history.len() >= 3,
        "expected at least 3 messages, got {}",
        history.len()
    );

    // Find the tool result message.
    let tool_msgs: Vec<_> = history
        .iter()
        .filter(|m| m.message.role == Role::Tool)
        .collect();
    assert_eq!(tool_msgs.len(), 1, "expected 1 tool result message");
    assert_eq!(tool_msgs[0].message.tool_call_id.as_deref(), Some("call-1"));
    assert!(
        tool_msgs[0].message.text_content().contains("world"),
        "tool result should contain echoed message"
    );

    // Find assistant messages with tool calls.
    let assistant_with_tools: Vec<_> = history
        .iter()
        .filter(|m| m.message.role == Role::Assistant && !m.message.tool_calls.is_empty())
        .collect();
    assert_eq!(assistant_with_tools.len(), 1);
    assert_eq!(
        assistant_with_tools[0].message.tool_calls[0].function.name,
        "Echo"
    );
}

// ===========================================================================
// 5. Loop Engine + Compaction Integration
// ===========================================================================

#[tokio::test]
async fn should_compact_memory_when_context_near_token_limit() {
    // Given: a ContextMemory with many messages (approaching token limit)
    // and a mock LLM that returns a compaction summary.
    let llm = MockLlm::new(vec![MockResponse::end_turn(
        "This is a summary of the conversation.",
    )]);
    let cancel = CancelToken::new();
    let tools = ToolManager::new();
    let mut memory = ContextMemory::new();

    // Fill memory with 20 user+assistant pairs to simulate a long conversation.
    // The user bodies are large so the compaction's 20k-token keep-window
    // genuinely trims — a tiny memory cannot pass the shrink check
    // (P8a-3 变小校验) and is rejected by design.
    for i in 0..20 {
        memory.append_user_message(
            vec![ContentPart::Text {
                text: format!("User message number {i}: {}", "x".repeat(16_000)),
            }],
            PromptOrigin::User,
        );
        // Simulate an assistant response by directly appending.
        memory.append_message(mirri_core::context::ContextMessage {
            message: Message {
                role: Role::Assistant,
                name: None,
                content: vec![ContentPart::Text {
                    text: format!("Assistant response number {}", i),
                }],
                tool_calls: Vec::new(),
                tool_call_id: None,
                partial: false,
            },
            origin: None,
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        });
    }

    let tokens_before = memory.token_count_with_pending();
    assert!(
        tokens_before > 0,
        "memory should have non-zero token count before compaction"
    );
    let messages_before = memory.history().len();
    assert_eq!(
        messages_before, 40,
        "expected 40 messages before compaction"
    );

    // When: run a compaction round.
    let result = compaction_round(CompactionRoundInput {
        llm: &llm,
        memory: &mut memory,
        cancel: &cancel,
        tools: &tools,
        custom_instruction: None,
        max_output_size: None,
        todo_list: None,
    })
    .await
    .expect("compaction should succeed");

    // Then: memory is compacted — fewer messages, lower token count.
    let messages_after = memory.history().len();

    assert!(
        messages_after < messages_before,
        "compacted memory should have fewer messages: {} < {}",
        messages_after,
        messages_before
    );
    assert!(
        result.compacted_count < messages_before,
        "compacted_count should cover the compacted prefix, not the retention tail"
    );
    // New placement form (ADR-001/002): the compacted prefix plus the
    // verbatim retention tail must cover the original history.
    assert_eq!(
        result.compacted_count + result.retained_tail_count,
        messages_before,
        "prefix + retention tail must cover the original history"
    );
    assert!(!result.summary.is_empty(), "summary should not be empty");
}

#[tokio::test]
async fn should_trigger_compaction_when_token_count_exceeds_threshold() {
    // Given: a FullCompaction with a known max_context_tokens and a memory
    // whose token count exceeds the trigger ratio.
    let mut compaction = FullCompaction::new(CompactionStrategy {
        max_context_tokens: 10_000,
        trigger_ratio: 0.85,
        ..Default::default()
    });
    compaction.reset_for_turn();

    let mut memory = ContextMemory::new();
    // Fill memory with enough content to exceed 85% of 10,000 tokens.
    // Each message is ~4 tokens, so we need ~2,200 messages to reach ~8,500 tokens.
    // Instead, use a large text message.
    memory.append_user_message(
        vec![ContentPart::Text {
            text: "x".repeat(40_000), // ~10,000 tokens
        }],
        PromptOrigin::User,
    );

    // When: check auto-compaction.
    let should_compact = compaction
        .check_auto_compaction(&memory, false)
        .expect("check should not error");

    // Then: compaction should be triggered.
    assert!(
        should_compact,
        "auto-compaction should trigger when token count exceeds 85% of max"
    );
}

// ===========================================================================
// 6. TurnFlow + Loop Engine + Context Memory Integration
// ===========================================================================

#[tokio::test]
async fn should_flush_steer_and_accumulate_usage_when_running_turn() {
    // Given: TurnFlow, mock LLM that calls a tool then ends, and ContextMemory.
    // The steer() method only buffers when a turn is active — we test the
    // steer-flush path by verifying that after the turn, the steer buffer
    // is empty (flushed during before_step hooks).
    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![make_tool_call(
            "call-1",
            "Echo",
            serde_json::json!({"message": "hi"}),
        )])
        .with_usage(100, 50),
        MockResponse::end_turn("Done!").with_usage(200, 75),
    ]);
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());

    // When: run one turn (steer buffer starts empty, hooks flush during steps).
    let result = turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Hello".to_string(),
            }],
            PromptOrigin::User,
            memory.clone(),
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(llm))),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("turn should succeed");

    // Then: turn completed, steer buffer empty, usage accumulated across steps.
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    assert_eq!(result.steps, 2);
    assert!(
        !turn_flow.has_pending_steers(),
        "steer buffer should be empty"
    );
    assert_eq!(
        result.usage.input_tokens, 300,
        "input tokens should accumulate"
    );
    assert_eq!(
        result.usage.output_tokens, 125,
        "output tokens should accumulate"
    );

    // ContextMemory should contain the user message and tool exchange.
    let history = {
        let guard = memory.lock().unwrap();
        guard.history().to_vec()
    };
    let has_user = history.iter().any(|m| m.message.role == Role::User);
    assert!(has_user, "should have at least one user message");
    let has_tool = history.iter().any(|m| m.message.role == Role::Tool);
    assert!(has_tool, "should have a tool result message");
}

// ===========================================================================
// 7. TurnFlow + Tool Call Deduplication Integration
// ===========================================================================

#[tokio::test]
async fn should_detect_duplicates_when_repeated_tool_calls_across_steps() {
    // Given: a TurnFlow and a mock LLM that returns the same Echo tool call
    // for 3 consecutive steps, then ends.
    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![make_tool_call(
            "call-1",
            "Echo",
            serde_json::json!({"message": "same"}),
        )]),
        MockResponse::tool_use(vec![make_tool_call(
            "call-2",
            "Echo",
            serde_json::json!({"message": "same"}),
        )]),
        MockResponse::tool_use(vec![make_tool_call(
            "call-3",
            "Echo",
            serde_json::json!({"message": "same"}),
        )]),
        MockResponse::end_turn("Done after dedup"),
    ]);
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());

    // When: run one turn — the deduplicator should detect the repeated
    // identical tool calls across steps and eventually add reminders.
    let result = turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Keep calling Echo".to_string(),
            }],
            PromptOrigin::User,
            memory.clone(),
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(llm))),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("turn should succeed");

    // Then: turn completes (either via EndTurn or dedup-forced stop).
    assert_eq!(result.steps, 4, "should execute 4 steps");

    // Verify that tool results exist in history — the cross-step dedup
    // escalation should not prevent results from being recorded.
    let history = {
        let guard = memory.lock().unwrap();
        guard.history().to_vec()
    };
    let tool_msgs: Vec<_> = history
        .iter()
        .filter(|m| m.message.role == Role::Tool)
        .collect();
    assert_eq!(
        tool_msgs.len(),
        3,
        "expected 3 tool result messages (one per step)"
    );

    // At least one of the later tool results should contain a dedup reminder
    // (streak ≥ 3 triggers reminder1).
    let has_reminder = tool_msgs.iter().any(|m| {
        m.message.text_content().contains("system-reminder")
            || m.message.text_content().contains("same tool")
            || m.message.text_content().contains("repeatedly")
    });
    assert!(
        has_reminder,
        "expected at least one dedup reminder in tool results after 3 identical calls"
    );
}

#[tokio::test]
async fn should_return_synthetic_result_when_same_step_duplicate_tool_call() {
    // Given: TurnFlow and mock LLM that calls the same tool twice in one step.
    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![
            make_tool_call(
                "call-1",
                "Echo",
                serde_json::json!({"message": "hello-echo"}),
            ),
            make_tool_call(
                "call-2",
                "Echo",
                serde_json::json!({"message": "hello-echo"}),
            ),
        ]),
        MockResponse::end_turn("Done"),
    ]);
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());

    // When: run the turn.
    let result = turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Hello".to_string(),
            }],
            PromptOrigin::User,
            memory.clone(),
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(llm))),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("turn should succeed");

    // Then: both tool calls produce results, but one is synthetic (empty).
    // The deduplicator returns a synthetic empty result for the second
    // identical call. We verify that at least one result contains the
    // echoed message and at least one does not.
    assert_eq!(result.steps, 2);

    let history = {
        let guard = memory.lock().unwrap();
        guard.history().to_vec()
    };
    let tool_msgs: Vec<_> = history
        .iter()
        .filter(|m| m.message.role == Role::Tool)
        .collect();
    assert_eq!(tool_msgs.len(), 2, "expected 2 tool results");

    // One result should contain the echoed "hello-echo" (the real execution),
    // and the other should NOT — it should be the synthetic dedup result
    // (a "result pending" placeholder, which does not contain "hello-echo").
    let texts: Vec<String> = tool_msgs.iter().map(|m| m.message.text_content()).collect();
    let has_real = texts.iter().any(|t| t.contains("hello-echo"));
    let has_synthetic = texts.iter().any(|t| !t.contains("hello-echo"));
    assert!(
        has_real,
        "at least one tool result should contain the echoed message"
    );
    assert!(
        has_synthetic,
        "at least one tool result should be synthetic (no echo)"
    );
}

// ===========================================================================
// 8. Scope Hierarchy + Engine Components Integration
// ===========================================================================

#[tokio::test]
async fn should_share_arc_pointers_when_accessing_agent_components_from_full_scope_tree() {
    // Given: a complete scope tree (App → Workspace → Session → Agent).
    let app = mirri_core::AppScope::new(mirri_core::AppScopeConfig::default());
    let ws = app
        .create_workspace(
            mirri_core::WorkspaceId::new("ws-1"),
            std::path::PathBuf::from("/tmp"),
        )
        .await
        .unwrap();
    let session = ws
        .create_session(
            mirri_core::SessionId::new("sess-1"),
            mirri_core::SessionMetadata::default(),
        )
        .await
        .unwrap();
    let agent = session
        .create_agent(
            mirri_core::AgentId::new("agent-1"),
            mirri_core::AgentScopeConfig::default(),
        )
        .await
        .unwrap();

    // When: get Arc handles to all engine components.
    let cm1 = agent.context_memory();
    let cm2 = agent.context_memory();
    let tm1 = agent.tool_manager();
    let tm2 = agent.tool_manager();
    let pm1 = agent.permission_manager();
    let pm2 = agent.permission_manager();

    // Then: all Arc handles point to the same instance.
    assert!(
        Arc::ptr_eq(&cm1, &cm2),
        "ContextMemory Arc should be shared"
    );
    assert!(Arc::ptr_eq(&tm1, &tm2), "ToolManager Arc should be shared");
    assert!(
        Arc::ptr_eq(&pm1, &pm2),
        "PermissionManager Arc should be shared"
    );

    // The engine components should be in their initial state.
    {
        let guard = cm1.lock().unwrap();
        assert!(guard.history().is_empty(), "ContextMemory should be empty");
    }
    assert_eq!(
        tm1.read().all_tool_names().len(),
        30,
        "ToolManager should have all built-in tools registered"
    );
    assert_eq!(
        pm1.mode(),
        PermissionMode::default(),
        "PermissionManager should be in default mode"
    );
}

// ===========================================================================
// 9. Scope Hierarchy + Multi-Agent Integration
// ===========================================================================

#[tokio::test]
async fn should_have_independent_context_memories_when_multiple_agents_execute_turns() {
    // Given: a session with two agents, each with its own ContextMemory.
    let app = mirri_core::AppScope::new(mirri_core::AppScopeConfig::default());
    let ws = app
        .create_workspace(
            mirri_core::WorkspaceId::new("ws-multi"),
            std::path::PathBuf::from("/tmp"),
        )
        .await
        .unwrap();
    let session = ws
        .create_session(
            mirri_core::SessionId::new("sess-multi"),
            mirri_core::SessionMetadata::default(),
        )
        .await
        .unwrap();

    let agent_a = session
        .create_agent(
            mirri_core::AgentId::new("agent-a"),
            mirri_core::AgentScopeConfig::default(),
        )
        .await
        .unwrap();
    let agent_b = session
        .create_agent(
            mirri_core::AgentId::new("agent-b"),
            mirri_core::AgentScopeConfig::default(),
        )
        .await
        .unwrap();

    // Verify the ContextMemory Arcs are NOT the same.
    let cm_a = agent_a.context_memory();
    let cm_b = agent_b.context_memory();
    assert!(
        !Arc::ptr_eq(&cm_a, &cm_b),
        "different agents should have different ContextMemory instances"
    );

    // When: write to agent A's memory only.
    {
        let mut guard = cm_a.lock().unwrap();
        guard.append_user_message(
            vec![ContentPart::Text {
                text: "Message for agent A".to_string(),
            }],
            PromptOrigin::User,
        );
    }

    // Then: agent B's memory is unaffected.
    {
        let guard_a = cm_a.lock().unwrap();
        let guard_b = cm_b.lock().unwrap();
        assert_eq!(guard_a.history().len(), 1, "agent A should have 1 message");
        assert_eq!(guard_b.history().len(), 0, "agent B should have 0 messages");
    }
}

#[tokio::test]
async fn should_isolate_state_when_multiple_agents_run_concurrent_turns() {
    // Given: a session with 3 agents, each with its own TurnFlow and LLM.
    let app = Arc::new(mirri_core::AppScope::new(
        mirri_core::AppScopeConfig::default(),
    ));
    let ws = app
        .create_workspace(
            mirri_core::WorkspaceId::new("ws-concurrent"),
            std::path::PathBuf::from("/tmp"),
        )
        .await
        .unwrap();
    let session = Arc::new(
        ws.create_session(
            mirri_core::SessionId::new("sess-concurrent"),
            mirri_core::SessionMetadata::default(),
        )
        .await
        .unwrap(),
    );

    let mut agents = Vec::new();
    for i in 0..3 {
        let agent = session
            .create_agent(
                mirri_core::AgentId::new(format!("agent-{i}")),
                mirri_core::AgentScopeConfig::default(),
            )
            .await
            .unwrap();
        agents.push(agent);
    }

    let cancel = Arc::new(CancelToken::new());

    // When: spawn concurrent tasks, each running a turn on its own agent.
    let mut handles = Vec::new();
    for (i, agent) in agents.into_iter().enumerate() {
        let llm = MockLlm::new(vec![MockResponse::end_turn(&format!(
            "Response from agent {}",
            i
        ))]);
        let cancel = cancel.clone();
        let memory = agent.context_memory();

        let handle = tokio::spawn(async move {
            let tools = ToolManager::new();
            let mut validators = ValidatorCache::new();
            let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());

            turn_flow
                .run_one_turn(
                    vec![ContentPart::Text {
                        text: format!("Hello from agent {}", i),
                    }],
                    PromptOrigin::User,
                    memory.clone(),
                    &tools,
                    Arc::new(FixedLlmResolver::new(Arc::new(llm))),
                    &cancel,
                    &mut validators,
                    None,
                    None,
                )
                .await
                .expect("turn should succeed");

            memory
        });
        handles.push(handle);
    }

    // Then: each agent's memory has exactly 2 messages (user + assistant).
    for (i, handle) in handles.into_iter().enumerate() {
        let memory = handle.await.expect("task should not panic");
        let guard = memory.lock().unwrap();
        let history = guard.history();
        assert_eq!(
            history.len(),
            2,
            "agent {} should have 2 messages, got {}",
            i,
            history.len()
        );
        assert_eq!(history[0].message.role, Role::User);
        assert_eq!(history[1].message.role, Role::Assistant);
    }
}

// ===========================================================================
// 10. Full Pipeline: Scope + TurnFlow + Loop Engine + Tools + Permission + Compaction
// ===========================================================================

#[tokio::test]
async fn should_update_all_state_correctly_when_executing_complete_turn_through_full_pipeline() {
    // Given: complete scope tree → AgentScope with all engine components.
    let app = mirri_core::AppScope::new(mirri_core::AppScopeConfig::default());
    let ws = app
        .create_workspace(
            mirri_core::WorkspaceId::new("ws-full"),
            std::path::PathBuf::from("/tmp"),
        )
        .await
        .unwrap();
    let session = ws
        .create_session(
            mirri_core::SessionId::new("sess-full"),
            mirri_core::SessionMetadata::default(),
        )
        .await
        .unwrap();

    let agent_config = mirri_core::AgentScopeConfig {
        permission_mode: PermissionMode::Yolo,
        ..Default::default()
    };
    let agent = session
        .create_agent(mirri_core::AgentId::new("agent-full"), agent_config)
        .await
        .unwrap();

    // The agent's ToolManager is behind Arc (immutable). We create a separate
    // ToolManager with the Echo tool for the turn, simulating how a host would
    // configure tools before launching the agent loop.
    let tools = make_tool_manager_with_echo();

    // Verify permission mode propagated from config.
    assert_eq!(agent.permission_manager().mode(), PermissionMode::Yolo);

    // Mock LLM: first calls Echo, then returns final text.
    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![make_tool_call(
            "call-1",
            "Echo",
            serde_json::json!({"message": "pipeline-test"}),
        )])
        .with_usage(150, 80),
        MockResponse::end_turn("Pipeline complete!").with_usage(250, 120),
    ]);

    let memory = agent.context_memory();
    let cancel = CancelToken::new();
    let mut validators = ValidatorCache::new();
    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());

    // When: execute a full turn through the entire pipeline.
    let result = turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Run the full pipeline".to_string(),
            }],
            PromptOrigin::User,
            memory.clone(),
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(llm))),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("full pipeline turn should succeed");

    // Then: verify all state is correctly updated.

    // Turn result.
    assert_eq!(result.turn_id, 1);
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    assert_eq!(result.steps, 2, "should execute 2 steps (tool + final)");
    assert_eq!(
        result.usage.input_tokens, 400,
        "input tokens should accumulate"
    );
    assert_eq!(
        result.usage.output_tokens, 200,
        "output tokens should accumulate"
    );
    assert!(!result.blocked_by_prompt_hook);
    assert!(!turn_flow.is_turn_active());

    // ContextMemory history.
    let history = {
        let guard = memory.lock().unwrap();
        guard.history().to_vec()
    };

    // Expected: [user, assistant(tool_call), tool(result), assistant(final_text)]
    assert!(
        history.len() >= 4,
        "expected at least 4 messages in history, got {}",
        history.len()
    );

    // First user message.
    assert_eq!(history[0].message.role, Role::User);
    assert!(
        history[0]
            .message
            .text_content()
            .contains("Run the full pipeline")
    );

    // Assistant with tool call.
    let assistant_with_tool = history
        .iter()
        .find(|m| m.message.role == Role::Assistant && !m.message.tool_calls.is_empty())
        .expect("should find assistant message with tool call");
    assert_eq!(
        assistant_with_tool.message.tool_calls[0].function.name,
        "Echo"
    );

    // Tool result.
    let tool_msg = history
        .iter()
        .find(|m| m.message.role == Role::Tool)
        .expect("should find tool result message");
    assert_eq!(tool_msg.message.tool_call_id.as_deref(), Some("call-1"));
    assert!(
        tool_msg.message.text_content().contains("pipeline-test"),
        "tool result should contain echoed message"
    );

    // Final assistant text.
    let final_assistant = history
        .iter()
        .rfind(|m| m.message.role == Role::Assistant)
        .expect("should find final assistant message");
    assert!(
        final_assistant
            .message
            .text_content()
            .contains("Pipeline complete!"),
        "final assistant message should contain the response text"
    );

    // Token count should be updated.
    let token_count = {
        let guard = memory.lock().unwrap();
        guard.token_count()
    };
    assert_eq!(
        token_count, 370,
        "token count should reflect last step usage (250 + 120)"
    );
}

#[tokio::test]
async fn should_increment_turn_id_when_second_turn_runs() {
    // Given: a full pipeline setup.
    let app = mirri_core::AppScope::new(mirri_core::AppScopeConfig::default());
    let ws = app
        .create_workspace(
            mirri_core::WorkspaceId::new("ws-turns"),
            std::path::PathBuf::from("/tmp"),
        )
        .await
        .unwrap();
    let session = ws
        .create_session(
            mirri_core::SessionId::new("sess-turns"),
            mirri_core::SessionMetadata::default(),
        )
        .await
        .unwrap();
    let agent = session
        .create_agent(
            mirri_core::AgentId::new("agent-turns"),
            mirri_core::AgentScopeConfig::default(),
        )
        .await
        .unwrap();

    let llm: Arc<dyn Llm> = Arc::new(MockLlm::new(vec![
        MockResponse::end_turn("First response"),
        MockResponse::end_turn("Second response"),
    ]));
    let memory = agent.context_memory();
    let tools = ToolManager::new();
    let cancel = CancelToken::new();
    let mut validators = ValidatorCache::new();
    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());

    // When: run two consecutive turns.
    let result1 = turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "First".to_string(),
            }],
            PromptOrigin::User,
            memory.clone(),
            &tools,
            Arc::new(FixedLlmResolver::new(llm.clone())),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("first turn should succeed");

    let result2 = turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Second".to_string(),
            }],
            PromptOrigin::User,
            memory.clone(),
            &tools,
            Arc::new(FixedLlmResolver::new(llm.clone())),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("second turn should succeed");

    // Then: turn IDs increment and both turns succeed.
    assert_eq!(result1.turn_id, 1);
    assert_eq!(result2.turn_id, 2);
    assert_eq!(result1.steps, 1);
    assert_eq!(result2.steps, 1);

    // Memory should contain messages from both turns.
    let history = {
        let guard = memory.lock().unwrap();
        guard.history().len()
    };
    assert_eq!(
        history, 4,
        "expected 4 messages (2 user + 2 assistant from two turns)"
    );
}

// ===========================================================================
// Helper trait for ToolOutput
// ===========================================================================

trait ToolOutputExt {
    fn as_text(&self) -> Option<String>;
}

impl ToolOutputExt for ToolOutput {
    fn as_text(&self) -> Option<String> {
        match self {
            ToolOutput::Text(t) => Some(t.clone()),
            ToolOutput::Parts(parts) => {
                let mut text = String::new();
                for part in parts {
                    if let ContentPart::Text { text: t } = part {
                        text.push_str(t);
                    }
                }
                if text.is_empty() { None } else { Some(text) }
            }
        }
    }
}
