//! Comprehensive tests for the loop engine module.
//!
//! These tests use a [`MockLlm`] that returns configurable responses from a
//! queue, and [`MockHooks`] that allows controlling hook behavior.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use futures::stream;
use serde_json::Value;

use mirri_core::PromptOrigin;
use mirri_core::cancel::CancelToken;
use mirri_core::context_memory::ContextMemory;
use mirri_core::error::{EngineError, EngineResult};
use mirri_core::hooks::*;
use mirri_core::llm::*;
use mirri_core::loop_engine::*;
use mirri_core::message::{ContentPart, ImageUrlData, Message};
use mirri_core::tool::*;
use mirri_core::tool_registry::{ToolManager, ValidatorCache};
use mirri_core::tools::ReadMediaFileTool;

// ---------------------------------------------------------------------------
// Mock LLM
// ---------------------------------------------------------------------------

/// A pre-configured response for the MockLlm to return.
#[derive(Clone)]
struct MockResponse {
    text: String,
    think_text: String,
    tool_calls: Vec<LlmToolCall>,
    /// Streamed tool-call argument fragments emitted before the block-level
    /// `ToolCall`s (index, fragment, call id, tool name).
    tool_parts: Vec<(u64, String, Option<String>, Option<String>)>,
    stop_reason: String,
    usage: LlmUsage,
    error: Option<String>,
    /// When set, the stream ends after the delivered text with no `Usage` and
    /// no `Done` (mid-stream cancellation shape).
    interrupted: bool,
}

impl MockResponse {
    fn end_turn(text: &str) -> Self {
        Self {
            text: text.to_string(),
            think_text: String::new(),
            tool_calls: Vec::new(),
            tool_parts: Vec::new(),
            stop_reason: "completed".to_string(),
            usage: LlmUsage {
                input_tokens: 100,
                output_tokens: 50,
                ..Default::default()
            },
            error: None,
            interrupted: false,
        }
    }

    fn tool_use(calls: Vec<LlmToolCall>) -> Self {
        Self {
            text: String::new(),
            think_text: String::new(),
            tool_calls: calls,
            tool_parts: Vec::new(),
            stop_reason: "tool_calls".to_string(),
            usage: LlmUsage {
                input_tokens: 100,
                output_tokens: 50,
                ..Default::default()
            },
            error: None,
            interrupted: false,
        }
    }

    fn with_tool_parts(
        mut self,
        parts: Vec<(u64, String, Option<String>, Option<String>)>,
    ) -> Self {
        self.tool_parts = parts;
        self
    }

    fn with_usage(mut self, input: u64, output: u64) -> Self {
        self.usage = LlmUsage {
            input_tokens: input,
            output_tokens: output,
            ..Default::default()
        };
        self
    }

    /// A response whose stream is cancelled mid-flight: the partial text is
    /// delivered (delta + block part) but the stream ends with no `Usage` and
    /// no `Done` event — the shape the provider pump produces when the cancel
    /// token fires mid-stream.
    fn interrupted_after(text: &str) -> Self {
        Self {
            text: text.to_string(),
            think_text: String::new(),
            tool_calls: Vec::new(),
            tool_parts: Vec::new(),
            stop_reason: String::new(),
            usage: LlmUsage::default(),
            error: None,
            interrupted: true,
        }
    }

    fn error(message: &str) -> Self {
        Self {
            text: String::new(),
            think_text: String::new(),
            tool_calls: Vec::new(),
            tool_parts: Vec::new(),
            stop_reason: String::new(),
            usage: LlmUsage::default(),
            error: Some(message.to_string()),
            interrupted: false,
        }
    }

    /// A degenerate response: usage was billed and the stream ended with a
    /// clean natural stop, but no text, no thinking, and no tool calls were
    /// delivered — the shape observed when a model burns output tokens yet
    /// produces nothing visible.
    fn empty_end_turn() -> Self {
        Self::empty_with_stop_reason("completed")
    }

    /// The same degenerate empty response under an arbitrary stop reason —
    /// every non-tool-use, non-filtered stop settles as "completed" on the
    /// wire, so an empty response must fail regardless of which one the
    /// provider used.
    fn empty_with_stop_reason(stop_reason: &str) -> Self {
        Self {
            text: String::new(),
            think_text: String::new(),
            tool_calls: Vec::new(),
            tool_parts: Vec::new(),
            stop_reason: stop_reason.to_string(),
            usage: LlmUsage {
                input_tokens: 8129,
                output_tokens: 109,
                ..Default::default()
            },
            error: None,
            interrupted: false,
        }
    }

    /// An empty stream that ends mid-flight because the cancel token fired —
    /// no text, no `Usage`, no `Done`. The cancellation closure owns this
    /// shape, never the empty-response guard.
    fn interrupted_empty() -> Self {
        Self {
            text: String::new(),
            think_text: String::new(),
            tool_calls: Vec::new(),
            tool_parts: Vec::new(),
            stop_reason: String::new(),
            usage: LlmUsage::default(),
            error: None,
            interrupted: true,
        }
    }
}

/// A mock LLM that pops responses from a queue.
struct MockLlm {
    system_prompt: String,
    model_name: String,
    responses: Arc<Mutex<Vec<MockResponse>>>,
    retryable: bool,
    /// When set, the token is cancelled at the moment `chat()` is invoked —
    // simulating a mid-stream cancellation (the engine sees a stream that
    /// ends without `Done` while the token is set).
    cancel_on_chat: Option<CancelToken>,
    /// The fixed capability reported to the engine. Defaults to a
    /// text-only model (every modality false — undefined is unsupported,
    /// ADR-003 revision); tests opt into media support via
    /// [`with_capability`](Self::with_capability).
    capability: ModelCapability,
    /// Captured per-call request message arrays (for snapshot assertions).
    requests: Arc<Mutex<Vec<Vec<Message>>>>,
    /// Captured per-call tool sets (for tool-visibility assertions).
    tool_sets: Arc<Mutex<Vec<Vec<ToolDefinition>>>>,
}

impl MockLlm {
    fn new(responses: Vec<MockResponse>) -> Self {
        Self {
            system_prompt: "You are a test assistant.".to_string(),
            model_name: "test-model".to_string(),
            responses: Arc::new(Mutex::new(responses)),
            retryable: false,
            cancel_on_chat: None,
            capability: ModelCapability::default(),
            requests: Arc::new(Mutex::new(Vec::new())),
            tool_sets: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn with_retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    fn with_capability(mut self, capability: ModelCapability) -> Self {
        self.capability = capability;
        self
    }

    fn with_cancel_on_chat(mut self, cancel: CancelToken) -> Self {
        self.cancel_on_chat = Some(cancel);
        self
    }

    /// Number of `chat` calls this instance has served — per-instance
    /// assertions for mid-turn model-switch tests.
    fn served_calls(&self) -> usize {
        self.requests.lock().unwrap().len()
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
        &self.capability
    }

    fn is_retryable_error(&self, _error: &ProviderErrorInfo) -> bool {
        self.retryable
    }

    fn chat(&self, params: LlmChatParams) -> EngineResult<LlmEventStream> {
        self.requests.lock().unwrap().push(params.messages.clone());
        self.tool_sets.lock().unwrap().push(params.tools.clone());
        if let Some(cancel) = &self.cancel_on_chat {
            cancel.cancel();
        }
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

/// Build the list of `LlmEvent`s for a `MockResponse`.
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

    // A mid-stream cancellation ends the stream right after the delivered
    // text — no usage, no stop reason.
    if resp.interrupted {
        return events;
    }

    if !resp.think_text.is_empty() {
        events.push(LlmEvent::ThinkPart {
            text: resp.think_text.clone(),
        });
    }

    for (index, fragment, call_id, tool_name) in &resp.tool_parts {
        events.push(LlmEvent::ToolCallPart {
            index: *index,
            arguments_part: fragment.clone(),
            call_id: call_id.clone(),
            tool_name: tool_name.clone(),
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

// ---------------------------------------------------------------------------
// Mock Tool
// ---------------------------------------------------------------------------

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

/// A tool that reports incremental updates through `ctx.progress` before
/// finishing — exercises the live tool-progress channel end to end.
struct ProgressingTool;

impl Tool for ProgressingTool {
    fn name(&self) -> &str {
        "Progressing"
    }

    fn description(&self) -> &str {
        "Emits progress updates then finishes"
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
                    summary: "Progressing".to_string(),
                },
                approval_rule: "Progressing".to_string(),
                stop_batch_after_this: false,
            },
            execute: Box::new(move |ctx| {
                Box::pin(async move {
                    ctx.progress.send(ToolUpdate {
                        kind: ToolUpdateKind::Progress,
                        text: Some("step one".to_string()),
                        percent: Some(25.0),
                        custom_kind: None,
                        custom_data: None,
                    });
                    ctx.progress.send(ToolUpdate {
                        kind: ToolUpdateKind::Status,
                        text: Some("step two".to_string()),
                        percent: Some(75.0),
                        custom_kind: None,
                        custom_data: None,
                    });
                    ToolResult::success("done")
                })
            }),
        })
    }
}

/// A tool that sets stop_turn on its result.
struct StopTurnTool;

impl Tool for StopTurnTool {
    fn name(&self) -> &str {
        "StopTurn"
    }

    fn description(&self) -> &str {
        "A tool that stops the turn"
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
                    summary: "Stop turn".to_string(),
                },
                approval_rule: "StopTurn".to_string(),
                stop_batch_after_this: false,
            },
            execute: Box::new(|_ctx| {
                Box::pin(async {
                    ToolResult {
                        output: ToolOutput::text("stopping turn"),
                        is_error: false,
                        note: None,
                        stop_turn: true,
                        truncated: false,
                        injection: None,
                    }
                })
            }),
        })
    }
}

// ---------------------------------------------------------------------------
// Mock Hooks
// ---------------------------------------------------------------------------

/// Hooks that can be configured to control behavior.
#[derive(Clone, Default)]
struct TestHooks {
    before_step_block: bool,
    before_step_block_reason: Option<String>,
    after_step_stop_turn: bool,
    should_continue: bool,
}

impl LoopHooks for TestHooks {
    fn before_step(&self, _ctx: BeforeStepContext) -> HookFuture<EngineResult<BeforeStepResult>> {
        let block = self.before_step_block;
        let reason = self.before_step_block_reason.clone();
        Box::pin(async move {
            Ok(BeforeStepResult {
                block,
                block_reason: reason,
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

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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

fn make_tool_manager_with_stop_turn() -> ToolManager {
    let mut tm = ToolManager::new();
    tm.set_active_tools(vec!["StopTurn".to_string()]);
    tm.register_builtin(Arc::new(StopTurnTool));
    tm
}

fn make_tool_call(id: &str, name: &str, args: Value) -> LlmToolCall {
    LlmToolCall {
        id: id.to_string(),
        name: name.to_string(),
        args,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_end_turn_in_single_step_when_llm_returns_completed_response() {
    let llm = MockLlm::new(vec![MockResponse::end_turn("Hello!")]);
    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

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

    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    assert_eq!(result.steps, 1);
    assert_eq!(result.usage.input_tokens, 100);
    assert_eq!(result.usage.output_tokens, 50);
}

#[tokio::test]
async fn should_continue_to_next_step_when_tool_call_is_made() {
    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![make_tool_call(
            "call-1",
            "Echo",
            serde_json::json!({"message": "hi"}),
        )]),
        MockResponse::end_turn("Done!"),
    ]);
    let hooks = TestHooks::default();
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

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

    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    assert_eq!(result.steps, 2);
}

#[tokio::test]
async fn should_return_max_steps_exceeded_error_when_step_limit_reached() {
    // Tool use forever — never ends.
    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![make_tool_call(
            "call-1",
            "Echo",
            serde_json::json!({"message": "a"}),
        )]),
        MockResponse::tool_use(vec![make_tool_call(
            "call-2",
            "Echo",
            serde_json::json!({"message": "b"}),
        )]),
        MockResponse::tool_use(vec![make_tool_call(
            "call-3",
            "Echo",
            serde_json::json!({"message": "c"}),
        )]),
    ]);
    let hooks = TestHooks::default();
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    let mut config = RunTurnConfig::new("turn-1");
    config.max_steps = Some(2);

    let result = run_turn(
        config,
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
    .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(matches!(err, EngineError::MaxStepsExceeded(_)));
}

#[tokio::test]
async fn should_abort_when_cancel_token_cancelled_before_turn() {
    let llm = MockLlm::new(vec![MockResponse::end_turn("Hello")]);
    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    // Cancel before running.
    cancel.cancel();

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
    .expect("turn should return (aborted)");

    assert_eq!(result.stop_reason, TurnStopReason::Aborted);
    assert_eq!(result.steps, 0);
}

#[tokio::test]
async fn should_hit_max_steps_error_when_should_continue_returns_true() {
    let llm = MockLlm::new(vec![
        MockResponse::end_turn("First"),
        MockResponse::end_turn("Second"),
    ]);
    let hooks = TestHooks {
        should_continue: true,
        ..Default::default()
    };
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    let mut config = RunTurnConfig::new("turn-1");
    config.max_steps = Some(2);

    let result = run_turn(
        config,
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
    .await;

    // After 2 steps with should_continue=true, max_steps triggers an error.
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(matches!(err, EngineError::MaxStepsExceeded(_)));
}

#[tokio::test]
async fn should_end_turn_when_should_continue_returns_false() {
    let llm = MockLlm::new(vec![MockResponse::end_turn("Done")]);
    let hooks = TestHooks {
        should_continue: false,
        ..Default::default()
    };
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

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

    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    assert_eq!(result.steps, 1);
}

#[tokio::test]
async fn should_aggregate_usage_across_steps_when_multiple_steps_executed() {
    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![make_tool_call(
            "call-1",
            "Echo",
            serde_json::json!({"message": "hi"}),
        )])
        .with_usage(100, 50),
        MockResponse::end_turn("Done").with_usage(200, 75),
    ]);
    let hooks = TestHooks::default();
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

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

    assert_eq!(result.usage.input_tokens, 300);
    assert_eq!(result.usage.output_tokens, 125);
    assert_eq!(result.steps, 2);
}

#[tokio::test]
async fn should_record_tool_call_results_in_history_when_tool_call_executed() {
    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![make_tool_call(
            "call-1",
            "Echo",
            serde_json::json!({"message": "hello world"}),
        )]),
        MockResponse::end_turn("Done"),
    ]);
    let hooks = TestHooks::default();
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

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

    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);

    // The history should contain: assistant (with tool call), tool result, assistant (end turn).
    let history = {
        let guard = memory.lock().unwrap();
        guard.history().to_vec()
    };
    // At least 3 messages: assistant step 1, tool result, assistant step 2.
    assert!(
        history.len() >= 2,
        "expected at least 2 messages in history, got {}",
        history.len()
    );

    // Check the assistant message has the tool call.
    let assistant_msg = &history[0].message;
    assert_eq!(assistant_msg.tool_calls.len(), 1);
    assert_eq!(assistant_msg.tool_calls[0].id, "call-1");

    // Check there's a tool result message.
    let tool_result_msg = &history[1].message;
    assert_eq!(tool_result_msg.role, mirri_core::message::Role::Tool);
    assert_eq!(tool_result_msg.tool_call_id.as_deref(), Some("call-1"));
}

#[tokio::test]
async fn should_pause_turn_with_unexecuted_tool_calls_when_stop_reason_is_paused() {
    let llm = MockLlm::new(vec![MockResponse {
        text: String::new(),
        think_text: String::new(),
        tool_calls: vec![make_tool_call(
            "call-1",
            "Echo",
            serde_json::json!({"message": "hi"}),
        )],
        tool_parts: Vec::new(),
        stop_reason: "paused".to_string(),
        usage: LlmUsage {
            input_tokens: 10,
            output_tokens: 5,
            ..Default::default()
        },
        error: None,
        interrupted: false,
    }]);
    let hooks = TestHooks::default();
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

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

    // Paused → should stop the turn with Paused stop reason.
    assert_eq!(result.stop_reason, TurnStopReason::Paused);
    assert_eq!(result.steps, 1);

    // The tool call should have been recorded but not executed (synthetic error).
    let history = {
        let guard = memory.lock().unwrap();
        guard.history().to_vec()
    };
    assert!(history.len() >= 2, "expected at least 2 messages");

    // Find the tool result message — it should be an error.
    let tool_msgs: Vec<_> = history
        .iter()
        .filter(|m| m.message.role == mirri_core::message::Role::Tool)
        .collect();
    assert!(!tool_msgs.is_empty(), "expected at least one tool message");
    assert!(tool_msgs[0].is_error, "tool result should be an error");
}

#[tokio::test]
async fn should_succeed_on_retry_when_first_attempt_errors() {
    // First call errors, second call succeeds. MockLlm returns from queue.
    let llm = MockLlm::new(vec![
        MockResponse::error("429 Too Many Requests"),
        MockResponse::end_turn("Success after retry"),
    ])
    .with_retryable(true);

    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    let mut config = RunTurnConfig::new("turn-1");
    config.max_retry_attempts = 3;

    let result = run_turn(
        config,
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
    .expect("turn should succeed after retry");

    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    assert_eq!(result.steps, 1);
}

#[tokio::test]
async fn should_return_provider_error_when_retry_attempts_exhausted() {
    let llm = MockLlm::new(vec![
        MockResponse::error("429 Too Many Requests"),
        MockResponse::error("429 Too Many Requests"),
        MockResponse::error("429 Too Many Requests"),
    ])
    .with_retryable(true);

    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    let mut config = RunTurnConfig::new("turn-1");
    config.max_retry_attempts = 2;

    let result = run_turn(
        config,
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
    .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    // T7 (F-P8-8): an exhausted transient rate limit surfaces with its
    // precise domain code (`provider.rate_limit`) instead of the generic
    // provider error — the retries themselves still ran (see the retry
    // budget assertions in the retry-layer tests).
    assert!(matches!(err, EngineError::ProviderRateLimit(_)));
    assert_eq!(err.code().as_str(), "provider.rate_limit");
}

#[tokio::test]
async fn should_fail_without_retry_when_error_is_non_retryable() {
    let llm =
        MockLlm::new(vec![MockResponse::error("400 Bad Request: invalid")]).with_retryable(false);

    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    let mut config = RunTurnConfig::new("turn-1");
    config.max_retry_attempts = 5;

    let result = run_turn(
        config,
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
    .await;

    assert!(result.is_err());
}

#[tokio::test]
async fn should_stop_turn_when_tool_returns_stop_turn_flag() {
    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![make_tool_call(
            "call-1",
            "StopTurn",
            serde_json::json!({}),
        )]),
        // This should never be reached because the tool stops the turn.
        MockResponse::end_turn("Should not reach"),
    ]);
    let hooks = TestHooks::default();
    let tools = make_tool_manager_with_stop_turn();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

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

    // The tool set stop_turn, so effective stop reason becomes EndTurn.
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    assert_eq!(result.steps, 1);
}

#[tokio::test]
async fn should_return_hook_blocked_error_when_before_step_blocks() {
    let llm = MockLlm::new(vec![MockResponse::end_turn("Hello")]);
    let hooks = TestHooks {
        before_step_block: true,
        before_step_block_reason: Some("Blocked by test".to_string()),
        ..Default::default()
    };
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

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
    .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(matches!(err, EngineError::HookBlocked(_)));
}

#[tokio::test]
async fn should_stop_turn_when_after_step_hook_returns_stop_turn() {
    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![make_tool_call(
            "call-1",
            "Echo",
            serde_json::json!({"message": "hi"}),
        )]),
        // Should not be reached because after_step forces stop.
        MockResponse::end_turn("Should not reach"),
    ]);
    let hooks = TestHooks {
        after_step_stop_turn: true,
        ..Default::default()
    };
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

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

    // after_step returns stop_turn=true, so the ToolUse becomes EndTurn.
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    assert_eq!(result.steps, 1);
}

#[tokio::test]
async fn should_stop_turn_when_record_step_usage_returns_stop() {
    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![make_tool_call(
            "call-1",
            "Echo",
            serde_json::json!({"message": "hi"}),
        )]),
        // Should not be reached because record_step_usage stops the turn.
        MockResponse::end_turn("Should not reach"),
    ]);
    let hooks = TestHooks::default();
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    let record_fn: RecordStepUsageFn =
        Box::new(|_usage, _natural_stop, _step| Box::pin(async { Ok(Some(true)) }));

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
        Some(&record_fn),
        None,
    )
    .await
    .expect("turn should succeed");

    // record_step_usage returned Some(true), so ToolUse becomes EndTurn.
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    assert_eq!(result.steps, 1);
}

#[tokio::test]
async fn should_record_think_part_in_assistant_message_when_llm_returns_thinking() {
    let llm = MockLlm::new(vec![MockResponse {
        text: "Response".to_string(),
        think_text: "Thinking...".to_string(),
        tool_calls: Vec::new(),
        tool_parts: Vec::new(),
        stop_reason: "completed".to_string(),
        usage: LlmUsage {
            input_tokens: 10,
            output_tokens: 20,
            ..Default::default()
        },
        error: None,
        interrupted: false,
    }]);
    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

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

    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);

    // Check that the assistant message has both text and think parts.
    let history = {
        let guard = memory.lock().unwrap();
        guard.history().to_vec()
    };
    assert!(!history.is_empty());
    let assistant = &history[0].message;
    assert_eq!(assistant.role, mirri_core::message::Role::Assistant);

    let has_text = assistant.content.iter().any(
        |p| matches!(p, mirri_core::message::ContentPart::Text { text } if text == "Response"),
    );
    let has_think = assistant.content.iter().any(
        |p| matches!(p, mirri_core::message::ContentPart::Think { text } if text == "Thinking..."),
    );
    assert!(has_text, "assistant message should have text part");
    assert!(has_think, "assistant message should have think part");
}

#[tokio::test]
async fn should_record_the_think_part_before_the_text_part_when_llm_returns_both() {
    // Given an LLM response carrying both reasoning and text (streamed
    // reasoning-first, the OpenAI/DeepSeek convention the live projector
    // renders in arrival order)
    let llm = MockLlm::new(vec![MockResponse {
        text: "Response".to_string(),
        think_text: "Thinking...".to_string(),
        tool_calls: Vec::new(),
        tool_parts: Vec::new(),
        stop_reason: "completed".to_string(),
        usage: LlmUsage {
            input_tokens: 10,
            output_tokens: 20,
            ..Default::default()
        },
        error: None,
        interrupted: false,
    }]);
    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    run_turn(
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

    // When the assistant message is materialized, the think part must come
    // BEFORE the text part — the wire/replay path must preserve the streamed
    // arrival order the live projector already renders (reasoning first),
    // or a re-rendered session shows the thinking block after the answer.
    let history = {
        let guard = memory.lock().unwrap();
        guard.history().to_vec()
    };
    let assistant = &history[0].message;
    let first_is_think = matches!(
        assistant.content.first(),
        Some(mirri_core::message::ContentPart::Think { .. })
    );
    assert!(
        first_is_think,
        "assistant content should be [think, text], got {:?}",
        assistant
            .content
            .iter()
            .map(|p| match p {
                mirri_core::message::ContentPart::Text { .. } => "text",
                mirri_core::message::ContentPart::Think { .. } => "think",
                _ => "other",
            })
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn should_return_max_tokens_stop_reason_when_stop_reason_is_truncated() {
    let llm = MockLlm::new(vec![MockResponse {
        text: "Truncated response".to_string(),
        think_text: String::new(),
        tool_calls: Vec::new(),
        tool_parts: Vec::new(),
        stop_reason: "truncated".to_string(),
        usage: LlmUsage {
            input_tokens: 10,
            output_tokens: 4096,
            ..Default::default()
        },
        error: None,
        interrupted: false,
    }]);
    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

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

    assert_eq!(result.stop_reason, TurnStopReason::MaxTokens);
}

#[tokio::test]
async fn should_return_filtered_stop_reason_when_stop_reason_is_filtered() {
    let llm = MockLlm::new(vec![MockResponse {
        text: "[filtered]".to_string(),
        think_text: String::new(),
        tool_calls: Vec::new(),
        tool_parts: Vec::new(),
        stop_reason: "filtered".to_string(),
        usage: LlmUsage {
            input_tokens: 10,
            output_tokens: 5,
            ..Default::default()
        },
        error: None,
        interrupted: false,
    }]);
    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

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

    assert_eq!(result.stop_reason, TurnStopReason::Filtered);
}

#[tokio::test]
async fn should_forward_text_delta_events_to_callback_when_live_events_provided() {
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    let captured_deltas: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
    let deltas_clone = captured_deltas.clone();

    let callback: LiveEventCallback = Arc::new(move |event| {
        if let LiveEvent::TextDelta { text } = event {
            deltas_clone.lock().unwrap().push(text);
        }
    });

    let llm = MockLlm::new(vec![MockResponse::end_turn("Hello world!")]);
    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    let result = run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        Some(&callback),
        None,
        None,
    )
    .await
    .expect("turn should succeed");

    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);

    let deltas = captured_deltas.lock().unwrap();
    assert!(
        !deltas.is_empty(),
        "expected at least one TextDelta live event"
    );
    assert_eq!(deltas[0], "Hello world!");
}

#[tokio::test]
async fn should_forward_tool_call_part_identity_to_callback_when_parts_carry_it() {
    use std::sync::Mutex as StdMutex;

    // Given an LLM response whose tool-call arguments arrive as two streamed
    // fragments, each carrying the call id and tool name
    type ForwardedPart = (Option<String>, Option<String>, String);
    let captured: Arc<StdMutex<Vec<ForwardedPart>>> = Arc::new(StdMutex::new(Vec::new()));
    let captured_clone = captured.clone();

    let callback: LiveEventCallback = Arc::new(move |event| {
        if let LiveEvent::ToolCallPart {
            call_id,
            tool_name,
            arguments_part,
            ..
        } = event
        {
            captured_clone
                .lock()
                .unwrap()
                .push((call_id, tool_name, arguments_part));
        }
    });

    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![LlmToolCall {
            id: "call_1".to_string(),
            name: "search".to_string(),
            args: serde_json::json!({"q": "hello"}),
        }])
        .with_tool_parts(vec![
            (
                0,
                "{\"q\":".to_string(),
                Some("call_1".to_string()),
                Some("search".to_string()),
            ),
            (
                0,
                "hello}".to_string(),
                Some("call_1".to_string()),
                Some("search".to_string()),
            ),
        ]),
        MockResponse::end_turn("done"),
    ]);

    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        Some(&callback),
        None,
        None,
    )
    .await
    .expect("turn should succeed");

    // Then both forwarded fragments keep their identity field-for-field —
    // the consumer can route interleaved parallel calls live
    let parts = captured.lock().unwrap();
    assert_eq!(parts.len(), 2, "expected two ToolCallPart live events");
    for (call_id, tool_name, _arguments_part) in parts.iter() {
        assert_eq!(call_id.as_deref(), Some("call_1"));
        assert_eq!(tool_name.as_deref(), Some("search"));
    }
    assert_eq!(parts[0].2, "{\"q\":");
    assert_eq!(parts[1].2, "hello}");
}

#[tokio::test]
async fn should_forward_tool_progress_updates_to_callback_when_tool_emits_updates() {
    use std::sync::Mutex as StdMutex;

    // Given a tool that emits two updates through its progress handle
    let captured: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
    let captured_clone = captured.clone();

    let callback: LiveEventCallback = Arc::new(move |event| match event {
        LiveEvent::ToolCallStarted { tool_call_id, .. } => {
            captured_clone
                .lock()
                .unwrap()
                .push(format!("started:{tool_call_id}"));
        }
        LiveEvent::ToolProgress {
            tool_call_id,
            update,
        } => {
            captured_clone
                .lock()
                .unwrap()
                .push(format!("progress:{tool_call_id}:{:?}", update.kind));
        }
        LiveEvent::ToolCallCompleted {
            tool_call_id,
            is_error,
            ..
        } => {
            captured_clone
                .lock()
                .unwrap()
                .push(format!("completed:{tool_call_id}:{is_error}"));
        }
        _ => {}
    });

    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![make_tool_call(
            "call-1",
            "Progressing",
            serde_json::json!({}),
        )]),
        MockResponse::end_turn("Done!"),
    ]);
    let hooks = TestHooks::default();
    let mut tools = ToolManager::new();
    tools.set_active_tools(vec!["Progressing".to_string()]);
    tools.register_builtin(Arc::new(ProgressingTool));
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    // When a turn runs the tool with the live callback installed
    let result = run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory,
        &cancel,
        &builders,
        Some(&callback),
        None,
        None,
    )
    .await
    .expect("turn should succeed");

    // Then the events arrive in order: started → both updates → completed
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    let events = captured.lock().unwrap();
    assert_eq!(
        *events,
        vec![
            "started:call-1".to_string(),
            "progress:call-1:Progress".to_string(),
            "progress:call-1:Status".to_string(),
            "completed:call-1:false".to_string(),
        ],
        "expected started → progress ×2 → completed in order"
    );
}

#[tokio::test]
async fn should_emit_step_retrying_event_when_request_level_retry_occurs() {
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    let retry_events: Arc<StdMutex<Vec<(u32, u32, u64)>>> = Arc::new(StdMutex::new(Vec::new()));
    let retry_clone = retry_events.clone();

    let callback: LiveEventCallback = Arc::new(move |event| {
        if let LiveEvent::StepRetrying {
            failed_attempt,
            next_attempt,
            delay_ms,
            ..
        } = event
        {
            retry_clone
                .lock()
                .unwrap()
                .push((failed_attempt, next_attempt, delay_ms));
        }
    });

    let llm = MockLlm::new(vec![
        MockResponse::error("429 Too Many Requests"),
        MockResponse::end_turn("Success"),
    ])
    .with_retryable(true);

    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    let mut config = RunTurnConfig::new("turn-1");
    config.max_retry_attempts = 3;

    let result = run_turn(
        config,
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        Some(&callback),
        None,
        None,
    )
    .await
    .expect("turn should succeed");

    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);

    let events = retry_events.lock().unwrap();
    assert_eq!(events.len(), 1, "expected one StepRetrying event");
    assert_eq!(events[0].0, 1, "failed_attempt should be 1");
    assert_eq!(events[0].1, 2, "next_attempt should be 2");
}

#[tokio::test]
async fn should_retry_failed_step_when_step_level_retry_is_granted() {
    // The mock is not request-level retryable, so `chat_with_retry` surfaces
    // the provider error immediately (max_retry_attempts = 1) and the
    // step-level service gets the chance to re-enqueue the failed step.
    let llm = MockLlm::new(vec![
        MockResponse::error("429 Too Many Requests"),
        MockResponse::end_turn("Recovered"),
    ]);

    let retry_events: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
    let retry_clone = retry_events.clone();
    let callback: LiveEventCallback = Arc::new(move |event| {
        if let LiveEvent::StepRetrying { next_attempt, .. } = event {
            retry_clone.lock().unwrap().push(next_attempt);
        }
    });

    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    let mut config = RunTurnConfig::new("turn-1");
    config.max_retry_attempts = 1; // surface the provider error immediately
    config.step_retry = Some(Arc::new(StepRetryService::with_defaults()));

    let result = run_turn(
        config,
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        Some(&callback),
        None,
        None,
    )
    .await
    .expect("turn should succeed after step-level retry");

    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    // The retried step keeps its step number: exactly one step executed.
    assert_eq!(result.steps, 1);
    let retries = retry_events.lock().unwrap();
    assert_eq!(
        retries.as_slice(),
        &[2],
        "expected one step-level retry with next attempt 2"
    );
}

#[tokio::test]
async fn should_give_up_step_retry_when_step_retry_budget_is_exhausted() {
    // Three identical failures: the first two consume the per-step budget
    // (default max_retries_per_step = 2), the third is a GiveUp and the
    // error surfaces to the caller.
    let llm = MockLlm::new(vec![
        MockResponse::error("connection reset by peer"),
        MockResponse::error("connection reset by peer"),
        MockResponse::error("connection reset by peer"),
    ]);

    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    let mut config = RunTurnConfig::new("turn-1");
    config.max_retry_attempts = 1; // surface the provider error immediately
    config.step_retry = Some(Arc::new(StepRetryService::with_defaults()));

    let result = run_turn(
        config,
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
    .await;

    let error = result.expect_err("step retry budget must be exhausted");
    assert!(error.to_string().contains("connection reset by peer"));
}

#[tokio::test]
async fn should_retry_step_when_llm_returns_empty_end_turn_response() {
    // Given an LLM whose first response is completely empty (usage billed,
    // clean natural stop, but no text, no thinking, no tool calls) and whose
    // second response recovers
    let llm = MockLlm::new(vec![
        MockResponse::empty_end_turn(),
        MockResponse::end_turn("Recovered after empty response"),
    ]);

    let retry_events: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
    let retry_clone = retry_events.clone();
    let callback: LiveEventCallback = Arc::new(move |event| {
        if let LiveEvent::StepRetrying { next_attempt, .. } = event {
            retry_clone.lock().unwrap().push(next_attempt);
        }
    });

    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    let mut config = RunTurnConfig::new("turn-1");
    config.max_retry_attempts = 1;
    config.step_retry = Some(Arc::new(StepRetryService::with_defaults()));

    // When the turn runs
    let result = run_turn(
        config,
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        Some(&callback),
        None,
        None,
    )
    .await
    .expect("the turn should succeed after the empty response was retried");

    // Then the empty step was retried once (same step number) and the
    // recovered answer is the recorded outcome — never a silent completion.
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    assert_eq!(result.steps, 1);
    let retries = retry_events.lock().unwrap();
    assert_eq!(
        retries.as_slice(),
        &[2],
        "expected one step-level retry with next attempt 2"
    );
    let recovered = memory
        .lock()
        .unwrap()
        .messages()
        .iter()
        .any(|m| m.text_content().contains("Recovered after empty response"));
    assert!(recovered, "the retried step's answer must be recorded");
}

#[tokio::test]
async fn should_fail_with_visible_wire_marker_when_empty_response_exhausts_step_retry() {
    // Given an LLM that only ever returns empty responses (no text, no
    // thinking, no tool calls) — the per-step retry budget (default 2) is
    // exhausted by the third attempt
    let llm = MockLlm::new(vec![
        MockResponse::empty_end_turn(),
        MockResponse::empty_end_turn(),
        MockResponse::empty_end_turn(),
    ]);

    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();
    let recorder = CollectingWireRecorder(Arc::new(Mutex::new(Vec::new())));

    let mut config = RunTurnConfig::new("turn-1");
    config.max_retry_attempts = 1;
    config.step_retry = Some(Arc::new(StepRetryService::with_defaults()));

    // When the turn runs
    let result = run_turn(
        config,
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        None,
        None,
        Some(&recorder as &dyn WireOpRecorder),
    )
    .await;

    // Then the turn fails explicitly with the dedicated error — it is never
    // silently completed.
    let error = result.expect_err("an all-empty turn must fail, not complete");
    assert!(
        matches!(error, EngineError::EmptyLlmResponse),
        "expected EmptyLlmResponse, got {error:?}"
    );

    // And the wire carries a visible marker telling the user the model
    // returned nothing (a system-reminder append, same mechanism as the
    // user-cancel closure).
    let ops = recorder.0.lock().unwrap().clone();
    let marker_on_wire = ops.iter().any(|op| match op {
        mirri_proto::WireOp::ContextAppendMessage { message } => message
            .get("content")
            .and_then(|c| c.as_array())
            .map(|parts| {
                parts.iter().any(|p| {
                    p.get("text")
                        .and_then(|t| t.as_str())
                        .is_some_and(|t| t.contains("empty response"))
                })
            })
            .unwrap_or(false),
        _ => false,
    });
    assert!(
        marker_on_wire,
        "the wire log must carry the empty-response marker"
    );

    // And the live memory carries the same reminder, so a resumed session
    // explains why the turn died.
    let marker_in_memory = memory
        .lock()
        .unwrap()
        .messages()
        .iter()
        .any(|m| m.text_content().contains("empty response"));
    assert!(
        marker_in_memory,
        "the memory must carry the empty-response reminder"
    );
}

#[tokio::test]
async fn should_fallback_to_media_degraded_when_payload_too_large() {
    // First step: 413 error → media_degraded fallback → success.
    // Second step: should use media_degraded projection.
    // We can't easily test the sticky flag directly, but we can verify the
    // turn completes successfully with the fallback.
    let llm = MockLlm::new(vec![
        MockResponse::error("413 Payload Too Large"),
        MockResponse::end_turn("Success with degraded"),
    ]);

    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = MessageBuilders {
        normal: Box::new(|| Ok(vec![Message::user("Hello")])),
        media_degraded: Some(Box::new(|| Ok(vec![Message::user("Hello degraded")]))),
        media_stripped: None,
        strict: None,
        format_rejected_flag: Arc::new(AtomicBool::new(false)),
    };

    // The mock LLM is not retryable, so the 413 error will not retry via
    // chat_with_retry. But the media fallback in turn_step should kick in.
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
    .await;

    // The media fallback should succeed.
    assert!(result.is_ok());
    let result = result.unwrap();
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
}

#[tokio::test]
async fn should_strip_image_into_placeholder_for_text_only_model() {
    // Given a conversation whose user message carries an image part (with an
    // archive path) and a text-only model (image_in=false — the MockLlm's
    // fixed capability)
    let llm = Arc::new(MockLlm::new(vec![MockResponse::end_turn("Got it")]));
    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = memory_with_image_message();
    let cancel = CancelToken::new();
    let builders = memory_backed_builders(memory.clone());

    // When the turn executes and the LLM receives the request
    run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(llm.clone() as Arc<dyn Llm>),
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

    // Then the request carries the omission placeholder instead of the
    // image part (ROADMAP §3 P0 main acceptance, normal request shape)
    let requests = llm.requests.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(requests.len(), 1, "exactly one request was served");
    assert_request_has_placeholder_not_image(&requests[0]);

    // And context memory keeps the original image (ADR-002: the projection
    // is pure — only the request copy is adapted)
    let history = {
        let mem = memory.lock().unwrap_or_else(|e| e.into_inner());
        mem.history().to_vec()
    };
    assert!(
        history.iter().any(|m| m
            .message
            .content
            .iter()
            .any(|p| matches!(p, ContentPart::ImageUrl { .. }))),
        "context memory still holds the original image part"
    );
}

#[tokio::test]
async fn should_strip_image_into_placeholder_for_strict_retry_after_structure_error() {
    // Given a KNOWN text-only model whose first response is rejected as a
    // 400 structure error, with a strict fallback builder that rebuilds the
    // request straight from the image-bearing context memory (the exact
    // shape that would otherwise resurrect the ROADMAP §3 error path in
    // fallback form)
    let llm = Arc::new(MockLlm::new(vec![
        MockResponse::error("400 Bad Request: invalid message structure"),
        MockResponse::end_turn("recovered via strict"),
    ]));
    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = memory_with_image_message();
    let cancel = CancelToken::new();
    let builders = fully_memory_backed_builders(memory.clone());

    // When the turn runs and the structure-error fallback chain retries
    // with the strict projection
    run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(llm.clone() as Arc<dyn Llm>),
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
    .expect("turn should succeed via the strict fallback");

    // Then BOTH requests (the initial one and the strict retry) carry the
    // omission placeholder instead of the image part — no request shape may
    // hand an image to a known text-only model
    let requests = llm.requests.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(
        requests.len(),
        2,
        "the initial request plus one strict retry"
    );
    assert_request_has_placeholder_not_image(&requests[0]);
    assert_request_has_placeholder_not_image(&requests[1]);
}

#[tokio::test]
async fn should_strip_image_into_placeholder_for_media_degraded_retry_after_413() {
    // Given a KNOWN text-only model whose first response is rejected as too
    // large, with a media-degraded fallback builder that also rebuilds from
    // the image-bearing context memory
    let llm = Arc::new(MockLlm::new(vec![
        MockResponse::error("413 Payload Too Large"),
        MockResponse::end_turn("recovered via degraded"),
    ]));
    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = memory_with_image_message();
    let cancel = CancelToken::new();
    let builders = fully_memory_backed_builders(memory.clone());

    // When the turn runs and the 413 fallback chain retries with the
    // media-degraded projection
    run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(llm.clone() as Arc<dyn Llm>),
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
    .expect("turn should succeed via the degraded fallback");

    // Then the degraded retry request carries the omission placeholder
    // instead of the image part (and so does the initial request)
    let requests = llm.requests.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(
        requests.len(),
        2,
        "the initial request plus one degraded retry"
    );
    assert_request_has_placeholder_not_image(&requests[0]);
    assert_request_has_placeholder_not_image(&requests[1]);
}

/// Assert that a request carries no image part and that its user-message
/// text contains the given omission-cause marker (US6.1 size-limit vs US6.2
/// format-rejected copy).
fn assert_request_media_placeholder(messages: &[Message], marker: &str) {
    for message in messages {
        for part in &message.content {
            assert!(
                !matches!(part, ContentPart::ImageUrl { .. }),
                "no image part may survive the stripped projection: {part:?}"
            );
        }
    }
    let user = messages
        .iter()
        .find(|m| m.role == mirri_core::message::Role::User)
        .expect("the user message is in the request");
    let placeholder = user
        .content
        .iter()
        .find_map(|part| match part {
            ContentPart::Text { text } if text.contains("omitted") => Some(text.as_str()),
            _ => None,
        })
        .expect("an omission placeholder is in the request");
    assert!(
        placeholder.contains(marker),
        "placeholder should carry the {marker:?} cause: {placeholder}"
    );
}

#[tokio::test]
async fn should_use_format_rejected_placeholders_for_stripped_retry_after_format_error() {
    // Given a KNOWN image-capable model whose first response rejects the
    // image format, with the production builders over image-bearing memory
    let llm = Arc::new(
        MockLlm::new(vec![
            MockResponse::error("400 Unsupported image format"),
            // The stripped retry succeeds with a tool call, so the turn
            // runs a second step — which replays the sticky stripped
            // projection.
            MockResponse::tool_use(vec![LlmToolCall {
                id: "call_1".to_string(),
                name: "search".to_string(),
                args: serde_json::json!({"q": "hello"}),
            }]),
            MockResponse::end_turn("recovered"),
        ])
        .with_capability(ModelCapability {
            image_in: true,
            video_in: true,
            audio_in: true,
            dynamically_loaded_tools: false,
        }),
    );
    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = memory_with_image_message();
    let cancel = CancelToken::new();
    let builders = MessageBuilders::from_shared_memory(memory.clone());

    // When the turn runs through the format-error fallback chain
    run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(llm.clone() as Arc<dyn Llm>),
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
    .expect("turn should succeed via the stripped fallback");

    // Then the initial request carried the image (the model is
    // image-capable), and BOTH the stripped retry and the subsequent sticky
    // stripped replay (step 2) carry the format-rejected placeholder with
    // the conversion guidance — not the size-limit copy (US6.2).
    let requests = llm.requests.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(
        requests.len(),
        3,
        "the initial request, the stripped retry, and the sticky step-2 replay"
    );
    assert!(
        requests[0].iter().any(|m| m
            .content
            .iter()
            .any(|p| matches!(p, ContentPart::ImageUrl { .. }))),
        "the image-capable model receives the image on the initial request"
    );
    for request in [&requests[1], &requests[2]] {
        assert_request_media_placeholder(request, "provider rejected its format");
        let user = request
            .iter()
            .find(|m| m.role == mirri_core::message::Role::User)
            .expect("the user message is in the request");
        let placeholder = user
            .content
            .iter()
            .find_map(|part| match part {
                ContentPart::Text { text } if text.contains("omitted") => Some(text.as_str()),
                _ => None,
            })
            .expect("an omission placeholder is in the request");
        assert!(placeholder.contains("Convert"));
        assert!(!placeholder.contains("size limit"));
    }
}

#[tokio::test]
async fn should_keep_size_limit_placeholders_for_stripped_retry_after_413() {
    // Given a KNOWN image-capable model that keeps rejecting the request as
    // too large (413), with the production builders over image-bearing
    // memory — the chain must walk degraded → stripped
    let llm = Arc::new(
        MockLlm::new(vec![
            MockResponse::error("413 Payload Too Large"),
            MockResponse::error("413 Payload Too Large"),
            MockResponse::end_turn("recovered via stripped"),
        ])
        .with_capability(ModelCapability {
            image_in: true,
            video_in: true,
            audio_in: true,
            dynamically_loaded_tools: false,
        }),
    );
    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = memory_with_image_message();
    let cancel = CancelToken::new();
    let builders = MessageBuilders::from_shared_memory(memory.clone());

    // When the turn runs through the 413 fallback chain
    run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(llm.clone() as Arc<dyn Llm>),
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
    .expect("turn should succeed via the stripped fallback");

    // Then the stripped retry carries the SIZE-LIMIT placeholder copy — the
    // 413 path never flips the format-rejected cause (US6.1 regression
    // guard).
    let requests = llm.requests.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(
        requests.len(),
        3,
        "the initial request, the degraded retry, and the stripped retry"
    );
    // The degraded retry keeps the most recent media part (keep_recent=2).
    assert!(
        requests[1].iter().any(|m| m
            .content
            .iter()
            .any(|p| matches!(p, ContentPart::ImageUrl { .. }))),
        "the degraded projection keeps the recent image"
    );
    assert_request_media_placeholder(&requests[2], "size limit");
    let user = requests[2]
        .iter()
        .find(|m| m.role == mirri_core::message::Role::User)
        .expect("the user message is in the request");
    let placeholder = user
        .content
        .iter()
        .find_map(|part| match part {
            ContentPart::Text { text } if text.contains("omitted") => Some(text.as_str()),
            _ => None,
        })
        .expect("an omission placeholder is in the request");
    assert!(placeholder.contains("Re-read the file"));
    assert!(!placeholder.contains("rejected its format"));
}

#[tokio::test]
async fn should_hide_read_media_file_from_models_without_media_input() {
    // Given a model without image or video input (image_in=false,
    // video_in=false — the MockLlm default; undefined is unsupported,
    // ADR-003 revision) and a tool manager whose active set includes
    // ReadMediaFile
    let llm = Arc::new(MockLlm::new(vec![MockResponse::end_turn("Got it")]));
    let hooks = TestHooks::default();
    let mut tools = ToolManager::new();
    tools.set_active_tools(vec!["ReadMediaFile".to_string()]);
    tools.register_builtin(Arc::new(ReadMediaFileTool::new(
        None,
        Arc::new(mirri_os::LocalFs::new()),
    )));
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    // When the turn executes and the LLM receives the request
    run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(llm.clone() as Arc<dyn Llm>),
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

    // Then the request's tool set contains no ReadMediaFile entry (US4.3: a
    // model known to lack image and video input is not offered the
    // image-reading tool — it would only set up a doomed call)
    let tool_sets = llm.tool_sets.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(tool_sets.len(), 1, "exactly one request was served");
    assert!(
        tool_sets[0].iter().all(|tool| tool.name != "ReadMediaFile"),
        "ReadMediaFile must be hidden from a model known to lack image/video input: {:?}",
        tool_sets[0]
    );
}

#[tokio::test]
async fn should_return_unknown_stop_reason_when_stop_reason_is_unrecognized() {
    let llm = MockLlm::new(vec![MockResponse {
        text: "Something happened".to_string(),
        think_text: String::new(),
        tool_calls: Vec::new(),
        tool_parts: Vec::new(),
        stop_reason: "other".to_string(),
        usage: LlmUsage::default(),
        error: None,
        interrupted: false,
    }]);
    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

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

    assert_eq!(result.stop_reason, TurnStopReason::Unknown);
}

#[tokio::test]
async fn should_execute_all_tool_calls_when_multiple_tool_calls_in_one_step() {
    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![
            make_tool_call("call-1", "Echo", serde_json::json!({"message": "first"})),
            make_tool_call("call-2", "Echo", serde_json::json!({"message": "second"})),
        ]),
        MockResponse::end_turn("Done with both"),
    ]);
    let hooks = TestHooks::default();
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

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

    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    assert_eq!(result.steps, 2);

    // Check both tool results are in the history.
    let history_vec = {
        let guard = memory.lock().unwrap();
        guard.history().to_vec()
    };
    let tool_msgs: Vec<_> = history_vec
        .iter()
        .filter(|m| m.message.role == mirri_core::message::Role::Tool)
        .collect();
    assert_eq!(tool_msgs.len(), 2, "expected 2 tool result messages");
}

#[tokio::test]
async fn should_fail_turn_when_text_is_empty_and_no_retry_service_is_wired() {
    // Given a single completely empty response (no text, no thinking, no
    // tool calls) and no step-level retry service configured
    let llm = MockLlm::new(vec![MockResponse::empty_end_turn()]);
    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    // When the turn runs
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
    .await;

    // Then the turn fails explicitly with the dedicated error — an empty
    // response is never a legitimate turn completion (turn-scheduling
    // US1.4 / SDR-002).
    let error = result.expect_err("an empty response must fail the turn");
    assert!(matches!(error, EngineError::EmptyLlmResponse));
}

#[tokio::test]
async fn should_retry_step_when_llm_returns_empty_response_with_unknown_stop_reason() {
    // Given an LLM whose first response is empty under an unrecognized stop
    // reason ("other" → Unknown) and whose second response recovers
    let llm = MockLlm::new(vec![
        MockResponse::empty_with_stop_reason("other"),
        MockResponse::end_turn("Recovered after unknown empty response"),
    ]);

    let retry_events: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
    let retry_clone = retry_events.clone();
    let callback: LiveEventCallback = Arc::new(move |event| {
        if let LiveEvent::StepRetrying { next_attempt, .. } = event {
            retry_clone.lock().unwrap().push(next_attempt);
        }
    });

    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    let mut config = RunTurnConfig::new("turn-1");
    config.max_retry_attempts = 1;
    config.step_retry = Some(Arc::new(StepRetryService::with_defaults()));

    // When the turn runs
    let result = run_turn(
        config,
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        Some(&callback),
        None,
        None,
    )
    .await
    .expect("the turn should succeed after the empty response was retried");

    // Then the empty step was retried — an Unknown stop also settles as
    // "completed" on the wire, so the guard must catch it (US1.4).
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    assert_eq!(result.steps, 1);
    let retries = retry_events.lock().unwrap();
    assert_eq!(
        retries.as_slice(),
        &[2],
        "expected one step-level retry with next attempt 2"
    );
}

#[tokio::test]
async fn should_fail_turn_when_empty_response_stops_truncated_without_retry_service() {
    // Given a single empty response truncated at the output limit (no
    // retry service wired)
    let llm = MockLlm::new(vec![MockResponse::empty_with_stop_reason("truncated")]);
    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    // When the turn runs
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
    .await;

    // Then the turn fails explicitly — a MaxTokens stop also renders as
    // "completed" on the wire, so an empty truncated response is never a
    // legitimate completion (US1.4).
    let error = result.expect_err("an empty truncated response must fail the turn");
    assert!(matches!(error, EngineError::EmptyLlmResponse));
}

#[tokio::test]
async fn should_settle_aborted_without_step_retry_when_empty_stream_is_interrupted() {
    // Given an empty stream cancelled mid-flight (the cancel token fires at
    // chat time, so the stream ends with no Usage and no Done) and a
    // step-level retry service wired
    let cancel = CancelToken::new();
    let llm =
        MockLlm::new(vec![MockResponse::interrupted_empty()]).with_cancel_on_chat(cancel.clone());

    let retry_events: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
    let retry_clone = retry_events.clone();
    let callback: LiveEventCallback = Arc::new(move |event| {
        if let LiveEvent::StepRetrying { next_attempt, .. } = event {
            retry_clone.lock().unwrap().push(next_attempt);
        }
    });

    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let builders = make_message_builders();

    let mut config = RunTurnConfig::new("turn-1");
    config.max_retry_attempts = 1;
    config.step_retry = Some(Arc::new(StepRetryService::with_defaults()));

    // When the turn runs
    let result = run_turn(
        config,
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        Some(&callback),
        None,
        None,
    )
    .await
    .expect("the cancelled turn should settle through the abort path");

    // Then the turn settles as aborted (the cancellation closure owns the
    // interrupted shape) and the empty-response guard never fires — no
    // step-level retry was attempted (US1.4 acceptance: cancellation does
    // not trigger the empty-response retry).
    assert_eq!(result.stop_reason, TurnStopReason::Aborted);
    assert!(retry_events.lock().unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// Interrupted-turn closure (方式 B 取消闭合) — P8a
// ---------------------------------------------------------------------------

/// A [`WireOpRecorder`] test double that collects every op into a shared
/// vector, in dispatch order.
struct CollectingWireRecorder(Arc<Mutex<Vec<mirri_proto::WireOp>>>);

impl WireOpRecorder for CollectingWireRecorder {
    fn record_op<'a>(
        &'a self,
        op: mirri_proto::WireOp,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        let collected = Arc::clone(&self.0);
        Box::pin(async move {
            collected.lock().unwrap_or_else(|e| e.into_inner()).push(op);
        })
    }
}

/// Message builders that project the given shared memory (normal projection
/// only), so request snapshots reflect the canonical history.
fn memory_backed_builders(memory: Arc<Mutex<ContextMemory>>) -> MessageBuilders {
    let mem_for_normal = memory;
    MessageBuilders {
        normal: Box::new(move || {
            let mem = mem_for_normal.lock().unwrap_or_else(|e| e.into_inner());
            Ok(mem.messages())
        }),
        media_degraded: None,
        media_stripped: None,
        strict: None,
        format_rejected_flag: Arc::new(AtomicBool::new(false)),
    }
}

/// Message builders that project the given shared memory through EVERY
/// projection slot (normal / degraded / stripped / strict), so fallback
/// retries rebuild straight from the canonical history — the shape that
/// must still pass the media adaptation pipeline (plan constraint: every
/// request assembly, including retries and fallbacks).
fn fully_memory_backed_builders(memory: Arc<Mutex<ContextMemory>>) -> MessageBuilders {
    let make = |memory: Arc<Mutex<ContextMemory>>| {
        Box::new(move || {
            let mem = memory.lock().unwrap_or_else(|e| e.into_inner());
            Ok(mem.messages())
        }) as Box<dyn Fn() -> EngineResult<Vec<Message>> + Send + Sync>
    };
    MessageBuilders {
        normal: make(memory.clone()),
        media_degraded: Some(make(memory.clone())),
        media_stripped: Some(make(memory.clone())),
        strict: Some(make(memory)),
        format_rejected_flag: Arc::new(AtomicBool::new(false)),
    }
}

/// Assert that a captured request to the KNOWN text-only mock LLM carries no
/// `ImageUrl` part, and that the user-message position holds the omission
/// placeholder with the archive path alongside the preserved user text
/// (ROADMAP §3 P0 acceptance, applied to a fallback/retry request shape).
fn assert_request_has_placeholder_not_image(messages: &[Message]) {
    for message in messages {
        for part in &message.content {
            assert!(
                !matches!(part, ContentPart::ImageUrl { .. }),
                "no image part may reach a text-only model, even on a fallback \
                 retry: {part:?}"
            );
        }
    }
    let user = messages
        .iter()
        .find(|m| m.role == mirri_core::message::Role::User)
        .expect("the user message is in the request");
    let placeholder = user
        .content
        .iter()
        .find_map(|part| match part {
            ContentPart::Text { text } if text.contains("does not support image input") => {
                Some(text.as_str())
            }
            _ => None,
        })
        .expect("the image is replaced with an omission placeholder");
    assert!(
        placeholder.contains("/tmp/sessions/s1/blobs/ab12"),
        "the placeholder carries the archive path: {placeholder}"
    );
    assert!(
        user.content.iter().any(
            |part| matches!(part, ContentPart::Text { text } if text == "what is in this picture?")
        ),
        "the user's text is preserved"
    );
}

/// Seed a memory with one user message carrying an archived image part.
fn memory_with_image_message() -> Arc<Mutex<ContextMemory>> {
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    memory
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .append_user_message(
            vec![
                ContentPart::Text {
                    text: "what is in this picture?".to_string(),
                },
                ContentPart::ImageUrl {
                    image_url: ImageUrlData {
                        url: "data:image/png;base64,AAAA".to_string(),
                        detail: None,
                        id: Some("/tmp/sessions/s1/blobs/ab12".to_string()),
                    },
                    detail: None,
                },
            ],
            PromptOrigin::User,
        );
    memory
}

/// Extract the loop-event sub-type of a collected wire op, if it is one.
fn loop_event_type(op: &mirri_proto::WireOp) -> Option<String> {
    match op {
        mirri_proto::WireOp::ContextAppendLoopEvent { event } => event
            .get("type")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        _ => None,
    }
}

#[tokio::test]
async fn should_abort_with_interrupted_anchor_when_stream_cancelled_mid_flight() {
    // Given an LLM whose first stream is cancelled mid-flight after
    // delivering partial text
    let cancel = CancelToken::new();
    let llm = Arc::new(
        MockLlm::new(vec![
            MockResponse::interrupted_after("Partial answer: the first half"),
            MockResponse::end_turn("Second turn answer"),
        ])
        .with_cancel_on_chat(cancel.clone()),
    );
    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let builders = memory_backed_builders(memory.clone());

    // When the turn runs, it aborts (cancelled mid-stream)
    let result = run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(Arc::clone(&llm) as Arc<dyn Llm>),
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
    .expect("turn should return, not error");

    assert_eq!(result.stop_reason, TurnStopReason::Aborted);
    assert_eq!(result.steps, 1);

    // Then the memory holds the delivered prefix as a SETTLED assistant
    // message with the interrupted anchor — not a dropped partial.
    {
        let mem = memory.lock().unwrap_or_else(|e| e.into_inner());
        let assistant = mem
            .history()
            .iter()
            .rev()
            .find(|m| m.message.role == mirri_core::message::Role::Assistant)
            .expect("an assistant message must survive the abort")
            .clone();
        assert!(
            !assistant.message.partial,
            "the interrupted step must be settled, not left partial"
        );
        let text = assistant.message.text_content();
        assert!(
            text.contains("Partial answer: the first half"),
            "the delivered prefix must be kept, got: {text:?}"
        );
        assert!(
            text.contains("[Request interrupted]"),
            "the interrupted anchor must be appended, got: {text:?}"
        );
        // No deferred stranding and no open tool exchange left behind.
        assert!(!mem.has_open_tool_exchange());
    }

    // And the NEXT request (fresh turn, same memory) contains the delivered
    // prefix — the model sees what the user saw.
    let cancel2 = CancelToken::new();
    run_turn(
        RunTurnConfig::new("turn-2"),
        &FixedLlmResolver::new(Arc::clone(&llm) as Arc<dyn Llm>),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel2,
        &builders,
        None,
        None,
        None,
    )
    .await
    .expect("second turn should succeed");

    let requests = llm.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2, "two LLM requests must have been made");
    let second_request = &requests[1];
    let assistant_texts: Vec<String> = second_request
        .iter()
        .filter(|m| m.role == mirri_core::message::Role::Assistant)
        .map(|m| m.text_content())
        .collect();
    assert!(
        assistant_texts
            .iter()
            .any(|t| t.contains("Partial answer: the first half")),
        "the second request must include the delivered prefix, got: {assistant_texts:?}"
    );
    assert!(
        assistant_texts
            .iter()
            .any(|t| t.contains("[Request interrupted]")),
        "the second request must include the interrupted anchor, got: {assistant_texts:?}"
    );
}

#[tokio::test]
async fn should_record_interrupted_closure_events_to_wire_when_recorder_installed() {
    // Given an LLM whose stream is cancelled mid-flight, and a wire recorder
    let cancel = CancelToken::new();
    let llm = MockLlm::new(vec![MockResponse::interrupted_after("Halfway")])
        .with_cancel_on_chat(cancel.clone());
    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let builders = memory_backed_builders(memory.clone());
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let recorder = CollectingWireRecorder(Arc::clone(&recorded));

    // When the turn aborts
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
        Some(&recorder as &dyn WireOpRecorder),
    )
    .await
    .expect("turn should return, not error");
    assert_eq!(result.stop_reason, TurnStopReason::Aborted);

    // Then the wire log carries the interrupted closure: the delivered
    // content part, the anchor content part, and a step.end — replay of the
    // log rebuilds the identical closed state.
    let ops = recorded.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let types: Vec<String> = ops.iter().filter_map(loop_event_type).collect();
    assert!(
        types.iter().any(|t| t == "step.begin"),
        "step.begin must be recorded, got {types:?}"
    );
    assert!(
        types.iter().filter(|t| t == &"content.part").count() >= 2,
        "delivered part + anchor part must be recorded, got {types:?}"
    );
    assert!(
        types.iter().any(|t| t == "step.end"),
        "the closure step.end must be recorded, got {types:?}"
    );
    // The anchor is on the wire as a content part.
    let has_anchor = ops.iter().any(|op| {
        if let mirri_proto::WireOp::ContextAppendLoopEvent { event } = op {
            event
                .get("part")
                .and_then(|p| p.get("text"))
                .and_then(serde_json::Value::as_str)
                .is_some_and(|t| t.contains("[Request interrupted]"))
        } else {
            false
        }
    });
    assert!(has_anchor, "the interrupted anchor must be on the wire");
}

/// A resolver that serves one instance for the first N resolves, then
/// switches — the deterministic mid-turn model switch.
struct SwitchingResolver {
    first: Arc<MockLlm>,
    second: Arc<MockLlm>,
    calls: std::sync::atomic::AtomicUsize,
    switch_at: usize,
}

impl LlmResolver for SwitchingResolver {
    fn resolve(&self) -> Result<Arc<dyn Llm>, String> {
        let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(if n < self.switch_at {
            self.first.clone()
        } else {
            self.second.clone()
        })
    }
}

#[tokio::test]
async fn should_use_the_switched_llm_for_the_next_step_when_the_resolver_changes_mid_turn() {
    // Given a turn whose first LLM answers with a tool call and whose
    // resolver switches to a second LLM after that step
    let first = Arc::new(MockLlm::new(vec![MockResponse::tool_use(vec![
        make_tool_call("call-1", "Echo", serde_json::json!({"message": "hi"})),
    ])]));
    let second = Arc::new(MockLlm::new(vec![MockResponse::end_turn("done by second")]));
    let resolver = SwitchingResolver {
        first: Arc::clone(&first),
        second: Arc::clone(&second),
        calls: std::sync::atomic::AtomicUsize::new(0),
        switch_at: 1,
    };
    let hooks = TestHooks::default();
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    // When the turn runs with the echo tool registered
    let result = run_turn(
        RunTurnConfig::new("turn-1"),
        &resolver,
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

    // Then the turn ran two steps and the second LLM served the second
    // step (the switch took effect on the next LLM call, not the next turn)
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    assert_eq!(result.steps, 2);
    assert!(
        second.served_calls() >= 1,
        "the switched-to LLM must serve the post-switch step"
    );
    assert_eq!(
        first.served_calls(),
        1,
        "the pre-switch LLM serves exactly the first step"
    );
}

#[tokio::test]
async fn should_record_request_header_changed_when_media_fallback_switches_projection() {
    // Given an LLM whose first response is rejected as too large, and a
    // media-degraded fallback builder, with a wire recorder installed
    let llm = MockLlm::new(vec![
        MockResponse::error("413 Payload Too Large"),
        MockResponse::end_turn("Success with degraded"),
    ]);
    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = MessageBuilders {
        normal: Box::new(|| Ok(vec![Message::user("Hello")])),
        media_degraded: Some(Box::new(|| Ok(vec![Message::user("Hello degraded")]))),
        media_stripped: None,
        strict: None,
        format_rejected_flag: Arc::new(AtomicBool::new(false)),
    };
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let recorder = CollectingWireRecorder(Arc::clone(&recorded));

    // When the turn runs and the fallback projection kicks in
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
        Some(&recorder as &dyn WireOpRecorder),
    )
    .await
    .expect("turn should succeed with the fallback");
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);

    // Then the wire log carries the explicit request-envelope drift marker,
    // and a reducer replay of the log can audit it after resume.
    let ops = recorded.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let mut state = mirri_proto::AgentState::new();
    let mut header_events = 0;
    for op in &ops {
        let (_, events) = mirri_proto::reducer::reduce(&state, op);
        if matches!(op, mirri_proto::WireOp::RequestHeaderChanged { .. }) {
            header_events += 1;
        }
        for event in events {
            if let mirri_proto::DomainEvent::RequestHeaderChanged { reason } = event {
                assert_eq!(reason, "media_projection_degraded");
            }
        }
        let (next, _) = mirri_proto::reducer::reduce(&state, op);
        state = next;
    }
    assert_eq!(
        header_events, 1,
        "exactly one request.header.changed op must be recorded"
    );
    let header = state
        .request_header
        .expect("the reducer must fold the request header state");
    assert_eq!(header.change_count, 1);
    assert_eq!(header.last_reason, "media_projection_degraded");
}
