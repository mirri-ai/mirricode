//! Integration tests for the `turn_flow` module.
//!
//! Tests cover:
//! - `SteerBuffer`: push, flush, is_empty, flush returns true/false
//! - `ToolCallDeduplicator`: same-step dedup, cross-step streak, begin/end step
//! - `make_key` / `sort_json_value`: canonical key generation
//! - `TurnFlow`: basic turn, steering, hooks, should_continue_after_stop
//! - Overflow recovery (compaction retry)

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use futures::stream;
use mirri_proto::WireOp;
use serde_json::Value;

use mirri_core::TurnStopReason;
use mirri_core::WireOpRecorder;
use mirri_core::cancel::CancelToken;
use mirri_core::compaction::{CompactionStrategy, FullCompaction};
use mirri_core::context_memory::ContextMemory;
use mirri_core::error::{EngineError, EngineResult};
use mirri_core::goal::{GOAL_BUDGET_STOP_REMINDER, GoalBudget, GoalManager, GoalStatus};
use mirri_core::hook_engine::{HookDef, HookEngine, HookEventType};
use mirri_core::hooks::{
    AuthorizeToolContext, AuthorizeToolResult, ContinueAfterStopContext, HookFuture, LoopHooks,
    PrepareToolContext, PrepareToolResult,
};
use mirri_core::llm::*;
use mirri_core::message::ContentPart;
use mirri_core::origin::PromptOrigin;
use mirri_core::permission::{
    PermissionManager, PermissionMode, PermissionRule, PermissionServices, RuleDecision, RuleScope,
};
use mirri_core::tool::*;
use mirri_core::tool_registry::{PrepareResult, ToolManager, ValidatorCache, prepare_tool_call};
use mirri_core::turn_flow::*;

// ---------------------------------------------------------------------------
// Mock LLM (reused from loop_engine_tests pattern)
// ---------------------------------------------------------------------------

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

    fn with_usage(mut self, input: u64, output: u64) -> Self {
        self.usage = LlmUsage {
            input_tokens: input,
            output_tokens: output,
            ..Default::default()
        };
        self
    }

    /// Set the full four-bucket usage (input, output, cache read, cache
    /// creation) — used by the three-bucket carry-through tests.
    fn with_full_usage(
        mut self,
        input: u64,
        output: u64,
        cache_read: u64,
        cache_creation: u64,
    ) -> Self {
        self.usage = LlmUsage {
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: cache_read,
            cache_creation_tokens: cache_creation,
        };
        self
    }
}

struct MockLlm {
    system_prompt: String,
    model_name: String,
    responses: Arc<Mutex<Vec<MockResponse>>>,
    retryable: bool,
    /// Number of `chat()` calls (for summarizer-call assertions).
    chat_calls: Arc<Mutex<u64>>,
}

impl MockLlm {
    fn new(responses: Vec<MockResponse>) -> Self {
        Self {
            system_prompt: "You are a test assistant.".to_string(),
            model_name: "test-model".to_string(),
            responses: Arc::new(Mutex::new(responses)),
            retryable: false,
            chat_calls: Arc::new(Mutex::new(0)),
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
        *self.chat_calls.lock().unwrap() += 1;
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

/// A scripted LLM that flips the shared usage-model key handle to a new
/// alias on its second `chat()` call — the same side-map write the session
/// handlers perform at a mid-turn model switch, landing BEFORE the next
/// step's usage accounting and `after_step` read (so the post-switch steps
/// re-attribute, spec 11 A13).
struct ModelSwitchingLlm {
    responses: Mutex<Vec<MockResponse>>,
    usage_model_key: Arc<Mutex<Option<String>>>,
    chat_calls: Mutex<u32>,
}

impl Llm for ModelSwitchingLlm {
    fn system_prompt(&self) -> &str {
        "You are a test assistant."
    }

    fn model_name(&self) -> &str {
        "resolved-upstream-model"
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
        false
    }

    fn chat(&self, _params: LlmChatParams) -> EngineResult<LlmEventStream> {
        let mut calls = self.chat_calls.lock().unwrap();
        *calls += 1;
        if *calls == 2 {
            *self.usage_model_key.lock().unwrap() = Some("alias-b".to_string());
        }
        drop(calls);
        let mut queue = self.responses.lock().unwrap();
        if queue.is_empty() {
            return Err(EngineError::ProviderError(
                "No more mock responses".to_string(),
            ));
        }
        let resp = queue.remove(0);
        drop(queue);
        Ok(Box::pin(stream::iter(build_events(&resp))))
    }
}

// ---------------------------------------------------------------------------
// Mock Tool
// ---------------------------------------------------------------------------

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

fn make_tool_call(id: &str, name: &str, args: Value) -> LlmToolCall {
    LlmToolCall {
        id: id.to_string(),
        name: name.to_string(),
        args,
    }
}

fn make_tool_manager_with_echo() -> ToolManager {
    let mut tm = ToolManager::new();
    tm.set_active_tools(vec!["Echo".to_string()]);
    tm.register_builtin(Arc::new(EchoTool));
    tm
}

// ---------------------------------------------------------------------------
// SteerBuffer tests
// ---------------------------------------------------------------------------

#[test]
fn should_track_items_when_steers_pushed_to_buffer() {
    let mut buf = SteerBuffer::new();
    assert!(buf.is_empty());
    assert_eq!(buf.len(), 0);

    buf.push(
        vec![ContentPart::Text {
            text: "steer".to_string(),
        }],
        PromptOrigin::User,
    );
    assert!(!buf.is_empty());
    assert_eq!(buf.len(), 1);
}

#[test]
fn should_return_false_when_flushing_empty_buffer() {
    let mut buf = SteerBuffer::new();
    let mut memory = ContextMemory::new();
    assert!(!buf.flush(&mut memory));
}

#[test]
fn should_return_true_and_clear_when_flushing_non_empty_buffer() {
    let mut buf = SteerBuffer::new();
    buf.push(
        vec![ContentPart::Text {
            text: "steer".to_string(),
        }],
        PromptOrigin::User,
    );
    let mut memory = ContextMemory::new();
    assert!(buf.flush(&mut memory));
    assert!(buf.is_empty());
}

#[test]
fn should_add_all_steers_to_memory_when_flushing_multiple_steers() {
    let mut buf = SteerBuffer::new();
    buf.push(
        vec![ContentPart::Text {
            text: "first".to_string(),
        }],
        PromptOrigin::User,
    );
    buf.push(
        vec![ContentPart::Text {
            text: "second".to_string(),
        }],
        PromptOrigin::User,
    );
    let mut memory = ContextMemory::new();
    assert!(buf.flush(&mut memory));
    assert_eq!(buf.len(), 0);

    let user_count = memory
        .history()
        .iter()
        .filter(|m| m.message.role == mirri_core::message::Role::User)
        .count();
    assert_eq!(user_count, 2);
}

// ---------------------------------------------------------------------------
// ToolCallDeduplicator tests
// ---------------------------------------------------------------------------

#[test]
fn should_return_none_when_first_call_in_same_step() {
    let mut dedup = ToolCallDeduplicator::new();
    dedup.begin_step();
    let result = dedup.check_same_step("Echo", &serde_json::json!({"x": 1}), "call-1");
    assert!(result.is_none());
}

#[test]
fn should_return_synthetic_result_when_duplicate_call_in_same_step() {
    let mut dedup = ToolCallDeduplicator::new();
    dedup.begin_step();
    dedup.check_same_step("Echo", &serde_json::json!({"x": 1}), "call-1");
    let result = dedup.check_same_step("Echo", &serde_json::json!({"x": 1}), "call-2");
    assert!(result.is_some());
    let result = result.unwrap();
    assert!(!result.is_error);
}

#[test]
fn should_return_none_when_args_differ_in_same_step() {
    let mut dedup = ToolCallDeduplicator::new();
    dedup.begin_step();
    dedup.check_same_step("Echo", &serde_json::json!({"x": 1}), "call-1");
    let result = dedup.check_same_step("Echo", &serde_json::json!({"x": 2}), "call-2");
    assert!(result.is_none());
}

#[test]
fn should_add_first_reminder_when_cross_step_streak_reaches_3() {
    let mut dedup = ToolCallDeduplicator::new();
    let args = serde_json::json!({"x": 1});

    for step in 0..3 {
        dedup.begin_step();
        dedup.check_same_step("Echo", &args, &format!("call-{step}-1"));
        let mut result = ToolResult::success("ok");
        dedup.finalize_result(&format!("call-{step}-1"), &mut result);
        dedup.end_step();

        if step == 2 {
            let text = match &result.output {
                ToolOutput::Text(t) => t.clone(),
                _ => panic!("expected text"),
            };
            assert!(
                text.contains("system-reminder"),
                "expected reminder at streak 3"
            );
            assert!(!result.stop_turn);
        }
    }
    assert_eq!(dedup.consecutive_count(), 3);
}

#[test]
fn should_add_second_reminder_when_cross_step_streak_reaches_5() {
    let mut dedup = ToolCallDeduplicator::new();
    let args = serde_json::json!({"x": 1});

    for step in 0..5 {
        dedup.begin_step();
        dedup.check_same_step("Echo", &args, &format!("call-{step}-1"));
        let mut result = ToolResult::success("ok");
        dedup.finalize_result(&format!("call-{step}-1"), &mut result);
        dedup.end_step();

        if step == 4 {
            let text = match &result.output {
                ToolOutput::Text(t) => t.clone(),
                _ => panic!("expected text"),
            };
            assert!(text.contains("5 times"), "expected reminder 2 at streak 5");
            assert!(!result.stop_turn);
        }
    }
    assert_eq!(dedup.consecutive_count(), 5);
}

#[test]
fn should_add_third_reminder_when_cross_step_streak_reaches_8() {
    let mut dedup = ToolCallDeduplicator::new();
    let args = serde_json::json!({"x": 1});

    for step in 0..8 {
        dedup.begin_step();
        dedup.check_same_step("Echo", &args, &format!("call-{step}-1"));
        let mut result = ToolResult::success("ok");
        dedup.finalize_result(&format!("call-{step}-1"), &mut result);
        dedup.end_step();

        if step == 7 {
            let text = match &result.output {
                ToolOutput::Text(t) => t.clone(),
                _ => panic!("expected text"),
            };
            assert!(
                text.contains("repeatedly calling"),
                "expected reminder 3 at streak 8"
            );
            assert!(!result.stop_turn);
        }
    }
    assert_eq!(dedup.consecutive_count(), 8);
}

#[test]
fn should_force_stop_when_cross_step_streak_reaches_12() {
    let mut dedup = ToolCallDeduplicator::new();
    let args = serde_json::json!({"x": 1});

    for step in 0..12 {
        dedup.begin_step();
        dedup.check_same_step("Echo", &args, &format!("call-{step}-1"));
        let mut result = ToolResult::success("ok");
        dedup.finalize_result(&format!("call-{step}-1"), &mut result);
        dedup.end_step();

        if step == 11 {
            assert!(result.stop_turn, "should stop at streak 12");
        }
    }
    assert_eq!(dedup.consecutive_count(), 12);
}

#[test]
fn should_clear_per_step_dedup_state_when_begin_step_called() {
    let mut dedup = ToolCallDeduplicator::new();
    dedup.begin_step();
    dedup.check_same_step("Echo", &serde_json::json!({"x": 1}), "call-1");

    dedup.begin_step();
    // Same call after begin_step should NOT be deduped.
    let result = dedup.check_same_step("Echo", &serde_json::json!({"x": 1}), "call-2");
    assert!(result.is_none());
}

#[test]
fn should_carry_streak_count_forward_when_end_step_called() {
    let mut dedup = ToolCallDeduplicator::new();
    let args = serde_json::json!({"x": 1});

    // Step 1
    dedup.begin_step();
    dedup.check_same_step("Echo", &args, "call-0-1");
    let mut result = ToolResult::success("ok");
    dedup.finalize_result("call-0-1", &mut result);
    dedup.end_step();
    assert_eq!(dedup.consecutive_count(), 1);

    // Step 2 — streak continues
    dedup.begin_step();
    dedup.check_same_step("Echo", &args, "call-1-1");
    let mut result = ToolResult::success("ok");
    dedup.finalize_result("call-1-1", &mut result);
    dedup.end_step();
    assert_eq!(dedup.consecutive_count(), 2);
}

// ---------------------------------------------------------------------------
// make_key / sort_json_value tests (tested via dedup behavior)
// ---------------------------------------------------------------------------

#[test]
fn should_produce_same_key_when_json_key_order_differs() {
    // If make_key canonicalizes key order, these should produce the same
    // dedup result.
    let mut dedup = ToolCallDeduplicator::new();
    dedup.begin_step();

    // First call with {"b": 1, "a": 2}
    let result1 = dedup.check_same_step("Echo", &serde_json::json!({"b": 1, "a": 2}), "call-1");
    assert!(result1.is_none());

    // Second call with {"a": 2, "b": 1} — should be deduped
    let result2 = dedup.check_same_step("Echo", &serde_json::json!({"a": 2, "b": 1}), "call-2");
    assert!(
        result2.is_some(),
        "different key order should produce same canonical key"
    );
}

// ---------------------------------------------------------------------------
// TurnFlow basic tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_complete_with_end_turn_when_basic_turn_runs() {
    let llm = MockLlm::new(vec![MockResponse::end_turn("Hello!")]);
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());

    let result = turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Hello".to_string(),
            }],
            PromptOrigin::User,
            memory,
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(llm))),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("turn should succeed");

    assert_eq!(result.turn_id, 1);
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    assert_eq!(result.steps, 1);
    assert!(!turn_flow.is_turn_active());
}

#[tokio::test]
async fn should_publish_the_auto_compaction_lifecycle_when_a_turns_context_exceeds_the_threshold() {
    // Given a flow whose compaction threshold trips on the first user
    // message, wired to a session event bus with a subscriber
    let compaction_strategy = CompactionStrategy {
        max_context_tokens: 30,
        trigger_ratio: 0.5,
        ..CompactionStrategy::default()
    };
    let mut turn_flow = TurnFlow::new(TurnFlowConfig {
        compaction_strategy,
        ..TurnFlowConfig::default()
    });
    let event_bus = Arc::new(mirri_core::session::events::EventBus::new(64));
    turn_flow.set_event_bus(Arc::clone(&event_bus));
    let mut events = event_bus.subscribe();

    // And an LLM that answers the summarizer first, then ends the turn
    let llm = MockLlm::new(vec![
        MockResponse::end_turn("The conversation asked for a long story."),
        MockResponse::end_turn("Done!"),
    ]);
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    // A prior exchange in memory so a legal retention-tail cut exists when
    // begin runs (a single-message history is one indivisible unit — the
    // round would give up, US2.6).
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    {
        let mut mem = memory.lock().unwrap();
        mem.append_user_message(
            vec![ContentPart::Text {
                text: "previous question".to_string(),
            }],
            PromptOrigin::User,
        );
        mem.append_message(mirri_core::context::ContextMessage {
            message: mirri_core::message::Message::assistant("previous answer"),
            origin: None,
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        });
    }
    let cancel = CancelToken::new();

    // When a turn runs with a user message big enough to trip the threshold
    let result = turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "x".repeat(200),
            }],
            PromptOrigin::User,
            memory,
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(llm))),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("turn should succeed");
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);

    // Then the step's start bracket precedes the auto-compaction
    // lifecycle (the step hook publishes before the compaction check
    // runs): started (auto, no instruction), blocked (the step waits on
    // the summarizer), completed (the applied result with its computed
    // token counts)
    match events.recv().await.expect("step started must arrive") {
        mirri_core::SessionEvent::TurnStepStarted { turn_id: _, step } => {
            assert_eq!(step, 1);
        }
        other => panic!("expected TurnStepStarted, got {other:?}"),
    }
    match events.recv().await.expect("started must arrive") {
        mirri_core::SessionEvent::CompactionStarted {
            agent_id,
            trigger,
            instruction,
        } => {
            assert_eq!(agent_id, "main");
            assert_eq!(trigger, mirri_core::compaction::CompactionSource::Auto);
            assert_eq!(instruction, None);
        }
        other => panic!("expected CompactionStarted, got {other:?}"),
    }
    match events.recv().await.expect("blocked must arrive") {
        mirri_core::SessionEvent::CompactionBlocked { agent_id, turn_id } => {
            assert_eq!(agent_id, "main");
            assert_eq!(turn_id, None);
        }
        other => panic!("expected CompactionBlocked, got {other:?}"),
    }
    match events.recv().await.expect("completed must arrive") {
        mirri_core::SessionEvent::CompactionCompleted { agent_id, result } => {
            assert_eq!(agent_id, "main");
            assert_eq!(result.summary, "The conversation asked for a long story.");
            // The old segment (the prior exchange) is what got compacted —
            // the new user message rides in the retention tail.
            assert_eq!(result.compacted_count, 2);
            // tokens_after is the post-apply memory count the orchestrator
            // computed — recorded, not assumed smaller (with an artificial
            // tiny threshold a summary can legitimately exceed the
            // original).
            assert!(result.tokens_before > 0);
        }
        other => panic!("expected CompactionCompleted, got {other:?}"),
    }
    // And nothing else was published after the lifecycle except the
    // step's completion bracket
    match events.recv().await.expect("step completed must arrive") {
        mirri_core::SessionEvent::TurnStepCompleted {
            turn_id: _, step, ..
        } => {
            assert_eq!(step, 1);
        }
        other => panic!("expected TurnStepCompleted, got {other:?}"),
    }
    assert!(
        events.try_recv().is_err(),
        "the compaction lifecycle plus the step brackets must be the whole stream"
    );
}

#[tokio::test]
async fn should_attribute_auto_compaction_events_to_the_flow_agent_when_threshold_is_crossed() {
    // Given the threshold-crossing scaffolding above, with the flow running
    // AS an agent (the identity the subagent host / the embedded main path
    // pin via set_agent_id)
    let compaction_strategy = CompactionStrategy {
        max_context_tokens: 30,
        trigger_ratio: 0.5,
        ..CompactionStrategy::default()
    };
    let mut turn_flow = TurnFlow::new(TurnFlowConfig {
        compaction_strategy,
        ..TurnFlowConfig::default()
    });
    let event_bus = Arc::new(mirri_core::session::events::EventBus::new(64));
    turn_flow.set_event_bus(Arc::clone(&event_bus));
    turn_flow.set_agent_id(Some("agent-0192-zzzz".to_string()));
    let mut events = event_bus.subscribe();

    let llm = MockLlm::new(vec![
        MockResponse::end_turn("The conversation asked for a long story."),
        MockResponse::end_turn("Done!"),
    ]);
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    // A prior exchange in memory so a legal retention-tail cut exists when
    // begin runs (a single-message history is one indivisible unit — the
    // round would give up, US2.6).
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    {
        let mut mem = memory.lock().unwrap();
        mem.append_user_message(
            vec![ContentPart::Text {
                text: "previous question".to_string(),
            }],
            PromptOrigin::User,
        );
        mem.append_message(mirri_core::context::ContextMessage {
            message: mirri_core::message::Message::assistant("previous answer"),
            origin: None,
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        });
    }
    let cancel = CancelToken::new();

    // When the turn trips the threshold
    let result = turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "x".repeat(200),
            }],
            PromptOrigin::User,
            memory,
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(llm))),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("turn should succeed");
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);

    // Then the auto-compaction lifecycle carries the flow's agent id —
    // the hooks-path emit sites read the config the flow cloned its
    // identity into, never a hardcoded main.
    match events.recv().await.expect("step started must arrive") {
        mirri_core::SessionEvent::TurnStepStarted { .. } => {}
        other => panic!("expected TurnStepStarted, got {other:?}"),
    }
    match events.recv().await.expect("started must arrive") {
        mirri_core::SessionEvent::CompactionStarted {
            agent_id,
            trigger: _,
            instruction: _,
        } => {
            assert_eq!(agent_id, "agent-0192-zzzz");
        }
        other => panic!("expected CompactionStarted, got {other:?}"),
    }
    match events.recv().await.expect("blocked must arrive") {
        mirri_core::SessionEvent::CompactionBlocked { agent_id, turn_id } => {
            assert_eq!(agent_id, "agent-0192-zzzz");
            assert_eq!(turn_id, None);
        }
        other => panic!("expected CompactionBlocked, got {other:?}"),
    }
    match events.recv().await.expect("completed must arrive") {
        mirri_core::SessionEvent::CompactionCompleted { agent_id, result } => {
            assert_eq!(agent_id, "agent-0192-zzzz");
            assert_eq!(result.summary, "The conversation asked for a long story.");
        }
        other => panic!("expected CompactionCompleted, got {other:?}"),
    }
}

/// Drive one turn-level overflow recovery: the first auto-compaction
/// round's summarizer overflow-errors four times in a row (exhausting the
/// round's L2 shrink ladder, `ContextOverflow { attempts: 4 }`), which
/// propagates out of `before_step` and makes `run_one_turn_inner` run its
/// overflow-recovery compaction — the flow-internal (mod.rs) emit sites —
/// before retrying the turn with the compacted context. Returns every
/// event the bus saw, with the flow's agent identity set to `agent_id`
/// beforehand (or left unset for `None`).
async fn run_overflow_recovery_turn(agent_id: Option<String>) -> Vec<mirri_core::SessionEvent> {
    // A tiny threshold so the first before_step fires auto compaction.
    let compaction_strategy = CompactionStrategy {
        max_context_tokens: 30,
        trigger_ratio: 0.5,
        ..CompactionStrategy::default()
    };
    let mut turn_flow = TurnFlow::new(TurnFlowConfig {
        compaction_strategy,
        ..TurnFlowConfig::default()
    });
    let event_bus = Arc::new(mirri_core::session::events::EventBus::new(64));
    turn_flow.set_event_bus(Arc::clone(&event_bus));
    if let Some(id) = agent_id {
        turn_flow.set_agent_id(Some(id));
    }
    let mut events = event_bus.subscribe();

    // The summarizer overflows until the ladder gives up (4 consecutive
    // overflow errors — see compaction_tests
    // should_stop_shrinking_when_level_two_budget_reached), then the
    // recovery round succeeds, then the retried turn's step ends it.
    let overflow = "context length exceeded maximum context window";
    let llm = MockLlm::new(vec![
        MockResponse::error(overflow),
        MockResponse::error(overflow),
        MockResponse::error(overflow),
        MockResponse::error(overflow),
        MockResponse::end_turn("Recovered summary."),
        MockResponse::end_turn("Done!"),
    ]);
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    // Several prior exchanges so the ladder has room to shrink (the round
    // re-cuts a smaller old segment on each rung) and the retention tail
    // has a legal cut.
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    {
        let mut mem = memory.lock().unwrap();
        for i in 0..8 {
            mem.append_user_message(
                vec![ContentPart::Text {
                    text: format!("question number {:02} padded for tokens", i),
                }],
                PromptOrigin::User,
            );
            mem.append_message(mirri_core::context::ContextMessage {
                message: mirri_core::message::Message::assistant(format!(
                    "answer number {:02} padded for tokens",
                    i
                )),
                origin: None,
                is_error: false,
                note: None,
                with_skills: None,
                entry_seq: 0,
            });
        }
    }
    let cancel = CancelToken::new();

    let result = turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "x".repeat(200),
            }],
            PromptOrigin::User,
            memory,
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(llm))),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("turn should succeed after overflow recovery");
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);

    let mut collected = Vec::new();
    while let Ok(event) = events.try_recv() {
        collected.push(event);
    }
    collected
}

#[tokio::test]
async fn should_default_flow_agent_attribution_to_main_when_unset() {
    // When a flow that never had set_agent_id called runs an overflow
    // recovery (the flow-internal emit sites)
    let events = run_overflow_recovery_turn(None).await;

    // Then the recovery lifecycle published: two starts and two blocks
    // (the failed threshold attempt from the hooks, then the recovery round
    // from the flow itself) and one completion (the recovery's apply).
    let mut started = Vec::new();
    let mut blocked = Vec::new();
    let mut completed = Vec::new();
    for event in events {
        match event {
            mirri_core::SessionEvent::CompactionStarted { agent_id, .. } => started.push(agent_id),
            mirri_core::SessionEvent::CompactionBlocked { agent_id, .. } => blocked.push(agent_id),
            mirri_core::SessionEvent::CompactionCompleted { agent_id, .. } => {
                completed.push(agent_id)
            }
            _ => {}
        }
    }
    assert_eq!(
        started,
        vec!["main".to_string(), "main".to_string()],
        "both the threshold attempt and the overflow recovery attribute to main"
    );
    assert_eq!(
        blocked,
        vec!["main".to_string(), "main".to_string()],
        "both block announcements attribute to main"
    );
    assert_eq!(
        completed,
        vec!["main".to_string()],
        "the recovery completion attributes to main"
    );
}

#[tokio::test]
async fn should_attribute_overflow_recovery_compaction_events_to_the_flow_agent_when_one_is_set() {
    // When a flow running AS an agent hits the same overflow recovery
    let events = run_overflow_recovery_turn(Some("agent-0192-zzzz".to_string())).await;

    // Then every compaction event — the hooks-path threshold attempt AND
    // the flow-internal overflow recovery — carries the agent id.
    for event in events {
        match event {
            mirri_core::SessionEvent::CompactionStarted { agent_id, .. }
            | mirri_core::SessionEvent::CompactionBlocked { agent_id, .. }
            | mirri_core::SessionEvent::CompactionCompleted { agent_id, .. } => {
                assert_eq!(agent_id, "agent-0192-zzzz");
            }
            _ => {}
        }
    }
}

#[tokio::test]
async fn should_stay_silent_on_the_bus_when_no_auto_compaction_triggers() {
    // Given a flow with the default strategy (max_context_tokens unknown —
    // no ratio-based triggering), wired to a session event bus
    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());
    let event_bus = Arc::new(mirri_core::session::events::EventBus::new(64));
    turn_flow.set_event_bus(Arc::clone(&event_bus));
    let mut events = event_bus.subscribe();

    let llm = MockLlm::new(vec![MockResponse::end_turn("Done!")]);
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    // When a turn runs below any compaction threshold
    let result = turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Hello".to_string(),
            }],
            PromptOrigin::User,
            memory,
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(llm))),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("turn should succeed");
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);

    // Then no compaction event was published — the turn's step brackets
    // (turn.step.started/completed) are expected bus traffic and are
    // drained explicitly.
    while let Ok(event) = events.try_recv() {
        assert!(
            !matches!(
                event,
                mirri_core::SessionEvent::CompactionStarted { .. }
                    | mirri_core::SessionEvent::CompactionBlocked { .. }
                    | mirri_core::SessionEvent::CompactionCompleted { .. }
                    | mirri_core::SessionEvent::CompactionCancelled { .. }
            ),
            "a turn without compaction must not publish compaction events"
        );
    }
}

#[tokio::test]
async fn should_publish_step_events_under_the_wire_turn_id_when_one_is_set() {
    // Given a flow wired to a session bus, with the wire turn id the
    // message-send dispatch minted plumbed down
    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());
    let event_bus = Arc::new(mirri_core::session::events::EventBus::new(64));
    turn_flow.set_event_bus(Arc::clone(&event_bus));
    turn_flow.set_wire_turn_id(Some("turn-abc-123".to_string()));
    let mut events = event_bus.subscribe();

    let llm = MockLlm::new(vec![MockResponse::end_turn("Done!")]);
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    // When the turn runs
    turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Hello".to_string(),
            }],
            PromptOrigin::User,
            memory,
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(llm))),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("turn should succeed");

    // Then the step brackets publish under the wire turn id — the same
    // key `turn.started` / `turn.ended` carry (not the engine's numeric
    // per-flow counter)
    match events.recv().await.expect("step started must arrive") {
        mirri_core::SessionEvent::TurnStepStarted { turn_id, step } => {
            assert_eq!(turn_id, "turn-abc-123");
            assert_eq!(step, 1);
        }
        other => panic!("expected TurnStepStarted, got {other:?}"),
    }
    match events.recv().await.expect("step completed must arrive") {
        mirri_core::SessionEvent::TurnStepCompleted { turn_id, .. } => {
            assert_eq!(turn_id, "turn-abc-123");
        }
        other => panic!("expected TurnStepCompleted, got {other:?}"),
    }
    assert!(events.try_recv().is_err(), "no other bus traffic expected");

    // And the wire id does not leak into the flow's next turn — a reused
    // flow without a fresh dispatch falls back to its numeric counter,
    // never silently inherits the previous turn's id.
    turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Again".to_string(),
            }],
            PromptOrigin::User,
            Arc::new(Mutex::new(ContextMemory::new())),
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(MockLlm::new(vec![
                MockResponse::end_turn("Done!"),
            ])))),
            &CancelToken::new(),
            &mut ValidatorCache::new(),
            None,
            None,
        )
        .await
        .expect("second turn should succeed");
    match events.recv().await.expect("second turn's step started") {
        mirri_core::SessionEvent::TurnStepStarted { turn_id, .. } => {
            assert_ne!(turn_id, "turn-abc-123", "the wire id must not leak");
        }
        other => panic!("expected TurnStepStarted, got {other:?}"),
    }
}

#[tokio::test]
async fn should_carry_the_usage_models_alias_on_turn_step_completed_across_a_mid_turn_switch() {
    // Given a flow wired to a session bus, with the usage-model key set
    // to the agent's configured alias (the by-model key the usage sink
    // records under — the same handle `set_usage_model_key` wires)
    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());
    let event_bus = Arc::new(mirri_core::session::events::EventBus::new(64));
    turn_flow.set_event_bus(Arc::clone(&event_bus));
    turn_flow.set_usage_model_key("alias-a");
    let usage_model_key = turn_flow.usage_model_key_handle();
    let mut events = event_bus.subscribe();

    // And an LLM that calls the Echo tool on step 1, ends the turn on
    // step 2, and flips the shared alias handle just before step 2 — the
    // same write the session handlers' side map performs at a mid-turn
    // model switch (before the post-switch step's accounting)
    let llm = ModelSwitchingLlm {
        responses: Mutex::new(vec![
            MockResponse::tool_use(vec![make_tool_call(
                "call-1",
                "Echo",
                serde_json::json!({"message": "hi"}),
            )]),
            MockResponse::end_turn("Done!"),
        ]),
        usage_model_key: Arc::clone(&usage_model_key),
        chat_calls: Mutex::new(0),
    };
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    // When the two-step turn runs
    let result = turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Hello".to_string(),
            }],
            PromptOrigin::User,
            memory,
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(llm))),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("turn should succeed");
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    assert_eq!(result.steps, 2);

    // Then step 1's completion carries the alias its usage recorded
    // under — the same key the usage sink's by-model entry uses
    for (expected_step, expected_model) in [(1, "alias-a"), (2, "alias-b")] {
        match events.recv().await.expect("step started must arrive") {
            mirri_core::SessionEvent::TurnStepStarted { step, .. } => {
                assert_eq!(step, expected_step);
            }
            other => panic!("expected TurnStepStarted, got {other:?}"),
        }
        match events.recv().await.expect("step completed must arrive") {
            mirri_core::SessionEvent::TurnStepCompleted { step, model, .. } => {
                assert_eq!(step, expected_step);
                assert_eq!(model.as_deref(), Some(expected_model));
            }
            other => panic!("expected TurnStepCompleted, got {other:?}"),
        }
    }
    // And the switch is the only bus traffic beyond the step brackets
    assert!(events.try_recv().is_err(), "no other bus traffic expected");
}

#[tokio::test]
async fn should_fall_back_to_the_turn_start_provider_model_when_no_alias_is_wired() {
    // Given a standalone flow (no agent config — the usage-model key
    // handle never got an alias) wired to a session bus
    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());
    let event_bus = Arc::new(mirri_core::session::events::EventBus::new(64));
    turn_flow.set_event_bus(Arc::clone(&event_bus));
    let mut events = event_bus.subscribe();

    let llm = MockLlm::new(vec![MockResponse::end_turn("Done!")]);
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    // When the turn runs
    turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Hello".to_string(),
            }],
            PromptOrigin::User,
            memory,
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(llm))),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("turn should succeed");

    // Then the step's completion carries the turn-start `llm.model_name()`
    // — the same fallback the usage accounting applies for standalone flows
    match events.recv().await.expect("step started must arrive") {
        mirri_core::SessionEvent::TurnStepStarted { .. } => {}
        other => panic!("expected TurnStepStarted, got {other:?}"),
    }
    match events.recv().await.expect("step completed must arrive") {
        mirri_core::SessionEvent::TurnStepCompleted { model, .. } => {
            assert_eq!(model.as_deref(), Some("test-model"));
        }
        other => panic!("expected TurnStepCompleted, got {other:?}"),
    }
    assert!(events.try_recv().is_err(), "no other bus traffic expected");
}

#[tokio::test]
async fn should_continue_for_second_step_when_tool_use_occurs() {
    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![make_tool_call(
            "call-1",
            "Echo",
            serde_json::json!({"message": "hi"}),
        )]),
        MockResponse::end_turn("Done!"),
    ]);
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());

    let result = turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Hello".to_string(),
            }],
            PromptOrigin::User,
            memory,
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(llm))),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("turn should succeed");

    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    assert_eq!(result.steps, 2);
}

#[tokio::test]
async fn should_return_aborted_when_cancel_token_is_set() {
    let llm = MockLlm::new(vec![MockResponse::end_turn("Hello")]);
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    cancel.cancel();

    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());

    let result = turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Hello".to_string(),
            }],
            PromptOrigin::User,
            memory,
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(llm))),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("aborted turn should return Ok with Aborted stop reason");

    assert_eq!(result.stop_reason, TurnStopReason::Aborted);
    assert_eq!(result.steps, 0);
}

#[tokio::test]
async fn should_flush_pending_steers_when_turn_starts() {
    // We can't easily test mid-turn steering in an integration test without
    // a way to inject the steer between steps. But we can test that the
    // steer buffer is flushed at before_step by verifying the turn continues.
    //
    // Strategy: run a turn that ends with EndTurn, then verify the steer
    // buffer is empty (it should have been flushed during before_step).
    let llm = MockLlm::new(vec![MockResponse::end_turn("Done")]);
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());

    // Buffer a steer before the turn starts (simulates a race condition where
    // steer arrives just as the turn is about to start).
    turn_flow.steer(
        vec![ContentPart::Text {
            text: "steered".to_string(),
        }],
        PromptOrigin::User,
    );

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

    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    // The steer should have been flushed during before_step.
    assert!(!turn_flow.has_pending_steers());
}

/// Given a turn flow whose steer buffer parks one background-task
/// notification steer and one user steer, when the flush runs (before_step),
/// then the wire log records the consumption marker followed by a durable
/// `context.append_message` for the notification steer only (the user steer
/// keeps its pre-existing unrecorded replay contract — the durable record
/// is deliberately narrow, SDR-B2-01) — a crash between flush and turn end
/// cannot strand the notification outside the context.
#[tokio::test]
async fn should_record_a_durable_append_when_a_notification_steer_flushes() {
    let llm = MockLlm::new(vec![MockResponse::end_turn("Done")]);
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    let recorded = Arc::new(Mutex::new(Vec::new()));
    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());
    turn_flow.set_wire_recorder(Some(
        Arc::new(CollectingWireRecorder(Arc::clone(&recorded))) as Arc<dyn WireOpRecorder>,
    ));

    // One user steer and one notification steer, parked in FIFO order (the
    // shared handle push — the driver's parking path, which bypasses the
    // turn-long flow mutex).
    {
        let buffer = turn_flow.steer_buffer_handle();
        let mut buf = buffer.lock().unwrap_or_else(|e| e.into_inner());
        buf.push(
            vec![ContentPart::Text {
                text: "user steer".to_string(),
            }],
            PromptOrigin::User,
        );
        buf.push(
            vec![ContentPart::Text {
                text: "<notification id=\"task:t1:completed\"></notification>".to_string(),
            }],
            PromptOrigin::BackgroundTask {
                task_id: "t1".to_string(),
                status: "completed".to_string(),
                notification_id: "task:t1:completed".to_string(),
            },
        );
    }

    turn_flow
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

    // Then the recorder saw the consumption marker for the exact count…
    let ops = recorded.lock().unwrap_or_else(|e| e.into_inner());
    let flush_at = ops
        .iter()
        .position(|op| matches!(op, WireOp::TurnSteerFlushed { count: 2 }))
        .expect("a turn.steer_flushed { count: 2 } marker must be recorded");
    // …followed by exactly one durable append for the notification steer —
    // the user steer stays unrecorded (narrow scope).
    let notification_appends: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(i, op)| {
            i > &flush_at
                && matches!(op, WireOp::ContextAppendMessage { message }
                    if message["origin"]["kind"] == "background_task")
        })
        .map(|(i, _)| i)
        .collect();
    assert_eq!(notification_appends.len(), 1, "exactly one durable append");
    let WireOp::ContextAppendMessage { message } = &ops[notification_appends[0]] else {
        unreachable!()
    };
    assert_eq!(message["origin"]["notificationId"], "task:t1:completed");
    assert_eq!(
        message["content"][0]["text"],
        "<notification id=\"task:t1:completed\"></notification>"
    );
    // No other append messages were recorded for the flush (the user steer
    // keeps the pre-existing contract).
    let all_appends = ops
        .iter()
        .filter(|op| matches!(op, WireOp::ContextAppendMessage { .. }))
        .count();
    assert_eq!(all_appends, 1, "the user steer flush stays unrecorded");

    // And the memory holds both steers as user messages in FIFO order.
    let binding = memory.lock().unwrap_or_else(|e| e.into_inner());
    let history = binding.history();
    let texts: Vec<String> = history
        .iter()
        .map(|m| m.message.text_content().to_string())
        .collect();
    let user_at = texts
        .iter()
        .position(|t| t == "user steer")
        .expect("user steer landed in memory");
    let notification_at = texts
        .iter()
        .position(|t| t.contains("<notification "))
        .expect("notification steer landed in memory");
    assert!(user_at < notification_at, "FIFO order: {texts:?}");
}

#[tokio::test]
async fn should_stop_turn_when_no_pending_steers_after_stop() {
    // With no pending steers, should_continue_after_stop returns false,
    // so the turn ends after one step.
    let llm = MockLlm::new(vec![MockResponse::end_turn("Done")]);
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());

    let result = turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Hello".to_string(),
            }],
            PromptOrigin::User,
            memory,
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(llm))),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("turn should succeed");

    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    assert_eq!(result.steps, 1);
}

#[tokio::test]
async fn should_replace_duplicate_tool_call_when_same_step_dedup_triggers() {
    // When the model calls the same tool with the same args twice in one
    // step, the second call should get a synthetic empty result.
    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![
            make_tool_call("call-1", "Echo", serde_json::json!({"message": "dup"})),
            make_tool_call("call-2", "Echo", serde_json::json!({"message": "dup"})),
        ]),
        MockResponse::end_turn("Done"),
    ]);
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());

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

    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    assert_eq!(result.steps, 2);

    // Check that both tool results are in history — the second should be
    // an empty string (synthetic dedup result).
    let history = {
        let guard = memory.lock().unwrap();
        guard.history().to_vec()
    };
    let tool_msgs: Vec<_> = history
        .iter()
        .filter(|m| m.message.role == mirri_core::message::Role::Tool)
        .collect();
    assert_eq!(tool_msgs.len(), 2, "expected 2 tool result messages");

    // The second tool result should be empty (synthetic dedup result).
    // The projector may replace empty output with a placeholder, so we
    // verify it's NOT the same as the first (which would be "hi").
    let second_text = tool_msgs[1].message.text_content();
    assert!(
        !second_text.contains("hi"),
        "second tool result should be empty (dedup), got: {second_text}"
    );
}

#[tokio::test]
async fn should_aggregate_usage_tokens_when_multiple_steps_execute() {
    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![make_tool_call(
            "call-1",
            "Echo",
            serde_json::json!({"message": "hi"}),
        )])
        .with_usage(100, 50),
        MockResponse::end_turn("Done").with_usage(200, 75),
    ]);
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());

    let result = turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Hello".to_string(),
            }],
            PromptOrigin::User,
            memory,
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(llm))),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("turn should succeed");

    assert_eq!(result.usage.input_tokens, 300);
    assert_eq!(result.usage.output_tokens, 125);
    assert_eq!(result.steps, 2);
}

#[test]
fn should_default_to_unlimited_steps_matching_the_ts_kernel() {
    // Given the turn-flow defaults with no explicit [loop_control]
    // configuration
    // Then turns are not step-limited — the TS kernel's default
    // (`maxStepsPerTurn: 0` = unlimited); users opt into a limit via
    // `max_steps_per_turn` in config.toml
    assert_eq!(TurnFlowConfig::default().max_steps, None);
}

#[tokio::test]
async fn should_return_max_steps_exceeded_error_when_step_limit_reached() {
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
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    let config = TurnFlowConfig {
        max_steps: Some(2),
        ..TurnFlowConfig::default()
    };

    let mut turn_flow = TurnFlow::new(config);

    let result = turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Hello".to_string(),
            }],
            PromptOrigin::User,
            memory,
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(llm))),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await;

    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        EngineError::MaxStepsExceeded(_)
    ));
}

#[tokio::test]
async fn should_increment_turn_id_when_multiple_turns_run() {
    let llm: Arc<dyn Llm> = Arc::new(MockLlm::new(vec![
        MockResponse::end_turn("First"),
        MockResponse::end_turn("Second"),
    ]));
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());

    let result1 = turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Hello".to_string(),
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
                text: "Again".to_string(),
            }],
            PromptOrigin::User,
            memory,
            &tools,
            Arc::new(FixedLlmResolver::new(llm.clone())),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("second turn should succeed");

    assert_eq!(result1.turn_id, 1);
    assert_eq!(result2.turn_id, 2);
}

// ---------------------------------------------------------------------------
// Gap 3: Permission Gate Wiring tests
// ---------------------------------------------------------------------------

/// A dummy LLM for hook tests (never actually called).
struct DummyLlm;
impl Llm for DummyLlm {
    fn system_prompt(&self) -> &str {
        "test"
    }
    fn model_name(&self) -> &str {
        "test"
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
    fn is_retryable_error(&self, _e: &ProviderErrorInfo) -> bool {
        false
    }
    fn chat(&self, _p: LlmChatParams) -> EngineResult<LlmEventStream> {
        Err(EngineError::ProviderError("not implemented".into()))
    }
}

/// Build a `TurnFlowHooks` with the given permission manager and no goal manager.
fn make_hooks_with_permission(
    memory: Arc<Mutex<ContextMemory>>,
    pm: Option<Arc<PermissionManager>>,
) -> TurnFlowHooks {
    make_hooks_with_permission_and_callback(memory, pm, None)
}

/// Build a `TurnFlowHooks` with the given permission manager and an optional
/// reverse-RPC callback (for `approval.ask` when the policy returns `Ask`).
fn make_hooks_with_permission_and_callback(
    memory: Arc<Mutex<ContextMemory>>,
    pm: Option<Arc<PermissionManager>>,
    reverse_rpc_callback: Option<Arc<dyn mirri_core::reverse_rpc::ReverseRpcCallback>>,
) -> TurnFlowHooks {
    let compaction = Arc::new(Mutex::new(FullCompaction::new(
        CompactionStrategy::default(),
    )));
    let steer_buffer = Arc::new(Mutex::new(SteerBuffer::new()));
    let deduper = Arc::new(Mutex::new(ToolCallDeduplicator::new()));
    let injection = Arc::new(mirri_core::turn_flow::InjectionManager::new());
    let cancel = CancelToken::new();
    let activity = Arc::new(mirri_core::turn_flow::ActivityTracker::new(
        compaction.clone(),
    ));
    TurnFlowHooks::new(
        memory,
        compaction,
        steer_buffer,
        deduper,
        injection,
        cancel,
        TurnFlowHooksConfig::default(),
        Arc::new(DummyLlm),
        pm,
        None,
        None,
        reverse_rpc_callback,
        None,
        None,
        None,
        activity,
        None,
    )
}

#[tokio::test]
async fn should_block_tool_when_permission_manager_has_deny_rule() {
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let pm = Arc::new(PermissionManager::new(
        PermissionMode::Manual,
        vec![PermissionRule {
            decision: RuleDecision::Deny,
            scope: RuleScope::User,
            pattern: "Bash".to_string(),
            reason: None,
        }],
    ));
    let hooks = make_hooks_with_permission(memory, Some(pm));
    let cancel = CancelToken::new();
    let args = serde_json::json!({"command": "rm -rf"});
    let display = ToolInputDisplay::Generic {
        summary: "test".into(),
    };
    let ctx = AuthorizeToolContext {
        turn_id: "1".to_string(),
        step: 1,
        tool_call_id: "call-1".to_string(),
        tool_name: "Bash".to_string(),
        args: &args,
        approval_rule: "",
        display: &display,
        cancel: cancel.clone(),
    };

    let result = hooks.authorize_tool_execution(ctx).await.unwrap();
    assert!(
        matches!(result, AuthorizeToolResult::Blocked { .. }),
        "expected Blocked, got {result:?}"
    );
}

#[tokio::test]
async fn should_approve_tool_when_permission_manager_has_allow_rule() {
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let pm = Arc::new(PermissionManager::new(
        PermissionMode::Manual,
        vec![PermissionRule {
            decision: RuleDecision::Allow,
            scope: RuleScope::User,
            pattern: "Read".to_string(),
            reason: None,
        }],
    ));
    let hooks = make_hooks_with_permission(memory, Some(pm));
    let cancel = CancelToken::new();
    let args = serde_json::json!({"path": "/tmp"});
    let display = ToolInputDisplay::Generic {
        summary: "test".into(),
    };
    let ctx = AuthorizeToolContext {
        turn_id: "1".to_string(),
        step: 1,
        tool_call_id: "call-1".to_string(),
        tool_name: "Read".to_string(),
        args: &args,
        approval_rule: "",
        display: &display,
        cancel: cancel.clone(),
    };

    let result = hooks.authorize_tool_execution(ctx).await.unwrap();
    assert!(
        matches!(result, AuthorizeToolResult::Approved),
        "expected Approved, got {result:?}"
    );
}

#[tokio::test]
async fn should_approve_tool_when_no_permission_manager_set() {
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let hooks = make_hooks_with_permission(memory, None);
    let cancel = CancelToken::new();
    let args = serde_json::json!({"command": "ls"});
    let display = ToolInputDisplay::Generic {
        summary: "test".into(),
    };
    let ctx = AuthorizeToolContext {
        turn_id: "1".to_string(),
        step: 1,
        tool_call_id: "call-1".to_string(),
        tool_name: "Bash".to_string(),
        args: &args,
        approval_rule: "",
        display: &display,
        cancel: cancel.clone(),
    };

    let result = hooks.authorize_tool_execution(ctx).await.unwrap();
    assert!(
        matches!(result, AuthorizeToolResult::Approved),
        "expected Approved (default), got {result:?}"
    );
}

#[tokio::test]
async fn should_block_tool_when_ask_rule_in_non_headless_mode() {
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    // In Manual mode (non-headless), a tool that falls through to the
    // FallbackAskPolicy gets Ask → Blocked.
    let pm = Arc::new(PermissionManager::new(PermissionMode::Manual, vec![]));
    // "Bash" is not in the default-approve list, so it will hit the
    // FallbackAskPolicy → Ask → Blocked (non-headless).
    let hooks = make_hooks_with_permission(memory, Some(pm));
    let cancel = CancelToken::new();
    let args = serde_json::json!({"command": "ls"});
    let display = ToolInputDisplay::Generic {
        summary: "test".into(),
    };
    let ctx = AuthorizeToolContext {
        turn_id: "1".to_string(),
        step: 1,
        tool_call_id: "call-1".to_string(),
        tool_name: "Bash".to_string(),
        args: &args,
        approval_rule: "",
        display: &display,
        cancel: cancel.clone(),
    };

    let result = hooks.authorize_tool_execution(ctx).await.unwrap();
    assert!(
        matches!(result, AuthorizeToolResult::Blocked { .. }),
        "expected Blocked (Ask in non-headless), got {result:?}"
    );
}

// ---------------------------------------------------------------------------
// Reverse-RPC approval round-trip tests
// ---------------------------------------------------------------------------

/// A mock [`ReverseRpcCallback`] that records every call and returns a canned
/// response. Used to verify the `approval.ask` round-trip in
/// `authorize_tool_execution`.
struct MockReverseRpcCallback {
    response: Value,
    last_method: Mutex<Option<String>>,
    last_params: Mutex<Option<Value>>,
}

#[async_trait::async_trait]
impl mirri_core::reverse_rpc::ReverseRpcCallback for MockReverseRpcCallback {
    async fn call(&self, method: &str, params: Value) -> Result<Value, String> {
        *self.last_method.lock().unwrap() = Some(method.to_string());
        *self.last_params.lock().unwrap() = Some(params);
        Ok(self.response.clone())
    }
}

/// Given a `TurnFlowHooks` wired with a reverse-RPC callback that returns
/// `{ "decision": "approved" }`, when the permission policy returns `Ask`
/// (Bash in Manual mode), then the hook issues an `approval.ask` reverse RPC
/// and the tool call is approved.
#[tokio::test]
async fn should_approve_tool_when_reverse_rpc_callback_returns_approved() {
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let pm = Arc::new(PermissionManager::new(PermissionMode::Manual, vec![]));
    let mock = Arc::new(MockReverseRpcCallback {
        response: serde_json::json!({ "decision": "approved" }),
        last_method: Mutex::new(None),
        last_params: Mutex::new(None),
    });
    let callback: Arc<dyn mirri_core::reverse_rpc::ReverseRpcCallback> = mock.clone();
    let hooks = make_hooks_with_permission_and_callback(memory, Some(pm), Some(callback));
    let cancel = CancelToken::new();
    let args = serde_json::json!({"command": "ls"});
    let display = ToolInputDisplay::Generic {
        summary: "test".into(),
    };
    let ctx = AuthorizeToolContext {
        turn_id: "1".to_string(),
        step: 1,
        tool_call_id: "call-1".to_string(),
        tool_name: "Bash".to_string(),
        args: &args,
        approval_rule: "",
        display: &display,
        cancel: cancel.clone(),
    };

    let result = hooks.authorize_tool_execution(ctx).await.unwrap();
    assert!(
        matches!(result, AuthorizeToolResult::Approved),
        "expected Approved when callback returns decision=approved, got {result:?}"
    );

    // The callback should have been called with "approval.ask".
    let method = mock.last_method.lock().unwrap().clone();
    assert_eq!(
        method.as_deref(),
        Some("approval.ask"),
        "callback should have been called with approval.ask"
    );

    // The request should include snake_case fields and the new `action` /
    // `display` fields.
    let params = mock.last_params.lock().unwrap().clone().unwrap();
    assert_eq!(params["tool_name"], "Bash");
    assert_eq!(params["tool_call_id"], "call-1");
    assert!(params["action"].as_str().unwrap().contains("execute Bash"));
    assert!(
        params["display"].is_object(),
        "display should be serialized"
    );
    assert_eq!(params["display"]["kind"], "generic");
    assert_eq!(params["display"]["summary"], "test");
}

/// Given a permission ask with a PermissionRequest hook wired, When the
/// policy chain returns `Ask` and the reverse-RPC approves, Then the hook
/// fires as a pure observation with the tool's input — it can never gate or
/// deny the call (TS parity: `permission.approval.requested`).
#[tokio::test]
async fn should_fire_permission_request_as_observation_when_policy_asks() {
    let marker_path = std::env::temp_dir().join(format!(
        "permission_request_hook_{}.json",
        std::process::id()
    ));
    let _ = std::fs::remove_file(&marker_path);

    let engine = Arc::new(HookEngine::new());
    engine.register(HookDef::new(
        HookEventType::PermissionRequest,
        format!("cat > {:?}", marker_path),
    ));
    // A PermissionResult hook proves the decision observation still fires
    // after the approving callback resolves.
    let result_marker =
        std::env::temp_dir().join(format!("permission_result_obs_{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&result_marker);
    engine.register(HookDef::new(
        HookEventType::PermissionResult,
        format!("echo decided > {:?}", result_marker),
    ));

    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let pm = Arc::new(PermissionManager::new(PermissionMode::Manual, vec![]));
    let mock = Arc::new(MockReverseRpcCallback {
        response: serde_json::json!({ "decision": "approved" }),
        last_method: Mutex::new(None),
        last_params: Mutex::new(None),
    });
    let callback: Arc<dyn mirri_core::reverse_rpc::ReverseRpcCallback> = mock.clone();

    let compaction = Arc::new(Mutex::new(FullCompaction::new(
        CompactionStrategy::default(),
    )));
    let steer_buffer = Arc::new(Mutex::new(SteerBuffer::new()));
    let deduper = Arc::new(Mutex::new(ToolCallDeduplicator::new()));
    let injection = Arc::new(mirri_core::turn_flow::InjectionManager::new());
    let cancel = CancelToken::new();
    let activity = Arc::new(mirri_core::turn_flow::ActivityTracker::new(
        compaction.clone(),
    ));
    let hooks = TurnFlowHooks::new(
        memory,
        compaction,
        steer_buffer,
        deduper,
        injection,
        cancel.clone(),
        TurnFlowHooksConfig {
            hook_engine: Some(engine),
            ..TurnFlowHooksConfig::default()
        },
        Arc::new(DummyLlm),
        Some(pm),
        None,
        None,
        Some(callback),
        None,
        None,
        None,
        activity,
        None,
    );

    let args = serde_json::json!({"command": "ls"});
    let display = ToolInputDisplay::Generic {
        summary: "test".into(),
    };
    let ctx = AuthorizeToolContext {
        turn_id: "1".to_string(),
        step: 1,
        tool_call_id: "call-1".to_string(),
        tool_name: "Bash".to_string(),
        args: &args,
        approval_rule: "",
        display: &display,
        cancel: cancel.clone(),
    };

    // When the authorization runs with an approving callback
    let result = hooks.authorize_tool_execution(ctx).await.unwrap();

    // Then the call is approved despite the hook being registered — the
    // PermissionRequest hook is an observer, never a gate.
    assert!(
        matches!(result, AuthorizeToolResult::Approved),
        "expected Approved, got {result:?}"
    );

    // And the hook observed the ask with the tool's input
    let content = std::fs::read_to_string(&marker_path).expect("hook input file should exist");
    let input: Value = serde_json::from_str(&content).expect("hook input should be JSON");
    assert_eq!(input["tool_name"], serde_json::json!("Bash"));
    assert_eq!(input["tool_input"]["command"], serde_json::json!("ls"));
    assert_eq!(input["tool_call_id"], serde_json::json!("call-1"));
    let _ = std::fs::remove_file(&marker_path);

    // And the PermissionResult observation fired after the decision
    assert!(
        result_marker.exists(),
        "PermissionResult hook should run after the approval"
    );
    let _ = std::fs::remove_file(&result_marker);
}

/// Given a turn flow with both a permission manager and a passing PreToolUse
/// hook wired, When a tool call runs, Then the PreToolUse hook fires exactly
/// once — the permission policy chain must not re-fire it (v2 parity: the
/// hook fires at the tool-executor boundary only).
#[tokio::test]
async fn should_fire_pre_tool_use_hook_once_when_permission_manager_is_wired() {
    let marker_path =
        std::env::temp_dir().join(format!("pre_tool_once_{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&marker_path);

    let engine = Arc::new(HookEngine::new());
    engine.register(HookDef::new(
        HookEventType::PreToolUse,
        format!("echo fired >> {:?}", marker_path),
    ));
    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());
    turn_flow.set_external_hooks(engine.clone(), HashMap::new());
    // Manual mode: the Echo call is gated by an Ask (no callback → blocked),
    // exercising the authorize path where the policy chain used to re-fire
    // the hook. The hook engine rides the permission services exactly as
    // the production `AgentScope` wiring does.
    turn_flow.set_permission_manager(Arc::new(PermissionManager::with_services(
        PermissionMode::Manual,
        vec![],
        PermissionServices {
            hook_engine: Some(engine),
            ..PermissionServices::default()
        },
    )));

    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![make_tool_call(
            "call-1",
            "Echo",
            serde_json::json!({"message": "hi"}),
        )]),
        MockResponse::end_turn("done"),
    ]);
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    // When a turn with a tool call runs
    turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Hello".to_string(),
            }],
            PromptOrigin::User,
            memory,
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(llm))),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("turn should succeed");

    // Then the PreToolUse hook fired exactly once
    let count = if marker_path.exists() {
        std::fs::read_to_string(&marker_path)
            .expect("marker readable")
            .lines()
            .filter(|l| !l.trim().is_empty())
            .count()
    } else {
        0
    };
    assert_eq!(count, 1, "PreToolUse must fire exactly once, got {count}");
    let _ = std::fs::remove_file(&marker_path);
}

/// Given a `TurnFlowHooks` wired with a reverse-RPC callback that returns
/// `{ "decision": "rejected", "feedback": "no" }`, when the permission policy
/// returns `Ask`, then the hook issues an `approval.ask` reverse RPC and the
/// tool call is blocked with the feedback as the reason.
#[tokio::test]
async fn should_block_tool_when_reverse_rpc_callback_returns_rejected() {
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let pm = Arc::new(PermissionManager::new(PermissionMode::Manual, vec![]));
    let mock = Arc::new(MockReverseRpcCallback {
        response: serde_json::json!({ "decision": "rejected", "feedback": "no" }),
        last_method: Mutex::new(None),
        last_params: Mutex::new(None),
    });
    let callback: Arc<dyn mirri_core::reverse_rpc::ReverseRpcCallback> = mock.clone();
    let hooks = make_hooks_with_permission_and_callback(memory, Some(pm), Some(callback));
    let cancel = CancelToken::new();
    let args = serde_json::json!({"command": "ls"});
    let display = ToolInputDisplay::Generic {
        summary: "test".into(),
    };
    let ctx = AuthorizeToolContext {
        turn_id: "1".to_string(),
        step: 1,
        tool_call_id: "call-1".to_string(),
        tool_name: "Bash".to_string(),
        args: &args,
        approval_rule: "",
        display: &display,
        cancel: cancel.clone(),
    };

    let result = hooks.authorize_tool_execution(ctx).await.unwrap();
    match result {
        AuthorizeToolResult::Blocked { reason } => {
            assert_eq!(
                reason, "no",
                "blocked reason should be the feedback from the callback"
            );
        }
        other => panic!("expected Blocked, got {other:?}"),
    }

    // The callback should have been called with "approval.ask".
    let method = mock.last_method.lock().unwrap().clone();
    assert_eq!(
        method.as_deref(),
        Some("approval.ask"),
        "callback should have been called with approval.ask"
    );
}

/// [`make_hooks_with_permission_and_callback`] plus an interaction kernel so
/// the ask registers with the gate (the B4-a seam the grant sink rides).
#[allow(clippy::too_many_arguments)]
fn make_hooks_with_permission_callback_and_kernel(
    memory: Arc<Mutex<ContextMemory>>,
    pm: Option<Arc<PermissionManager>>,
    reverse_rpc_callback: Option<Arc<dyn mirri_core::reverse_rpc::ReverseRpcCallback>>,
    interaction_kernel: Option<Arc<mirri_core::InteractionKernel>>,
) -> TurnFlowHooks {
    let compaction = Arc::new(Mutex::new(FullCompaction::new(
        CompactionStrategy::default(),
    )));
    let steer_buffer = Arc::new(Mutex::new(SteerBuffer::new()));
    let deduper = Arc::new(Mutex::new(ToolCallDeduplicator::new()));
    let injection = Arc::new(mirri_core::turn_flow::InjectionManager::new());
    let cancel = CancelToken::new();
    let activity = Arc::new(mirri_core::turn_flow::ActivityTracker::new(
        compaction.clone(),
    ));
    TurnFlowHooks::new(
        memory,
        compaction,
        steer_buffer,
        deduper,
        injection,
        cancel,
        TurnFlowHooksConfig {
            interaction_kernel,
            ..TurnFlowHooksConfig::default()
        },
        Arc::new(DummyLlm),
        pm,
        None,
        None,
        reverse_rpc_callback,
        None,
        None,
        None,
        activity,
        None,
    )
}

/// A test grant sink recording every `(dir, persist)` application.
#[derive(Default)]
struct RecordingGrantSink {
    calls: Arc<Mutex<Vec<(std::path::PathBuf, bool)>>>,
}

impl mirri_core::AdditionalDirGrantSink for RecordingGrantSink {
    fn apply_grant(&self, dir: &std::path::Path, persist: bool) {
        self.calls
            .lock()
            .unwrap()
            .push((dir.to_path_buf(), persist));
    }
}

/// Given a Manual-mode permission manager wired to a workspace sandbox and a
/// reverse-RPC callback that approves a grant-carrying outside read with
/// session scope, when the hooks authorize the Read, then the ask payload
/// carries the grant dir, the directory grant is applied through the
/// kernel's sink (not persisted), the call is approved, and the tool-name
/// pattern is NOT memorized into the session approvals (a directory grant is
/// its own memory — memorizing "Read" would over-approve every later read).
#[tokio::test]
async fn should_apply_session_dir_grant_and_skip_pattern_record_when_reply_approves_an_outside_read_with_session_scope()
 {
    let _root_guard = tempfile::tempdir().expect("workspace root tempdir");
    let workspace_root = _root_guard.path().to_path_buf();
    let outside = tempfile::tempdir().expect("outside tempdir must be creatable");
    let outside_file = outside.path().join("secret.txt");
    std::fs::write(&outside_file, "s3cret").expect("write the outside file");

    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let pm = Arc::new(PermissionManager::with_services(
        PermissionMode::Manual,
        vec![],
        PermissionServices {
            workspace_paths: Some(Arc::new(mirri_core::tools::WorkspacePaths::new(
                workspace_root,
            ))),
            ..PermissionServices::default()
        },
    ));
    let mock = Arc::new(MockReverseRpcCallback {
        response: serde_json::json!({ "decision": "approved", "scope": "session" }),
        last_method: Mutex::new(None),
        last_params: Mutex::new(None),
    });
    let callback: Arc<dyn mirri_core::reverse_rpc::ReverseRpcCallback> = mock.clone();

    let sink = Arc::new(RecordingGrantSink::default());
    let approval_gate = Arc::new(mirri_core::ApprovalGate::new(false));
    let question_gate = Arc::new(mirri_core::QuestionGate::new());
    let kernel = Arc::new(
        mirri_core::InteractionKernel::new(Arc::clone(&approval_gate), Arc::clone(&question_gate))
            .with_grant_sink(Some(
                Arc::clone(&sink) as Arc<dyn mirri_core::AdditionalDirGrantSink>
            )),
    );
    let hooks = make_hooks_with_permission_callback_and_kernel(
        memory,
        Some(Arc::clone(&pm)),
        Some(callback),
        Some(kernel),
    );
    let cancel = CancelToken::new();
    let args = serde_json::json!({ "path": outside_file.to_string_lossy() });
    let display = ToolInputDisplay::FileIo {
        path: outside_file.to_string_lossy().into_owned(),
        operation: "read".into(),
    };
    let ctx = AuthorizeToolContext {
        turn_id: "1".to_string(),
        step: 1,
        tool_call_id: "call-read-outside".to_string(),
        tool_name: "Read".to_string(),
        args: &args,
        approval_rule: "Read",
        display: &display,
        cancel: cancel.clone(),
    };

    let result = hooks.authorize_tool_execution(ctx).await.unwrap();
    assert!(
        matches!(result, AuthorizeToolResult::Approved),
        "expected Approved, got {result:?}"
    );

    // The ask payload carries the grant dir under `grant.dir` (additive).
    let params = mock.last_params.lock().unwrap().clone().unwrap();
    let canonical_dir = outside.path().canonicalize().unwrap();
    assert_eq!(
        params["grant"]["dir"],
        serde_json::json!(canonical_dir.to_string_lossy()),
        "the approval.ask payload must carry the grant dir"
    );

    // The directory grant reached the sink with persist=false.
    let calls = sink.calls.lock().unwrap().clone();
    assert_eq!(
        calls,
        vec![(canonical_dir.clone(), false)],
        "session scope must apply the dir grant without persisting"
    );

    // The tool-name pattern was NOT memorized — the directory grant is the
    // memory.
    assert!(
        pm.all_session_approvals().is_empty(),
        "a grant ask must not memorize the tool pattern, got {:?}",
        pm.all_session_approvals()
    );
}

/// Given a grant-carrying outside read approved with persist scope through
/// the reverse-RPC reply, when the hooks authorize, then the dir grant is
/// applied with persist=true (the local.toml write is the embedded sink's
/// job — the hook only forwards the flag).
#[tokio::test]
async fn should_apply_persisted_dir_grant_when_reply_approves_an_outside_read_with_persist_scope() {
    let _root_guard = tempfile::tempdir().expect("workspace root tempdir");
    let workspace_root = _root_guard.path().to_path_buf();
    let outside = tempfile::tempdir().expect("outside tempdir must be creatable");
    let outside_file = outside.path().join("secret.txt");
    std::fs::write(&outside_file, "s3cret").expect("write the outside file");

    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let pm = Arc::new(PermissionManager::with_services(
        PermissionMode::Manual,
        vec![],
        PermissionServices {
            workspace_paths: Some(Arc::new(mirri_core::tools::WorkspacePaths::new(
                workspace_root,
            ))),
            ..PermissionServices::default()
        },
    ));
    let mock = Arc::new(MockReverseRpcCallback {
        response: serde_json::json!({ "decision": "approved", "scope": "persist" }),
        last_method: Mutex::new(None),
        last_params: Mutex::new(None),
    });
    let callback: Arc<dyn mirri_core::reverse_rpc::ReverseRpcCallback> = mock.clone();

    let sink = Arc::new(RecordingGrantSink::default());
    let approval_gate = Arc::new(mirri_core::ApprovalGate::new(false));
    let question_gate = Arc::new(mirri_core::QuestionGate::new());
    let kernel = Arc::new(
        mirri_core::InteractionKernel::new(Arc::clone(&approval_gate), Arc::clone(&question_gate))
            .with_grant_sink(Some(
                Arc::clone(&sink) as Arc<dyn mirri_core::AdditionalDirGrantSink>
            )),
    );
    let hooks = make_hooks_with_permission_callback_and_kernel(
        memory,
        Some(Arc::clone(&pm)),
        Some(callback),
        Some(kernel),
    );
    let cancel = CancelToken::new();
    let args = serde_json::json!({ "path": outside_file.to_string_lossy() });
    let display = ToolInputDisplay::FileIo {
        path: outside_file.to_string_lossy().into_owned(),
        operation: "read".into(),
    };
    let ctx = AuthorizeToolContext {
        turn_id: "1".to_string(),
        step: 1,
        tool_call_id: "call-read-outside".to_string(),
        tool_name: "Read".to_string(),
        args: &args,
        approval_rule: "Read",
        display: &display,
        cancel: cancel.clone(),
    };

    let result = hooks.authorize_tool_execution(ctx).await.unwrap();
    assert!(matches!(result, AuthorizeToolResult::Approved));

    let calls = sink.calls.lock().unwrap().clone();
    assert_eq!(
        calls,
        vec![(outside.path().canonicalize().unwrap(), true)],
        "persist scope must forward persist=true to the sink"
    );
    assert!(
        pm.all_session_approvals().is_empty(),
        "a grant ask must not memorize the tool pattern"
    );
}

/// Given a grant-carrying outside read approved with once scope (or
/// rejected), when the hooks authorize, then no grant reaches the sink.
#[tokio::test]
async fn should_not_apply_dir_grant_when_reply_approves_with_once_scope_or_rejects() {
    for (response, expected) in [
        (
            serde_json::json!({ "decision": "approved", "scope": "once" }),
            true,
        ),
        (
            serde_json::json!({ "decision": "rejected", "feedback": "no" }),
            false,
        ),
    ] {
        let _root_guard = tempfile::tempdir().expect("workspace root tempdir");
        let workspace_root = _root_guard.path().to_path_buf();
        let outside = tempfile::tempdir().expect("outside tempdir must be creatable");
        let outside_file = outside.path().join("secret.txt");
        std::fs::write(&outside_file, "s3cret").expect("write the outside file");

        let memory = Arc::new(Mutex::new(ContextMemory::new()));
        let pm = Arc::new(PermissionManager::with_services(
            PermissionMode::Manual,
            vec![],
            PermissionServices {
                workspace_paths: Some(Arc::new(mirri_core::tools::WorkspacePaths::new(
                    workspace_root,
                ))),
                ..PermissionServices::default()
            },
        ));
        let mock = Arc::new(MockReverseRpcCallback {
            response,
            last_method: Mutex::new(None),
            last_params: Mutex::new(None),
        });
        let callback: Arc<dyn mirri_core::reverse_rpc::ReverseRpcCallback> = mock.clone();

        let sink = Arc::new(RecordingGrantSink::default());
        let approval_gate = Arc::new(mirri_core::ApprovalGate::new(false));
        let question_gate = Arc::new(mirri_core::QuestionGate::new());
        let kernel = Arc::new(
            mirri_core::InteractionKernel::new(
                Arc::clone(&approval_gate),
                Arc::clone(&question_gate),
            )
            .with_grant_sink(Some(
                Arc::clone(&sink) as Arc<dyn mirri_core::AdditionalDirGrantSink>
            )),
        );
        let hooks = make_hooks_with_permission_callback_and_kernel(
            memory,
            Some(Arc::clone(&pm)),
            Some(callback),
            Some(kernel),
        );
        let cancel = CancelToken::new();
        let args = serde_json::json!({ "path": outside_file.to_string_lossy() });
        let display = ToolInputDisplay::FileIo {
            path: outside_file.to_string_lossy().into_owned(),
            operation: "read".into(),
        };
        let ctx = AuthorizeToolContext {
            turn_id: "1".to_string(),
            step: 1,
            tool_call_id: "call-read-outside".to_string(),
            tool_name: "Read".to_string(),
            args: &args,
            approval_rule: "Read",
            display: &display,
            cancel: cancel.clone(),
        };

        let result = hooks.authorize_tool_execution(ctx).await.unwrap();
        if expected {
            assert!(
                matches!(result, AuthorizeToolResult::Approved),
                "a once-scope approval must approve the call"
            );
        } else {
            // Plan D4: a rejected GRANT ask carries the user feedback plus
            // the NEVER-bypass contract — the model must not route around
            // the declined directory grant.
            let expected_reason = format!(
                "no. {}",
                mirri_core::permission::OUTSIDE_SANDBOX_DENIAL_CONTRACT
            );
            assert!(
                matches!(
                    result,
                    AuthorizeToolResult::Blocked { reason } if reason == expected_reason
                ),
                "a rejected grant ask must block with the feedback and the NEVER-bypass contract"
            );
        }
        let calls = sink.calls.lock().unwrap().clone();
        assert!(
            calls.is_empty(),
            "an once-scope or rejected ask must not apply any grant, got {calls:?}"
        );
    }
}

/// Given a grant-carrying outside read, when the reverse-RPC reply
/// rejects (user feedback) or cancels (default "denied by user"), then
/// the model-visible Blocked reason appends the NEVER-bypass contract
/// (plan D4) to the feedback — the user declined the directory grant, so
/// the model must not route around the sandbox through other tools,
/// shell, sub-agents, or alternate path spellings.
#[tokio::test]
async fn should_append_the_never_bypass_contract_when_reply_rejects_or_cancels_a_grant_ask() {
    let contract = mirri_core::permission::OUTSIDE_SANDBOX_DENIAL_CONTRACT;
    for (response, expected_feedback) in [
        (
            serde_json::json!({ "decision": "rejected", "feedback": "no" }),
            "no",
        ),
        (
            serde_json::json!({ "decision": "cancelled" }),
            "denied by user",
        ),
    ] {
        let _root_guard = tempfile::tempdir().expect("workspace root tempdir");
        let workspace_root = _root_guard.path().to_path_buf();
        let outside = tempfile::tempdir().expect("outside tempdir must be creatable");
        let outside_file = outside.path().join("secret.txt");
        std::fs::write(&outside_file, "s3cret").expect("write the outside file");

        let memory = Arc::new(Mutex::new(ContextMemory::new()));
        let pm = Arc::new(PermissionManager::with_services(
            PermissionMode::Manual,
            vec![],
            PermissionServices {
                workspace_paths: Some(Arc::new(mirri_core::tools::WorkspacePaths::new(
                    workspace_root,
                ))),
                ..PermissionServices::default()
            },
        ));
        let mock = Arc::new(MockReverseRpcCallback {
            response,
            last_method: Mutex::new(None),
            last_params: Mutex::new(None),
        });
        let callback: Arc<dyn mirri_core::reverse_rpc::ReverseRpcCallback> = mock.clone();

        let sink = Arc::new(RecordingGrantSink::default());
        let approval_gate = Arc::new(mirri_core::ApprovalGate::new(false));
        let question_gate = Arc::new(mirri_core::QuestionGate::new());
        let kernel = Arc::new(
            mirri_core::InteractionKernel::new(
                Arc::clone(&approval_gate),
                Arc::clone(&question_gate),
            )
            .with_grant_sink(Some(
                Arc::clone(&sink) as Arc<dyn mirri_core::AdditionalDirGrantSink>
            )),
        );
        let hooks = make_hooks_with_permission_callback_and_kernel(
            memory,
            Some(Arc::clone(&pm)),
            Some(callback),
            Some(kernel),
        );
        let cancel = CancelToken::new();
        let args = serde_json::json!({ "path": outside_file.to_string_lossy() });
        let display = ToolInputDisplay::FileIo {
            path: outside_file.to_string_lossy().into_owned(),
            operation: "read".into(),
        };
        let ctx = AuthorizeToolContext {
            turn_id: "1".to_string(),
            step: 1,
            tool_call_id: "call-read-outside".to_string(),
            tool_name: "Read".to_string(),
            args: &args,
            approval_rule: "Read",
            display: &display,
            cancel: cancel.clone(),
        };

        let result = hooks.authorize_tool_execution(ctx).await.unwrap();
        match result {
            AuthorizeToolResult::Blocked { reason } => {
                assert_eq!(
                    reason,
                    format!("{expected_feedback}. {contract}"),
                    "a '{expected_feedback}' outcome for a grant ask must carry \
                         the NEVER-bypass contract"
                );
            }
            other => panic!("expected Blocked, got {other:?}"),
        }

        let calls = sink.calls.lock().unwrap().clone();
        assert!(
            calls.is_empty(),
            "a rejected or cancelled ask must not apply any grant, got {calls:?}"
        );
    }
}

/// A custom `LoopHooks` implementation that returns a `Synthetic` result from
/// `authorize_tool_execution` for a specific tool name. This is used to test
/// the `AuthorizeToolResult::Synthetic` path, which the `PermissionManager`
/// itself never produces (its `PermissionPolicyResult` enum has no `Synthetic`
/// variant — `Synthetic` is only reachable through a custom hooks impl).
struct SyntheticAuthorizingHooks {
    synthetic_tool_name: String,
    synthetic_result: ToolResult,
}

impl LoopHooks for SyntheticAuthorizingHooks {
    fn authorize_tool_execution(
        &self,
        ctx: AuthorizeToolContext<'_>,
    ) -> HookFuture<EngineResult<AuthorizeToolResult>> {
        let tool_name = ctx.tool_name.clone();
        let result = if tool_name == self.synthetic_tool_name {
            AuthorizeToolResult::Synthetic {
                result: self.synthetic_result.clone(),
            }
        } else {
            AuthorizeToolResult::Approved
        };
        Box::pin(async move { Ok(result) })
    }

    // The prepare hook must allow the call to reach authorize.
    fn prepare_tool_execution(
        &self,
        _ctx: PrepareToolContext<'_>,
    ) -> HookFuture<EngineResult<PrepareToolResult>> {
        Box::pin(async { Ok(PrepareToolResult::Allowed) })
    }
}

#[tokio::test]
async fn should_return_synthetic_result_when_prepare_tool_call_with_synthetic_hooks() {
    // The permission system's `PermissionPolicyResult` has no `Synthetic`
    // variant, so `TurnFlowHooks::authorize_tool_execution` (which delegates
    // to `PermissionManager::before_tool_call`) can never produce
    // `AuthorizeToolResult::Synthetic`. The `Synthetic` variant is only
    // reachable through a custom `LoopHooks` implementation. This test
    // verifies that `prepare_tool_call` correctly maps
    // `AuthorizeToolResult::Synthetic` into `PrepareResult::Synthetic` with
    // the exact `ToolResult` the hook returned.
    let hooks = SyntheticAuthorizingHooks {
        synthetic_tool_name: "Echo".to_string(),
        synthetic_result: ToolResult::success("synthetic permission result"),
    };

    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let cancel = CancelToken::new();

    // Preflight the tool call to get a ValidatedToolCall.
    let validated = match mirri_core::tool_registry::preflight_tool_call(
        "Echo",
        r#"{"message":"hi"}"#,
        "call-syn-1",
        &tools,
        &mut validators,
    ) {
        mirri_core::tool_registry::PreflightResult::Valid(v) => v,
        mirri_core::tool_registry::PreflightResult::Rejected(r) => {
            panic!("expected Valid, got Rejected: {}", r.error)
        }
    };

    let result = prepare_tool_call(validated, "1", 1, &cancel, &hooks)
        .await
        .expect("prepare_tool_call should succeed");

    match result {
        PrepareResult::Synthetic(synthetic) => {
            assert_eq!(synthetic.tool_call_id, "call-syn-1");
            assert_eq!(synthetic.tool_name, "Echo");
            let text = match &synthetic.result.output {
                ToolOutput::Text(t) => t.clone(),
                _ => panic!("expected text output"),
            };
            assert_eq!(
                text, "synthetic permission result",
                "synthetic result should carry the exact ToolResult from the hook"
            );
        }
        _ => panic!("expected PrepareResult::Synthetic"),
    }
}

// ---------------------------------------------------------------------------
// Gap 5: Goal Budget Enforcement tests
// ---------------------------------------------------------------------------

/// Build a `TurnFlowHooks` with the given goal manager and no permission manager.
fn make_hooks_with_goal(
    memory: Arc<Mutex<ContextMemory>>,
    gm: Option<Arc<GoalManager>>,
) -> TurnFlowHooks {
    let compaction = Arc::new(Mutex::new(FullCompaction::new(
        CompactionStrategy::default(),
    )));
    let steer_buffer = Arc::new(Mutex::new(SteerBuffer::new()));
    let deduper = Arc::new(Mutex::new(ToolCallDeduplicator::new()));
    let injection = Arc::new(mirri_core::turn_flow::InjectionManager::new());
    let cancel = CancelToken::new();
    let activity = Arc::new(mirri_core::turn_flow::ActivityTracker::new(
        compaction.clone(),
    ));
    TurnFlowHooks::new(
        memory,
        compaction,
        steer_buffer,
        deduper,
        injection,
        cancel,
        TurnFlowHooksConfig::default(),
        Arc::new(DummyLlm),
        None,
        gm,
        None,
        None,
        None,
        None,
        None,
        activity,
        None,
    )
}

/// Like `make_hooks_with_goal` but also returns the shared steer buffer so
/// tests can inject pending steers before calling `should_continue_after_stop`.
fn make_hooks_with_goal_and_steer(
    memory: Arc<Mutex<ContextMemory>>,
    gm: Option<Arc<GoalManager>>,
) -> (TurnFlowHooks, Arc<Mutex<SteerBuffer>>) {
    let compaction = Arc::new(Mutex::new(FullCompaction::new(
        CompactionStrategy::default(),
    )));
    let steer_buffer = Arc::new(Mutex::new(SteerBuffer::new()));
    let deduper = Arc::new(Mutex::new(ToolCallDeduplicator::new()));
    let injection = Arc::new(mirri_core::turn_flow::InjectionManager::new());
    let cancel = CancelToken::new();
    let activity = Arc::new(mirri_core::turn_flow::ActivityTracker::new(
        compaction.clone(),
    ));
    let hooks = TurnFlowHooks::new(
        memory,
        compaction,
        steer_buffer.clone(),
        deduper,
        injection,
        cancel,
        TurnFlowHooksConfig::default(),
        Arc::new(DummyLlm),
        None,
        gm,
        None,
        None,
        None,
        None,
        None,
        activity,
        None,
    );
    (hooks, steer_buffer)
}

fn make_continue_ctx(cancel: &CancelToken) -> ContinueAfterStopContext {
    ContinueAfterStopContext {
        turn_id: "1".to_string(),
        stop_reason: "completed".to_string(),
        cancel: cancel.clone(),
    }
}

#[tokio::test]
async fn should_continue_turn_when_active_goal_has_remaining_budget() {
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let gm = Arc::new(GoalManager::new());
    gm.create("Do the thing".to_string(), None).unwrap();
    gm.set_budget(GoalBudget {
        turns: Some(10),
        tokens: None,
        wall_clock_ms: None,
    });
    let hooks = make_hooks_with_goal(memory, Some(gm));
    let cancel = CancelToken::new();
    let ctx = make_continue_ctx(&cancel);

    let result = hooks.should_continue_after_stop(ctx).await.unwrap();
    assert!(
        result.continue_turn,
        "expected turn to continue with active goal and remaining budget"
    );
}

#[tokio::test]
async fn should_stop_turn_when_active_goal_has_exhausted_budget() {
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let gm = Arc::new(GoalManager::new());
    gm.create("Do the thing".to_string(), None).unwrap();
    gm.set_budget(GoalBudget {
        turns: Some(1),
        tokens: None,
        wall_clock_ms: None,
    });
    // Record one step to exhaust the turn budget (1 >= 1).
    gm.record_step_usage(LlmUsage {
        input_tokens: 10,
        output_tokens: 5,
        ..Default::default()
    })
    .unwrap();
    let hooks = make_hooks_with_goal(memory, Some(gm));
    let cancel = CancelToken::new();
    let ctx = make_continue_ctx(&cancel);

    let result = hooks.should_continue_after_stop(ctx).await.unwrap();
    assert!(
        !result.continue_turn,
        "expected turn to stop when goal budget is exhausted"
    );
}

#[tokio::test]
async fn should_stop_turn_when_goal_is_completed() {
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let gm = Arc::new(GoalManager::new());
    gm.create("Done".to_string(), None).unwrap();
    gm.update(GoalStatus::Complete);
    let hooks = make_hooks_with_goal(memory, Some(gm));
    let cancel = CancelToken::new();
    let ctx = make_continue_ctx(&cancel);

    let result = hooks.should_continue_after_stop(ctx).await.unwrap();
    assert!(
        !result.continue_turn,
        "expected turn to stop when goal is complete"
    );
}

#[tokio::test]
async fn should_continue_turn_when_pending_steers_and_exhausted_goal_budget() {
    // Steers take precedence over goal budget: even when the goal's budget is
    // exhausted, pending steers must be flushed and the turn continued so the
    // model can respond to the steered content.
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let gm = Arc::new(GoalManager::new());
    gm.create("Do the thing".to_string(), None).unwrap();
    gm.set_budget(GoalBudget {
        turns: Some(1),
        tokens: None,
        wall_clock_ms: None,
    });
    // Exhaust the turn budget (1 >= 1).
    gm.record_step_usage(LlmUsage {
        input_tokens: 10,
        output_tokens: 5,
        ..Default::default()
    })
    .unwrap();
    assert!(
        gm.is_budget_exhausted(),
        "precondition: goal budget should be exhausted"
    );

    let (hooks, steer_buffer) = make_hooks_with_goal_and_steer(memory, Some(gm));

    // Inject a pending steer — this must take precedence over the exhausted
    // goal budget.
    steer_buffer.lock().unwrap().push(
        vec![ContentPart::Text {
            text: "new direction".to_string(),
        }],
        PromptOrigin::User,
    );

    let cancel = CancelToken::new();
    let ctx = make_continue_ctx(&cancel);

    let result = hooks.should_continue_after_stop(ctx).await.unwrap();
    assert!(
        result.continue_turn,
        "steers should take precedence over exhausted goal budget"
    );

    // The steer should have been flushed (consumed) by the hook.
    assert!(
        steer_buffer.lock().unwrap().is_empty(),
        "steer buffer should be empty after should_continue_after_stop"
    );
}

/// Given a steer buffer holding only a parked background-task notification,
/// when the model naturally stops, then the turn ends cleanly
/// (`continue_turn: false`) and the notification stays parked in the buffer
/// — the stop boundary must not fold parked notifications after the turn's
/// final summary; the task driver's idle wakeup delivers them as their own
/// turn on the next tick instead.
#[tokio::test]
async fn should_stop_and_keep_the_notification_parked_when_only_notifications_are_buffered() {
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let (hooks, steer_buffer) = make_hooks_with_goal_and_steer(memory.clone(), None);

    // A parked background-task notification steer (the busy-path park shape)
    steer_buffer.lock().unwrap().push(
        vec![ContentPart::Text {
            text: "<notification id=\"task:t1:completed\"></notification>".to_string(),
        }],
        PromptOrigin::BackgroundTask {
            task_id: "t1".to_string(),
            status: "completed".to_string(),
            notification_id: "task:t1:completed".to_string(),
        },
    );

    let cancel = CancelToken::new();
    let ctx = make_continue_ctx(&cancel);

    let result = hooks.should_continue_after_stop(ctx).await.unwrap();
    assert!(
        !result.continue_turn,
        "a parked notification alone must not force the turn to continue"
    );

    // The notification is still parked (delivery belongs to the idle wakeup)
    let buf = steer_buffer.lock().unwrap();
    assert_eq!(buf.len(), 1, "the parked notification must stay buffered");
    assert!(
        matches!(
            buf.iter().next().map(|s| &s.origin),
            Some(PromptOrigin::BackgroundTask { .. })
        ),
        "the buffered steer is still the parked notification"
    );
}

/// Given a steer buffer holding a parked user steer, when the model naturally
/// stops, then the turn continues and the user steer has been flushed into
/// context memory — the user-interjection semantics stay intact (regression
/// guard for the notification-parking change).
#[tokio::test]
async fn should_continue_and_flush_a_user_steer_into_memory_when_only_a_user_steer_is_buffered() {
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let (hooks, steer_buffer) = make_hooks_with_goal_and_steer(memory.clone(), None);

    steer_buffer.lock().unwrap().push(
        vec![ContentPart::Text {
            text: "new direction".to_string(),
        }],
        PromptOrigin::User,
    );

    let cancel = CancelToken::new();
    let ctx = make_continue_ctx(&cancel);

    let result = hooks.should_continue_after_stop(ctx).await.unwrap();
    assert!(
        result.continue_turn,
        "a user steer must still continue the turn"
    );
    assert!(
        steer_buffer.lock().unwrap().is_empty(),
        "the user steer must be consumed by the stop-boundary flush"
    );
    let guard = memory.lock().unwrap();
    assert!(
        guard
            .history()
            .iter()
            .any(|m| m.message.text_content() == "new direction"),
        "the user steer must land in context memory"
    );
}

/// Given a steer buffer mixing user steers with background-task notifications
/// (FIFO), when the model naturally stops, then the user steers flush into
/// context memory and continue the turn while every notification stays parked
/// in the buffer, preserving their FIFO relative order — the wakeup delivers
/// them later, in the order they arrived.
#[tokio::test]
async fn should_flush_user_steers_but_keep_notifications_in_order_when_the_buffer_is_mixed() {
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let (hooks, steer_buffer) = make_hooks_with_goal_and_steer(memory.clone(), None);

    let notification_origin = |task: &str| PromptOrigin::BackgroundTask {
        task_id: task.to_string(),
        status: "completed".to_string(),
        notification_id: format!("task:{task}:completed"),
    };
    {
        let mut buf = steer_buffer.lock().unwrap();
        buf.push(
            vec![ContentPart::Text {
                text: "notification one".to_string(),
            }],
            notification_origin("t1"),
        );
        buf.push(
            vec![ContentPart::Text {
                text: "user steer".to_string(),
            }],
            PromptOrigin::User,
        );
        buf.push(
            vec![ContentPart::Text {
                text: "notification two".to_string(),
            }],
            notification_origin("t2"),
        );
    }

    let cancel = CancelToken::new();
    let ctx = make_continue_ctx(&cancel);

    let result = hooks.should_continue_after_stop(ctx).await.unwrap();
    assert!(
        result.continue_turn,
        "a mixed buffer with user steers must continue the turn"
    );

    // The user steer landed in memory; the notifications stayed parked.
    {
        let guard = memory.lock().unwrap();
        assert!(
            guard
                .history()
                .iter()
                .any(|m| m.message.text_content() == "user steer"),
            "the user steer must land in context memory"
        );
        assert!(
            !guard
                .history()
                .iter()
                .any(|m| m.message.text_content().contains("notification")),
            "the parked notifications must not enter context at the stop boundary"
        );
    }
    let texts: Vec<String> = {
        let buf = steer_buffer.lock().unwrap();
        assert_eq!(buf.len(), 2, "both notifications must stay buffered");
        buf.iter()
            .map(|s| match &s.content[0] {
                ContentPart::Text { text } => text.clone(),
                _ => String::new(),
            })
            .collect()
    };
    assert_eq!(
        texts,
        vec![
            "notification one".to_string(),
            "notification two".to_string()
        ],
        "the parked notifications keep their FIFO relative order"
    );
}

#[tokio::test]
async fn should_stop_turn_when_goal_is_paused_and_no_steers() {
    // A paused goal does not drive continuation — it's treated as inactive
    // (is_goal_active returns false for Paused), so the turn stops.
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let gm = Arc::new(GoalManager::new());
    gm.create("On hold".to_string(), None).unwrap();
    gm.update(GoalStatus::Paused);
    assert!(
        !gm.is_goal_active(),
        "precondition: paused goal should not be active"
    );

    let hooks = make_hooks_with_goal(memory, Some(gm));
    let cancel = CancelToken::new();
    let ctx = make_continue_ctx(&cancel);

    let result = hooks.should_continue_after_stop(ctx).await.unwrap();
    assert!(
        !result.continue_turn,
        "paused goal with no steers should not continue the turn"
    );
}

#[tokio::test]
async fn should_stop_turn_when_no_goal_manager_set() {
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let hooks = make_hooks_with_goal(memory, None);
    let cancel = CancelToken::new();
    let ctx = make_continue_ctx(&cancel);

    let result = hooks.should_continue_after_stop(ctx).await.unwrap();
    assert!(
        !result.continue_turn,
        "expected turn to stop when no goal manager is set"
    );
}

#[tokio::test]
async fn should_record_goal_usage_when_step_completes() {
    // This test verifies that record_step_usage is wired through the
    // TurnFlowHooks. We test the callback directly since the full turn
    // integration requires a working LLM.
    let gm = Arc::new(GoalManager::new());
    gm.create("Track usage".to_string(), None).unwrap();
    gm.set_budget(GoalBudget {
        turns: Some(5),
        tokens: Some(1000),
        wall_clock_ms: None,
    });

    // Simulate recording step usage.
    let result = gm
        .record_step_usage(LlmUsage {
            input_tokens: 100,
            output_tokens: 50,
            ..Default::default()
        })
        .unwrap();

    assert_eq!(result, Some(false), "budget should not be exhausted yet");
    assert_eq!(gm.used_turns(), 1);
    assert_eq!(gm.used_tokens(), 150);
}

// ---------------------------------------------------------------------------
// Gap 3+5: TurnFlow integration tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_activate_permission_gate_when_permission_manager_set_on_turn_flow() {
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let pm = Arc::new(PermissionManager::new(
        PermissionMode::Manual,
        vec![PermissionRule {
            decision: RuleDecision::Deny,
            scope: RuleScope::User,
            pattern: "Echo".to_string(),
            reason: None,
        }],
    ));
    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());
    turn_flow.set_permission_manager(pm);

    // The permission manager should now be set on the TurnFlow.
    // We verify via a turn that the Echo tool is blocked.
    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![make_tool_call(
            "call-1",
            "Echo",
            serde_json::json!({"message": "hi"}),
        )]),
        MockResponse::end_turn("Done"),
    ]);
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let cancel = CancelToken::new();

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

    // The turn should complete — the blocked tool call produces an error
    // result that the model sees, then the model ends the turn.
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);

    // Prove the permission gate actually fired: inspect the conversation
    // history and verify that the blocked tool result is present.
    let history = {
        let guard = memory.lock().unwrap();
        guard.history().to_vec()
    };

    // There must be an assistant message containing a tool call to "Echo".
    let has_assistant_echo_call = history.iter().any(|m| {
        m.message.role == mirri_core::message::Role::Assistant
            && m.message
                .tool_calls
                .iter()
                .any(|tc| tc.function.name == "Echo")
    });
    assert!(
        has_assistant_echo_call,
        "expected an assistant message with a tool call to Echo"
    );

    // There must be a tool message whose content indicates the call was
    // blocked or denied (the permission gate produced an error result).
    let has_blocked_tool_result = history.iter().any(|m| {
        m.message.role == mirri_core::message::Role::Tool
            && (m.is_error
                || m.message.text_content().to_lowercase().contains("denied")
                || m.message.text_content().to_lowercase().contains("blocked"))
    });
    assert!(
        has_blocked_tool_result,
        "expected a tool result message indicating the call was blocked/denied"
    );
}

#[tokio::test]
async fn should_activate_goal_tracking_when_goal_manager_set_on_turn_flow() {
    let gm = Arc::new(GoalManager::new());
    gm.create("Complete the task".to_string(), None).unwrap();
    // Set a budget of 1 turn so the goal budget is exhausted after step 1.
    // record_step_usage returns Some(true) → forces EndTurn, and
    // should_continue_after_stop sees budget exhausted → stops the turn.
    gm.set_budget(GoalBudget {
        turns: Some(1),
        tokens: None,
        wall_clock_ms: None,
    });

    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());
    turn_flow.set_goal_manager(gm.clone());

    let llm = MockLlm::new(vec![MockResponse::end_turn("Done")]);
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    let result = turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Hello".to_string(),
            }],
            PromptOrigin::User,
            memory,
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(llm))),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("turn should succeed");

    // After 1 step, the goal budget is exhausted (1 >= 1) and the turn stops.
    assert_eq!(result.steps, 1);
    assert_eq!(gm.used_turns(), 1);
}

// ---------------------------------------------------------------------------
// External command hooks — UserPromptSubmit lifecycle event
// ---------------------------------------------------------------------------

/// Register a UserPromptSubmit hook on a fresh engine and wire it into the
/// turn flow.
fn make_prompt_hook_turn_flow(command: &str) -> (TurnFlow, Arc<HookEngine>) {
    let engine = Arc::new(HookEngine::new());
    engine.register(HookDef::new(
        HookEventType::UserPromptSubmit,
        command.to_string(),
    ));
    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());
    turn_flow.set_external_hooks(engine.clone(), HashMap::new());
    (turn_flow, engine)
}

#[tokio::test]
async fn should_fire_user_prompt_submit_hook_when_turn_runs() {
    // Given a turn flow wired with a UserPromptSubmit hook that dumps its
    // JSON input to a marker file
    let marker_path =
        std::env::temp_dir().join(format!("prompt_submit_hook_{}.json", std::process::id()));
    let _ = std::fs::remove_file(&marker_path);

    let (mut turn_flow, _engine) = make_prompt_hook_turn_flow(&format!("cat > {:?}", marker_path));

    let llm = MockLlm::new(vec![MockResponse::end_turn("Hello!")]);
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    // When a user turn runs
    let result = turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Hello".to_string(),
            }],
            PromptOrigin::User,
            memory,
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(llm))),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("turn should succeed");

    // Then the hook fired with the TS input shape { prompt, isSteer }
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    let content = std::fs::read_to_string(&marker_path).expect("hook input file should exist");
    let input: Value = serde_json::from_str(&content).expect("hook input should be JSON");
    assert_eq!(input["isSteer"], serde_json::json!(false));
    assert_eq!(input["prompt"][0]["type"], serde_json::json!("text"));
    assert_eq!(input["prompt"][0]["text"], serde_json::json!("Hello"));
    let _ = std::fs::remove_file(&marker_path);
}

#[tokio::test]
async fn should_not_fire_user_prompt_submit_hook_when_origin_is_not_user() {
    // Given a turn flow wired with a UserPromptSubmit hook that writes a
    // marker file
    let marker_path =
        std::env::temp_dir().join(format!("prompt_submit_no_fire_{}.txt", std::process::id()));
    let _ = std::fs::remove_file(&marker_path);

    let (mut turn_flow, _engine) =
        make_prompt_hook_turn_flow(&format!("echo fired > {:?}", marker_path));

    let llm = MockLlm::new(vec![MockResponse::end_turn("ok")]);
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    // When a non-user-origin prompt runs the turn
    let result = turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "System trigger".to_string(),
            }],
            PromptOrigin::SystemTrigger,
            memory,
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(llm))),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("turn should succeed");

    // Then the hook did not fire and the turn completed normally
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    assert!(
        !marker_path.exists(),
        "UserPromptSubmit hook should not fire for non-user origins"
    );
}

#[tokio::test]
async fn should_skip_turn_when_user_prompt_submit_hook_blocks() {
    // Given a turn flow wired with a blocking UserPromptSubmit hook (exit 2)
    // and an LLM that would fail if called (empty response queue)
    let (mut turn_flow, _engine) = make_prompt_hook_turn_flow("exit 2");

    let llm = MockLlm::new(vec![]);
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    // When a user turn runs
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
        .expect("blocked turn should return a result, not an error");

    // Then the turn is reported as hook-blocked with zero steps — the LLM
    // was never called (proven by the empty mock response queue)
    assert!(result.blocked_by_prompt_hook);
    assert_eq!(result.stop_reason, TurnStopReason::BlockedByHook);
    assert_eq!(result.steps, 0);

    // The user message is still appended so the caller observes the prompt.
    let history = memory.lock().unwrap();
    assert!(
        history
            .history()
            .iter()
            .any(|m| m.message.role == mirri_core::message::Role::User),
        "the user message should remain in memory after a hook block"
    );
}

#[tokio::test]
async fn should_inject_additional_context_before_user_prompt_when_hook_returns_it() {
    // Given a turn flow wired with a UserPromptSubmit hook that returns
    // additionalContext as JSON on stdout
    let command = r#"echo '{"additionalContext":"remember: task XYZ is urgent"}'"#;
    let (mut turn_flow, _engine) = make_prompt_hook_turn_flow(command);

    let llm = MockLlm::new(vec![MockResponse::end_turn("Got it!")]);
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    // When a user turn runs
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

    // Then the turn completed normally
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);

    // And the hook's output was rendered as a hook_result user message ahead
    // of the additionalContext and the real prompt (the TS append order),
    // with the stdout fallback as the display message
    let history = memory.lock().unwrap();
    let messages = history.history();
    assert!(
        messages.len() >= 3,
        "should have at least 3 messages: hook result + additionalContext + user prompt"
    );

    // The first message should be the rendered hook result (the raw stdout
    // with no structured message — the TS stdout fallback)
    let first = &messages[0];
    assert_eq!(first.message.role, mirri_core::message::Role::User);
    assert_eq!(
        first.origin,
        Some(PromptOrigin::HookResult),
        "first message should have HookResult origin"
    );
    assert!(
        first.message.text_content().contains("task XYZ is urgent"),
        "rendered hook result should contain the hook's stdout text"
    );

    // The second message should be the injected additional context
    let second = &messages[1];
    assert_eq!(second.message.role, mirri_core::message::Role::User);
    assert_eq!(
        second.origin,
        Some(PromptOrigin::HookAdditionalContext),
        "second message should have HookAdditionalContext origin"
    );

    // The third message should be the real user prompt
    let third = &messages[2];
    assert_eq!(third.message.role, mirri_core::message::Role::User);
    assert_eq!(
        third.origin,
        Some(PromptOrigin::User),
        "third message should have User origin"
    );
    assert_eq!(third.message.text_content(), "Hello");
}

#[tokio::test]
async fn should_render_and_publish_the_hook_result_when_a_passing_hook_outputs_a_message() {
    // Given a turn flow wired with a UserPromptSubmit hook that returns a
    // structured display message, and a subscribed event bus
    let (mut turn_flow, _engine) =
        make_prompt_hook_turn_flow(r#"echo '{"message":"hook says hi"}'"#);
    let event_bus = Arc::new(mirri_core::session::events::EventBus::new(64));
    turn_flow.set_event_bus(Arc::clone(&event_bus));
    let mut events = event_bus.subscribe();

    let llm = MockLlm::new(vec![MockResponse::end_turn("Got it!")]);
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    // When a user turn runs
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

    // Then the turn completed normally
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);

    // And the hook result was rendered into memory as a user message tagged
    // with the hook_result origin, wrapped in the hook_result envelope
    // (scoped so the memory lock drops before the await below)
    {
        let history = memory.lock().unwrap();
        let hook_messages: Vec<_> = history
            .history()
            .iter()
            .filter(|m| m.origin == Some(PromptOrigin::HookResult))
            .collect();
        assert_eq!(hook_messages.len(), 1, "one hook result message expected");
        assert_eq!(
            hook_messages[0].message.role,
            mirri_core::message::Role::User
        );
        assert_eq!(
            hook_messages[0].message.text_content(),
            "<hook_result hook_event=\"UserPromptSubmit\">\nhook says hi\n</hook_result>"
        );
    }

    // And the event bus observed the hook result (no blocked flag). The
    // turn's step brackets may precede it — drain those first.
    let mut event = events.recv().await.expect("hook result event expected");
    while matches!(
        event,
        mirri_core::SessionEvent::TurnStepStarted { .. }
            | mirri_core::SessionEvent::TurnStepCompleted { .. }
    ) {
        event = events.recv().await.expect("hook result event expected");
    }
    assert!(matches!(
        event,
        mirri_core::session::events::SessionEvent::HookResult { .. }
    ));
    match event {
        mirri_core::session::events::SessionEvent::HookResult {
            hook_event,
            content,
            blocked,
        } => {
            assert_eq!(hook_event, "UserPromptSubmit");
            assert_eq!(content, "hook says hi");
            assert!(blocked.is_none());
        }
        _ => unreachable!(),
    }
    // The step's completion bracket may trail the hook result — drain it.
    while let Ok(event) = events.try_recv() {
        assert!(
            matches!(
                event,
                mirri_core::SessionEvent::TurnStepStarted { .. }
                    | mirri_core::SessionEvent::TurnStepCompleted { .. }
            ),
            "no further events expected, got {event:?}"
        );
    }
}

#[tokio::test]
async fn should_render_the_block_message_as_an_assistant_message_and_publish_blocked_when_a_hook_blocks()
 {
    // Given a turn flow wired with a blocking hook (exit 2 with a stderr
    // message) and a subscribed event bus
    let (mut turn_flow, _engine) =
        make_prompt_hook_turn_flow("echo 'you shall not pass' >&2; exit 2");
    let event_bus = Arc::new(mirri_core::session::events::EventBus::new(64));
    turn_flow.set_event_bus(Arc::clone(&event_bus));
    let mut events = event_bus.subscribe();

    let llm = MockLlm::new(vec![]);
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    // When a user turn runs
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
        .expect("blocked turn should return a result, not an error");

    // Then the turn is hook-blocked
    assert!(result.blocked_by_prompt_hook);
    assert_eq!(result.stop_reason, TurnStopReason::BlockedByHook);

    // And the block message was rendered as an assistant message tagged
    // with the hook_result origin (scoped so the memory lock drops before
    // the await below)
    {
        let history = memory.lock().unwrap();
        let hook_messages: Vec<_> = history
            .history()
            .iter()
            .filter(|m| m.origin == Some(PromptOrigin::HookResult))
            .collect();
        assert_eq!(hook_messages.len(), 1, "one hook result message expected");
        assert_eq!(
            hook_messages[0].message.role,
            mirri_core::message::Role::Assistant
        );
        assert_eq!(
            hook_messages[0].message.text_content(),
            "<hook_result hook_event=\"UserPromptSubmit\">\nyou shall not pass\n</hook_result>"
        );
    }

    // And the event bus observed the blocked hook result
    let event = events.recv().await.expect("hook result event expected");
    match event {
        mirri_core::session::events::SessionEvent::HookResult {
            hook_event,
            content,
            blocked,
        } => {
            assert_eq!(hook_event, "UserPromptSubmit");
            assert_eq!(content, "you shall not pass");
            assert_eq!(blocked, Some(true));
        }
        _ => unreachable!(),
    }
    assert!(events.try_recv().is_err(), "no further events expected");
}

#[tokio::test]
async fn should_prefer_the_deny_message_over_the_reason_and_trim_when_a_hook_denies_with_both() {
    // Given a structured deny carrying a padded top-level message AND a
    // decision reason (distinct values, so a precedence swap or a missing
    // trim fails the assertions) and a subscribed event bus
    let command = r#"echo '{"message":"  stop it  ","hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"policy-reason"}}'"#;
    let (mut turn_flow, _engine) = make_prompt_hook_turn_flow(command);
    let event_bus = Arc::new(mirri_core::session::events::EventBus::new(64));
    turn_flow.set_event_bus(Arc::clone(&event_bus));
    let mut events = event_bus.subscribe();

    let llm = MockLlm::new(vec![]);
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    // When a user turn runs
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
        .expect("blocked turn should return a result, not an error");

    // Then the turn is hook-blocked
    assert!(result.blocked_by_prompt_hook);

    // And the rendered content is the trimmed deny message (not the reason)
    let event = events.recv().await.expect("hook result event expected");
    match event {
        mirri_core::session::events::SessionEvent::HookResult { content, .. } => {
            assert_eq!(content, "stop it");
        }
        _ => unreachable!(),
    }
    assert!(events.try_recv().is_err(), "no further events expected");
}

#[tokio::test]
async fn should_fall_back_to_the_default_block_message_when_a_blocking_hook_is_silent() {
    // Given a turn flow wired with a blocking hook that produces no stderr
    // (no message, no reason) and a subscribed event bus
    let (mut turn_flow, _engine) = make_prompt_hook_turn_flow("exit 2");
    let event_bus = Arc::new(mirri_core::session::events::EventBus::new(64));
    turn_flow.set_event_bus(Arc::clone(&event_bus));
    let mut events = event_bus.subscribe();

    let llm = MockLlm::new(vec![]);
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    // When a user turn runs
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
        .expect("blocked turn should return a result, not an error");

    // Then the turn is hook-blocked
    assert!(result.blocked_by_prompt_hook);

    // And the block message falls back to the TS default text
    let event = events.recv().await.expect("hook result event expected");
    match event {
        mirri_core::session::events::SessionEvent::HookResult { content, .. } => {
            assert_eq!(content, "Blocked by UserPromptSubmit hook");
        }
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn should_not_inject_additional_context_when_hook_returns_empty_context() {
    // Given a turn flow wired with a UserPromptSubmit hook that produces no
    // output at all (a silent exit 0)
    let (mut turn_flow, _engine) = make_prompt_hook_turn_flow("true");

    let llm = MockLlm::new(vec![MockResponse::end_turn("Hello!")]);
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    // When a user turn runs
    turn_flow
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

    // Then only one user message exists — no additional context injected
    let history = memory.lock().unwrap();
    let user_messages: Vec<_> = history
        .history()
        .iter()
        .filter(|m| m.message.role == mirri_core::message::Role::User)
        .collect();
    assert_eq!(
        user_messages.len(),
        1,
        "should have exactly one user message when no additionalContext"
    );
    assert_eq!(user_messages[0].origin, Some(PromptOrigin::User));
}

#[tokio::test]
async fn should_inject_additional_context_and_block_when_multiple_hooks_disagree() {
    // Given a turn flow with two UserPromptSubmit hooks: hook A passes with
    // additionalContext, hook B blocks (exit 2)
    let engine = Arc::new(HookEngine::new());
    engine.register(HookDef::new(
        HookEventType::UserPromptSubmit,
        r#"echo '{"additionalContext":"ctx-from-hook-A"}'"#,
    ));
    engine.register(HookDef::new(HookEventType::UserPromptSubmit, "exit 2"));
    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());
    turn_flow.set_external_hooks(engine, HashMap::new());

    let llm = MockLlm::new(vec![]);
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    // When a user turn runs
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
        .expect("blocked turn should return a result, not an error");

    // Then the turn is blocked but the additionalContext from hook A was
    // still injected before the user prompt
    assert!(result.blocked_by_prompt_hook);
    assert_eq!(result.stop_reason, TurnStopReason::BlockedByHook);

    let history = memory.lock().unwrap();
    let messages = history.history();
    // Should have: [HookAdditionalContext, User, HookResult(assistant block)]
    assert_eq!(messages.len(), 3);
    assert_eq!(
        messages[0].origin,
        Some(PromptOrigin::HookAdditionalContext),
        "first message should be the injected additional context"
    );
    assert!(
        messages[0]
            .message
            .text_content()
            .contains("ctx-from-hook-A"),
        "injected context should contain hook A's text"
    );
    assert_eq!(messages[1].origin, Some(PromptOrigin::User));
    // The block branch renders hook B's message as an assistant message
    // with the default text (hook B was silent) after the consumed prompt.
    assert_eq!(messages[2].origin, Some(PromptOrigin::HookResult));
    assert_eq!(
        messages[2].message.role,
        mirri_core::message::Role::Assistant,
        "the block message should render as an assistant message"
    );
    assert!(
        messages[2].message.text_content().contains("hook_result"),
        "the block message should carry the hook_result envelope"
    );
}

#[tokio::test]
async fn should_skip_hook_additional_context_when_undoing() {
    // Given a completed turn where additionalContext was injected before the
    // user prompt
    let (mut turn_flow, _engine) =
        make_prompt_hook_turn_flow(r#"echo '{"additionalContext":"ephemeral-context"}'"#);

    let llm = MockLlm::new(vec![MockResponse::end_turn("ok")]);
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    turn_flow
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

    // History is: [HookResult, HookAdditionalContext, User, Assistant]
    {
        let history = memory.lock().unwrap();
        assert_eq!(history.history().len(), 4);
    }

    // When undoing 1 turn
    let undo_result = memory.lock().unwrap().undo_count(1);
    assert!(undo_result.is_ok(), "undo should succeed");

    // Then the HookAdditionalContext message is also removed (it's ephemeral
    // and skipped by the undo traversal), leaving an empty history
    let history = memory.lock().unwrap();
    assert!(
        history.history().is_empty(),
        "undo should remove the user prompt and its preceding ephemeral context, got {} messages",
        history.history().len()
    );
}

// ---------------------------------------------------------------------------
// Tool result truncation
// ---------------------------------------------------------------------------

/// A tool that returns a large string output exceeding MAX_CHARS.
struct BigOutputTool;

impl Tool for BigOutputTool {
    fn name(&self) -> &str {
        "BigOutput"
    }

    fn description(&self) -> &str {
        "Returns a large output for truncation testing"
    }

    fn parameters(&self) -> &Value {
        static SCHEMA: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {}
            })
        })
    }

    fn resolve_execution(&self, _input: Value) -> EngineResult<ToolExecution> {
        Ok(ToolExecution::Runnable {
            plan: ExecutionPlan {
                accesses: Vec::new(),
                display: ToolInputDisplay::Generic {
                    summary: "BigOutput".to_string(),
                },
                approval_rule: "BigOutput".to_string(),
                stop_batch_after_this: false,
            },
            execute: Box::new(|_ctx| {
                Box::pin(async move {
                    ToolResult::success(
                        "x".repeat(mirri_core::tool_result_truncation::MAX_CHARS + 1),
                    )
                })
            }),
        })
    }
}

#[tokio::test]
async fn should_truncate_oversized_tool_result_when_truncator_is_configured() {
    // Given a turn flow with a tool result truncator wired to a temp dir
    let temp_dir = tempfile::tempdir().unwrap();
    let truncator = mirri_core::tool_result_truncation::ToolResultTruncator::new(temp_dir.path());

    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![make_tool_call(
            "call-1",
            "BigOutput",
            serde_json::json!({}),
        )]),
        MockResponse::end_turn("Done!"),
    ]);
    let mut tools = ToolManager::new();
    tools.set_active_tools(vec!["BigOutput".to_string()]);
    tools.register_builtin(Arc::new(BigOutputTool));
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());
    turn_flow.set_tool_result_truncator(Some(truncator));

    // When a turn runs with a tool that produces oversized output
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

    // Then the turn completed normally
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);

    // And the tool result in context memory was truncated — it should
    // contain the truncation notice, not the full oversized output
    let history = memory.lock().unwrap();
    let tool_results: Vec<_> = history
        .history()
        .iter()
        .filter(|m| m.message.role == mirri_core::message::Role::Tool)
        .collect();
    assert!(
        !tool_results.is_empty(),
        "should have at least one tool result"
    );
    let tool_text = tool_results[0].message.text_content();
    assert!(
        tool_text.contains("Tool output exceeded 50000 characters"),
        "tool result should be truncated, got: {} chars",
        tool_text.len()
    );
    assert!(
        tool_text.contains("output_path:"),
        "truncated result should contain output_path"
    );
    assert!(
        tool_text.len() < mirri_core::tool_result_truncation::MAX_CHARS,
        "truncated result should be smaller than MAX_CHARS"
    );
}

#[tokio::test]
async fn should_not_truncate_tool_result_when_no_truncator_configured() {
    // Given a turn flow with NO truncator wired
    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![make_tool_call(
            "call-1",
            "BigOutput",
            serde_json::json!({}),
        )]),
        MockResponse::end_turn("Done!"),
    ]);
    let mut tools = ToolManager::new();
    tools.set_active_tools(vec!["BigOutput".to_string()]);
    tools.register_builtin(Arc::new(BigOutputTool));
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());
    // No truncator set — oversized output passes through unchanged

    // When a turn runs with a tool that produces oversized output
    turn_flow
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

    // Then the full oversized output is in context memory (no truncation)
    let history = memory.lock().unwrap();
    let tool_results: Vec<_> = history
        .history()
        .iter()
        .filter(|m| m.message.role == mirri_core::message::Role::Tool)
        .collect();
    assert!(!tool_results.is_empty());
    let tool_text = tool_results[0].message.text_content();
    assert_eq!(
        tool_text.len(),
        mirri_core::tool_result_truncation::MAX_CHARS + 1,
        "tool result should be full size when no truncator is configured"
    );
}

#[tokio::test]
async fn should_preserve_truncation_when_tool_call_is_duplicated_by_dedup() {
    // Given a turn flow with a truncator and a tool that produces oversized output
    let temp_dir = tempfile::tempdir().unwrap();
    let truncator = mirri_core::tool_result_truncation::ToolResultTruncator::new(temp_dir.path());

    let big_output = "x".repeat(mirri_core::tool_result_truncation::MAX_CHARS + 1);
    let llm = MockLlm::new(vec![
        // First step: call BigOutput
        MockResponse::tool_use(vec![make_tool_call(
            "call-1",
            "BigOutput",
            serde_json::json!({}),
        )]),
        // Second step: call BigOutput again with the same args (dedup should fire)
        MockResponse::tool_use(vec![make_tool_call(
            "call-2",
            "BigOutput",
            serde_json::json!({}),
        )]),
        // Third step: end turn
        MockResponse::end_turn("Done!"),
    ]);
    let mut tools = ToolManager::new();
    tools.set_active_tools(vec!["BigOutput".to_string()]);
    tools.register_builtin(Arc::new(BigOutputTool));
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());
    turn_flow.set_tool_result_truncator(Some(truncator));

    // When a turn runs with two identical oversized tool calls
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

    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);

    // Then both tool results in context memory should be truncated
    let history = memory.lock().unwrap();
    let tool_results: Vec<_> = history
        .history()
        .iter()
        .filter(|m| m.message.role == mirri_core::message::Role::Tool)
        .collect();
    assert_eq!(
        tool_results.len(),
        2,
        "should have two tool results (original + dedup)"
    );

    // The dedup result should also be truncated, not the full oversized output
    for (i, msg) in tool_results.iter().enumerate() {
        let text = msg.message.text_content();
        assert!(
            text.len() < mirri_core::tool_result_truncation::MAX_CHARS,
            "tool result #{i} should be truncated (len={}), not full size ({})",
            text.len(),
            big_output.len()
        );
    }
}

// ---------------------------------------------------------------------------
// Wire-op recording (4.3)
// ---------------------------------------------------------------------------

/// A [`WireOpRecorder`] test double that collects every op into a shared
/// vector, in dispatch order.
struct CollectingWireRecorder(Arc<Mutex<Vec<WireOp>>>);

impl WireOpRecorder for CollectingWireRecorder {
    fn record_op<'a>(&'a self, op: WireOp) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        let collected = Arc::clone(&self.0);
        Box::pin(async move {
            collected.lock().unwrap_or_else(|e| e.into_inner()).push(op);
        })
    }
}

/// The `type` discriminator of a collected loop-event op (for order
/// assertions).
fn wire_op_type(op: &WireOp) -> &'static str {
    match op {
        WireOp::ContextAppendLoopEvent { event } => {
            match event.get("type").and_then(serde_json::Value::as_str) {
                Some("step.begin") => "step.begin",
                Some("content.part") => "content.part",
                Some("think.part") => "think.part",
                Some("tool.call") => "tool.call",
                Some("tool.result") => "tool.result",
                Some("step.end") => "step.end",
                other => panic!("unexpected loop event type: {other:?}"),
            }
        }
        WireOp::UsageRecord { .. } => "usage.record",
        other => panic!("unexpected op recorded by the loop: {other:?}"),
    }
}

#[tokio::test]
async fn should_record_loop_events_and_usage_when_wire_recorder_installed() {
    // Given a turn flow with a wire recorder installed and a scripted LLM
    let llm = MockLlm::new(vec![MockResponse::end_turn("Hello!").with_usage(100, 50)]);
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    let recorded = Arc::new(Mutex::new(Vec::new()));
    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());
    turn_flow.set_wire_recorder(Some(
        Arc::new(CollectingWireRecorder(Arc::clone(&recorded))) as Arc<dyn WireOpRecorder>,
    ));

    // When a turn runs
    turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Hello".to_string(),
            }],
            PromptOrigin::User,
            memory,
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(llm))),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("turn should succeed");

    // Then the recorder saw the step lifecycle (step.begin → content.part →
    // step.end) interleaved with the step's usage record, in dispatch order
    let ops = recorded.lock().unwrap_or_else(|e| e.into_inner());
    let types: Vec<&str> = ops.iter().map(wire_op_type).collect();
    assert_eq!(
        types,
        vec!["step.begin", "usage.record", "content.part", "step.end",],
        "loop events must be recorded via context.append_loop_event in order, with usage interleaved"
    );

    // The usage record carries the model name and the step's token counts
    match &ops[1] {
        WireOp::UsageRecord {
            model,
            usage,
            usage_scope,
        } => {
            assert_eq!(model, "test-model");
            assert_eq!(usage["inputTokens"], 100);
            assert_eq!(usage["outputTokens"], 50);
            assert_eq!(*usage_scope, None);
        }
        other => panic!("expected usage record, got {other:?}"),
    }

    // The loop events carry the TS wire shapes (uuid-based step ids)
    match &ops[0] {
        WireOp::ContextAppendLoopEvent { event } => {
            assert_eq!(event["type"], "step.begin");
            assert!(event["uuid"].is_string());
        }
        other => panic!("expected loop event, got {other:?}"),
    }
    match &ops[2] {
        WireOp::ContextAppendLoopEvent { event } => {
            assert_eq!(event["type"], "content.part");
            assert_eq!(event["part"]["text"], "Hello!");
        }
        other => panic!("expected loop event, got {other:?}"),
    }
}

/// Given a turn flow with a wire recorder, a goal manager carrying an active
/// goal, and a scripted LLM step, When the turn runs, Then the step's usage
/// accounting records a usage-only `goal.update` op (turnsUsed/tokensUsed
/// absolute totals, no status or budget change) ahead of the in-memory
/// counter mutation, and no goal op is recorded when no goal is active.
#[tokio::test]
async fn should_record_usage_only_goal_update_when_a_goal_is_active() {
    // Given a turn flow with a wire recorder, an active budgeted goal, and a
    // scripted LLM step reporting 100 input + 50 output tokens
    let llm = MockLlm::new(vec![MockResponse::end_turn("Hello!").with_usage(100, 50)]);
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let gm = Arc::new(GoalManager::new());
    gm.create("Track usage".to_string(), None).unwrap();
    // A 1-turn budget makes the single scripted step exhaust the budget, so
    // the turn ends after one LLM call (an active goal with remaining
    // budget would continue the turn and demand more scripted responses).
    gm.set_budget(GoalBudget {
        turns: Some(1),
        tokens: Some(100_000),
        wall_clock_ms: None,
    });

    let recorded = Arc::new(Mutex::new(Vec::new()));
    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());
    turn_flow.set_wire_recorder(Some(
        Arc::new(CollectingWireRecorder(Arc::clone(&recorded))) as Arc<dyn WireOpRecorder>,
    ));
    turn_flow.set_goal_manager(Arc::clone(&gm));

    // When a turn runs
    turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Hello".to_string(),
            }],
            PromptOrigin::User,
            memory,
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(llm))),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("turn should succeed");

    // Then a usage-only goal.update op was recorded alongside the step's
    // usage.record, carrying the absolute totals (1 turn, 150 tokens) —
    // followed by the blocked transition (the 1-turn budget is exhausted
    // by the same step; the goal goes blocked with the hard-budget reason,
    // no usage totals on that op).
    let ops = recorded.lock().unwrap_or_else(|e| e.into_inner());
    let goal_updates: Vec<&WireOp> = ops
        .iter()
        .filter(|op| matches!(op, WireOp::GoalUpdate { .. }))
        .collect();
    assert_eq!(
        goal_updates.len(),
        2,
        "usage-only op + blocked transition per exhausted step: {ops:?}"
    );
    match goal_updates[0] {
        WireOp::GoalUpdate {
            status,
            turns_used,
            tokens_used,
            budget_limits,
            actor,
            ..
        } => {
            assert_eq!(*status, None, "usage-only: no status change");
            assert_eq!(*turns_used, Some(1));
            assert_eq!(*tokens_used, Some(150));
            assert_eq!(*budget_limits, None, "usage-only: no budget change");
            assert_eq!(*actor, Some("runtime".to_string()));
        }
        other => panic!("expected goal.update, got {other:?}"),
    }
    match goal_updates[1] {
        WireOp::GoalUpdate {
            status,
            reason,
            turns_used,
            tokens_used,
            budget_limits,
            actor,
            ..
        } => {
            assert_eq!(*status, Some("blocked".to_string()));
            assert_eq!(reason.as_deref(), Some("hard budget reached"));
            assert_eq!(*turns_used, None, "blocked op carries no usage totals");
            assert_eq!(*tokens_used, None, "blocked op carries no usage totals");
            assert_eq!(*budget_limits, None, "blocked op carries no budget change");
            assert_eq!(actor.as_deref(), Some("runtime"));
        }
        other => panic!("expected goal.update, got {other:?}"),
    }
    // The in-memory counters advanced to the same totals.
    assert_eq!(gm.used_turns(), 1);
    assert_eq!(gm.used_tokens(), 150);
}

/// Given a turn flow with a wire recorder but no active goal, When a turn
/// runs, Then no goal.update op is recorded (the reducer would drop it and
/// the log would only accrue noise).
#[tokio::test]
async fn should_not_record_goal_update_when_no_goal_is_active() {
    let llm = MockLlm::new(vec![MockResponse::end_turn("Hello!")]);
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let gm = Arc::new(GoalManager::new());

    let recorded = Arc::new(Mutex::new(Vec::new()));
    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());
    turn_flow.set_wire_recorder(Some(
        Arc::new(CollectingWireRecorder(Arc::clone(&recorded))) as Arc<dyn WireOpRecorder>,
    ));
    turn_flow.set_goal_manager(Arc::clone(&gm));

    turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Hello".to_string(),
            }],
            PromptOrigin::User,
            memory,
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(llm))),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("turn should succeed");

    let ops = recorded.lock().unwrap_or_else(|e| e.into_inner());
    assert!(
        !ops.iter().any(|op| matches!(op, WireOp::GoalUpdate { .. })),
        "no goal.update expected without an active goal: {ops:?}"
    );
}

// ---------------------------------------------------------------------------
// Goal hard-budget stop (T9): blocked transition + one grace step + reminder
// ---------------------------------------------------------------------------

/// Collect the indices (into the conversation history) of the messages
/// carrying the goal budget stop reminder.
fn budget_reminder_positions(history: &[mirri_core::context::ContextMessage]) -> Vec<usize> {
    history
        .iter()
        .enumerate()
        .filter(|(_, m)| m.message.text_content().contains(GOAL_BUDGET_STOP_REMINDER))
        .map(|(i, _)| i)
        .collect()
}

/// Count the durable `context.append_message` ops carrying the goal budget
/// stop reminder in a collected wire-op log.
fn budget_reminder_wire_ops(ops: &[WireOp]) -> Vec<&WireOp> {
    ops.iter()
        .filter(|op| match op {
            WireOp::ContextAppendMessage { message } => message["content"][0]["text"]
                .as_str()
                .is_some_and(|t| t.contains(GOAL_BUDGET_STOP_REMINDER)),
            _ => false,
        })
        .collect()
}

/// Count the `goal.update` ops marking the goal blocked for the hard budget.
fn budget_blocked_goal_updates(ops: &[WireOp]) -> Vec<&WireOp> {
    ops.iter()
        .filter(|op| match op {
            WireOp::GoalUpdate { status, reason, .. } => {
                status.as_deref() == Some("blocked")
                    && reason.as_deref() == Some("hard budget reached")
            }
            _ => false,
        })
        .collect()
}

/// Given a turn flow with an active goal carrying a 1-turn budget and a
/// scripted LLM step that ends naturally in tool calls, When the hard budget
/// is hit, Then the goal is marked blocked (in memory and on the wire), one
/// grace step is granted so the turn continues to a second LLM request where
/// the model writes its final message, and the budget stop reminder is
/// appended to memory exactly once — after the exhausted step's tool
/// results, before the final assistant message — with its durable
/// `context.append_message` wire op.
#[tokio::test]
async fn should_grant_one_grace_step_with_budget_stop_reminder_when_budget_hits_with_tool_calls() {
    // Given an active goal with a 1-turn budget, a wire recorder, and a
    // scripted LLM whose first step ends in a tool call
    let gm = Arc::new(GoalManager::new());
    gm.create("Do the thing".to_string(), None).unwrap();
    gm.set_budget(GoalBudget {
        turns: Some(1),
        tokens: None,
        wall_clock_ms: None,
    });
    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![make_tool_call(
            "call-1",
            "Echo",
            serde_json::json!({"message": "hi"}),
        )]),
        MockResponse::end_turn("Final status: one tool call done"),
    ]);
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());
    turn_flow.set_wire_recorder(Some(
        Arc::new(CollectingWireRecorder(Arc::clone(&recorded))) as Arc<dyn WireOpRecorder>,
    ));
    turn_flow.set_goal_manager(Arc::clone(&gm));

    // When the turn runs and the first step exhausts the budget
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

    // Then the turn was NOT cut short at the exhausted step: the grace step
    // let the model write its final message on a second LLM request.
    assert_eq!(result.steps, 2, "grace step should run one more LLM call");
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);

    // And the goal was marked blocked exactly once (idempotent transition).
    let goal = gm.get().expect("goal still exists");
    assert_eq!(goal.status, GoalStatus::Blocked);
    assert_eq!(goal.terminal_reason.as_deref(), Some("hard budget reached"));

    // And the reminder was injected exactly once, after the exhausted
    // step's tool results and before the final assistant message.
    let history = {
        let guard = memory.lock().unwrap();
        guard.history().to_vec()
    };
    let positions = budget_reminder_positions(&history);
    assert_eq!(
        positions.len(),
        1,
        "reminder injected exactly once, got {positions:?} in {history:?}"
    );
    let reminder_idx = positions[0];
    let tool_result_idx = history
        .iter()
        .position(|m| m.message.role == mirri_core::message::Role::Tool)
        .expect("tool result message present");
    assert!(
        reminder_idx > tool_result_idx,
        "reminder must land after the exhausted step's tool results"
    );
    assert_eq!(
        history[reminder_idx + 1].message.role,
        mirri_core::message::Role::Assistant,
        "the final assistant message follows the reminder"
    );

    // And the wire log carries the blocked transition plus the durable
    // reminder append.
    let ops = recorded.lock().unwrap_or_else(|e| e.into_inner());
    let blocked = budget_blocked_goal_updates(&ops);
    assert_eq!(blocked.len(), 1, "one blocked goal.update op: {ops:?}");
    match blocked[0] {
        WireOp::GoalUpdate {
            status,
            reason,
            turns_used,
            tokens_used,
            budget_limits,
            actor,
            ..
        } => {
            assert_eq!(status.as_deref(), Some("blocked"));
            assert_eq!(reason.as_deref(), Some("hard budget reached"));
            assert_eq!(*turns_used, None, "blocked op carries no usage totals");
            assert_eq!(*tokens_used, None, "blocked op carries no usage totals");
            assert_eq!(*budget_limits, None, "blocked op carries no budget change");
            assert_eq!(actor.as_deref(), Some("runtime"));
        }
        other => panic!("expected goal.update, got {other:?}"),
    }
    let reminder_ops = budget_reminder_wire_ops(&ops);
    assert_eq!(
        reminder_ops.len(),
        1,
        "one durable context.append_message for the reminder: {ops:?}"
    );
}

/// Given a goal whose 1-turn budget was already hit and graced once, When the
/// post-grace step again ends in tool calls with the budget still exhausted,
/// Then the turn is force-stopped on that step (no third LLM request), the
/// goal stays blocked with no second blocked transition, and no second
/// budget stop reminder is injected.
#[tokio::test]
async fn should_force_stop_without_second_reminder_when_budget_hits_again_after_grace() {
    // Given an active goal with a 1-turn budget, a wire recorder, and a
    // scripted LLM that keeps ending in tool calls after the grace step
    let gm = Arc::new(GoalManager::new());
    gm.create("Do the thing".to_string(), None).unwrap();
    gm.set_budget(GoalBudget {
        turns: Some(1),
        tokens: None,
        wall_clock_ms: None,
    });
    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![make_tool_call(
            "call-1",
            "Echo",
            serde_json::json!({"message": "hi"}),
        )]),
        MockResponse::tool_use(vec![make_tool_call(
            "call-2",
            "Echo",
            serde_json::json!({"message": "still working"}),
        )]),
        // Never consumed: the second exhaustion force-stops the turn.
        MockResponse::end_turn("Unreachable"),
    ]);
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());
    turn_flow.set_wire_recorder(Some(
        Arc::new(CollectingWireRecorder(Arc::clone(&recorded))) as Arc<dyn WireOpRecorder>,
    ));
    turn_flow.set_goal_manager(Arc::clone(&gm));

    // When the turn runs
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

    // Then the turn was force-stopped on the post-grace step: no third
    // LLM request was made.
    assert_eq!(result.steps, 2, "second exhaustion stops immediately");
    let goal = gm.get().expect("goal still exists");
    assert_eq!(goal.status, GoalStatus::Blocked);

    // And no second reminder was injected.
    let history = {
        let guard = memory.lock().unwrap();
        guard.history().to_vec()
    };
    let positions = budget_reminder_positions(&history);
    assert_eq!(
        positions.len(),
        1,
        "no second reminder after the grace was spent: {history:?}"
    );

    // And the wire log shows one blocked transition and one reminder
    // append (both idempotent across the second hit).
    let ops = recorded.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(
        budget_blocked_goal_updates(&ops).len(),
        1,
        "blocked transition is idempotent: {ops:?}"
    );
    assert_eq!(
        budget_reminder_wire_ops(&ops).len(),
        1,
        "no second durable reminder append: {ops:?}"
    );
}

/// Given a goal whose 1-turn budget was hit and graced in an earlier turn
/// (the goal is now blocked with the hard-budget reason), When the user
/// sends a new message under that blocked goal and the turn runs with the
/// budget still exhausted, Then the turn hard-stops on its first step (no
/// re-granted grace step, no extra LLM call), no new budget stop reminder is
/// injected, and no second blocked `goal.update` op is recorded.
#[tokio::test]
async fn should_stop_immediately_without_re_granted_grace_when_a_new_turn_runs_under_a_blocked_goal()
 {
    // Given an active goal with a 1-turn budget, a wire recorder, and a
    // scripted LLM whose first step ends in a tool call — turn 1 will hit
    // the budget, get blocked, and spend its grace step.
    let gm = Arc::new(GoalManager::new());
    gm.create("Do the thing".to_string(), None).unwrap();
    gm.set_budget(GoalBudget {
        turns: Some(1),
        tokens: None,
        wall_clock_ms: None,
    });
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());
    turn_flow.set_wire_recorder(Some(
        Arc::new(CollectingWireRecorder(Arc::clone(&recorded))) as Arc<dyn WireOpRecorder>,
    ));
    turn_flow.set_goal_manager(Arc::clone(&gm));

    // When turn 1 runs: the first step exhausts the budget in tool calls
    // (blocked + reminder + grace) and the grace step writes the final
    // message.
    let turn1_llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![make_tool_call(
            "call-1",
            "Echo",
            serde_json::json!({"message": "hi"}),
        )]),
        MockResponse::end_turn("Final status: one tool call done"),
    ]);
    let result = turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Hello".to_string(),
            }],
            PromptOrigin::User,
            memory.clone(),
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(turn1_llm))),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("turn 1 should succeed");
    assert_eq!(result.steps, 2, "turn 1: grace step should run");
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    let goal = gm.get().expect("goal still exists");
    assert_eq!(goal.status, GoalStatus::Blocked);
    assert_eq!(goal.terminal_reason.as_deref(), Some("hard budget reached"));

    // And when the user sends a new message under the blocked goal (a fresh
    // manual turn): the model responds with tool calls again, but the budget
    // is still exhausted — the turn must hard-stop on the first step.
    let turn2_llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![make_tool_call(
            "call-2",
            "Echo",
            serde_json::json!({"message": "still trying"}),
        )]),
        // Never consumed: the exhaustion under an already-blocked goal must
        // force the stop with no re-granted grace step.
        MockResponse::end_turn("Unreachable"),
    ]);
    let result = turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Hello again".to_string(),
            }],
            PromptOrigin::User,
            memory.clone(),
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(turn2_llm))),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("turn 2 should succeed");

    // Then the second turn was hard-stopped on its first step: one LLM
    // request, no re-granted grace step.
    assert_eq!(
        result.steps, 1,
        "turn 2 must stop on the first step with no re-granted grace"
    );
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);

    // And the goal stays blocked with the same terminal reason (no
    // duplicate blocked transition in memory or on the wire).
    let goal = gm.get().expect("goal still exists");
    assert_eq!(goal.status, GoalStatus::Blocked);
    assert_eq!(goal.terminal_reason.as_deref(), Some("hard budget reached"));
    let ops = recorded.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(
        budget_blocked_goal_updates(&ops).len(),
        1,
        "no second blocked goal.update op: {ops:?}"
    );

    // And no second budget stop reminder was injected — exactly the one
    // from turn 1, in memory and on the wire.
    let history = {
        let guard = memory.lock().unwrap();
        guard.history().to_vec()
    };
    assert_eq!(
        budget_reminder_positions(&history).len(),
        1,
        "no new reminder in the follow-up turn: {history:?}"
    );
    assert_eq!(
        budget_reminder_wire_ops(&ops).len(),
        1,
        "no new durable reminder append in the follow-up turn: {ops:?}"
    );
}

/// Given an active goal whose 1-turn budget is exhausted by a step that ends
/// naturally (EndTurn), When the turn runs, Then the goal is still marked
/// blocked with the hard-budget reason (memory + wire), but no budget stop
/// reminder is injected — the reminder only rides the tool_calls grace path.
#[tokio::test]
async fn should_mark_goal_blocked_when_budget_reached_even_on_non_tool_stop() {
    // Given an active goal with a 1-turn budget and a scripted LLM whose
    // single step ends naturally
    let gm = Arc::new(GoalManager::new());
    gm.create("Do the thing".to_string(), None).unwrap();
    gm.set_budget(GoalBudget {
        turns: Some(1),
        tokens: None,
        wall_clock_ms: None,
    });
    let llm = MockLlm::new(vec![MockResponse::end_turn("Done")]);
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());
    turn_flow.set_wire_recorder(Some(
        Arc::new(CollectingWireRecorder(Arc::clone(&recorded))) as Arc<dyn WireOpRecorder>,
    ));
    turn_flow.set_goal_manager(Arc::clone(&gm));

    // When the turn runs
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

    // Then the turn ended naturally after the single step.
    assert_eq!(result.steps, 1);
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);

    // And the goal was marked blocked with the hard-budget reason.
    let goal = gm.get().expect("goal still exists");
    assert_eq!(goal.status, GoalStatus::Blocked);
    assert_eq!(goal.terminal_reason.as_deref(), Some("hard budget reached"));

    // And the blocked transition landed on the wire.
    let ops = recorded.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(
        budget_blocked_goal_updates(&ops).len(),
        1,
        "blocked transition recorded: {ops:?}"
    );

    // But no reminder was injected (memory or wire).
    let history = {
        let guard = memory.lock().unwrap();
        guard.history().to_vec()
    };
    assert!(
        budget_reminder_positions(&history).is_empty(),
        "no reminder on the non-tool stop path: {history:?}"
    );
    assert!(
        budget_reminder_wire_ops(&ops).is_empty(),
        "no durable reminder append on the non-tool stop path: {ops:?}"
    );
}

/// Given a turn flow with a wire recorder and a scripted LLM that reports all
/// four usage buckets, When a turn runs, Then both the `usage.record` op and
/// the `step.end` loop event carry the cache buckets on the wire.
#[tokio::test]
async fn should_carry_cache_buckets_when_wire_recorder_installed() {
    // Given a turn flow with a wire recorder installed and a scripted LLM
    // whose step reports cache read and cache creation buckets
    let llm = MockLlm::new(vec![
        MockResponse::end_turn("Hello!").with_full_usage(100, 50, 200, 40),
    ]);
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    let recorded = Arc::new(Mutex::new(Vec::new()));
    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());
    turn_flow.set_wire_recorder(Some(
        Arc::new(CollectingWireRecorder(Arc::clone(&recorded))) as Arc<dyn WireOpRecorder>,
    ));

    // When a turn runs
    turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Hello".to_string(),
            }],
            PromptOrigin::User,
            memory,
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(llm))),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("turn should succeed");

    // Then the usage record op carries all four buckets
    let ops = recorded.lock().unwrap_or_else(|e| e.into_inner());
    let usage_op = ops
        .iter()
        .find_map(|op| match op {
            WireOp::UsageRecord { usage, .. } => Some(usage.clone()),
            _ => None,
        })
        .expect("usage record op present");
    assert_eq!(usage_op["inputTokens"], 100);
    assert_eq!(usage_op["outputTokens"], 50);
    assert_eq!(usage_op["cacheReadTokens"], 200);
    assert_eq!(usage_op["cacheCreationTokens"], 40);

    // And the step.end loop event's usage carries all four buckets
    let step_end = ops
        .iter()
        .find_map(|op| match op {
            WireOp::ContextAppendLoopEvent { event } if event["type"] == "step.end" => {
                Some(event.clone())
            }
            _ => None,
        })
        .expect("step.end loop event present");
    assert_eq!(step_end["usage"]["inputTokens"], 100);
    assert_eq!(step_end["usage"]["outputTokens"], 50);
    assert_eq!(step_end["usage"]["cacheReadTokens"], 200);
    assert_eq!(step_end["usage"]["cacheCreationTokens"], 40);
}

// ---------------------------------------------------------------------------
// StopFailure / Interrupt hook tests
// ---------------------------------------------------------------------------

/// Register hooks that append the triggered event's JSON input (piped to the
/// command's stdin) to `log_path`, prefixed with the event name, so tests can
/// observe which lifecycle events fired on a `TurnFlow`.
fn register_hook_recorders(engine: &HookEngine, log_path: &std::path::Path) {
    engine.register_all(vec![
        HookDef::new(
            HookEventType::StopFailure,
            format!("sed 's/^/STOPFAILURE /' >> '{}'", log_path.display()),
        ),
        HookDef::new(
            HookEventType::Interrupt,
            format!("sed 's/^/INTERRUPT /' >> '{}'", log_path.display()),
        ),
    ]);
}

/// should define StopFailure and Interrupt in HookEventType
#[test]
fn should_serialize_stop_failure_and_interrupt_variants_when_serializing_hook_event_type() {
    // Both variants exist and serialize to the TS `HookEventType` wire names.
    assert_eq!(
        serde_json::to_string(&HookEventType::StopFailure).unwrap(),
        "\"StopFailure\""
    );
    assert_eq!(
        serde_json::to_string(&HookEventType::Interrupt).unwrap(),
        "\"Interrupt\""
    );
}

/// should report user cancellation only when cancel_user is used
#[test]
fn should_report_user_cancellation_only_when_cancel_user_is_used() {
    // A fresh token is not cancelled.
    let cancel = CancelToken::new();
    assert!(!cancel.is_user_cancelled());

    // System cancellation (`cancel`) is an abort, not a user cancellation.
    let cancel = CancelToken::new();
    cancel.cancel();
    assert!(!cancel.is_user_cancelled());

    // `cancel_user` marks the token as user-cancelled — this is the gate for
    // the Interrupt hook.
    let cancel = CancelToken::new();
    cancel.cancel_user();
    assert!(cancel.is_user_cancelled());
}

/// should fire StopFailure when turn fails with non-abort error
#[tokio::test]
async fn should_fire_stop_failure_hook_when_turn_fails_with_non_abort_error() {
    // Given a turn flow whose LLM always fails with a provider error, and a
    // hook engine recording StopFailure events
    let tmp = tempfile::tempdir().unwrap();
    let log_path = tmp.path().join("hooks.log");
    let engine = Arc::new(HookEngine::new());
    register_hook_recorders(&engine, &log_path);
    let llm: Arc<dyn Llm> = Arc::new(DummyLlm);
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());
    turn_flow.set_external_hooks(engine, HashMap::new());

    // When the turn fails with a non-abort error
    let result = turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Hello".to_string(),
            }],
            PromptOrigin::User,
            memory,
            &tools,
            Arc::new(FixedLlmResolver::new(llm)),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await;

    // Then the error propagates and StopFailure fired with the error payload
    assert!(result.is_err());
    let log = std::fs::read_to_string(&log_path).unwrap();
    assert!(
        log.contains("STOPFAILURE "),
        "expected StopFailure in log: {log}"
    );
    assert!(
        log.contains("\"error_type\":\"EngineError\""),
        "expected error_type in log: {log}"
    );
    assert!(
        log.contains("not implemented"),
        "expected error message in log: {log}"
    );
}

/// should fire Interrupt when turn is cancelled by user
#[tokio::test]
async fn should_fire_interrupt_hook_when_turn_is_cancelled_by_user() {
    // Given a turn flow with a recorded Interrupt hook and a user-cancelled
    // token (cancelled before the turn starts, so the loop-top abort check
    // returns Ok(Aborted) with zero steps)
    let tmp = tempfile::tempdir().unwrap();
    let log_path = tmp.path().join("hooks.log");
    let engine = Arc::new(HookEngine::new());
    register_hook_recorders(&engine, &log_path);
    let llm = Arc::new(MockLlm::new(vec![MockResponse::end_turn("Hello")]));
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    cancel.cancel_user();

    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());
    turn_flow.set_external_hooks(engine, HashMap::new());

    // When the turn runs with the user-cancelled token
    let result = turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Hello".to_string(),
            }],
            PromptOrigin::User,
            memory,
            &tools,
            Arc::new(FixedLlmResolver::new(llm)),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("user-cancelled turn should return Ok with Aborted stop reason");

    // Then the turn aborted and Interrupt fired with the turn id and reason
    assert_eq!(result.stop_reason, TurnStopReason::Aborted);
    let log = std::fs::read_to_string(&log_path).unwrap();
    assert!(
        log.contains("INTERRUPT "),
        "expected Interrupt in log: {log}"
    );
    assert!(
        log.contains("\"turn_id\":1"),
        "expected turn_id in log: {log}"
    );
    assert!(
        log.contains("\"reason\":\"cancelled\""),
        "expected reason in log: {log}"
    );
}

/// should not fire Interrupt when turn is aborted programmatically
#[tokio::test]
async fn should_not_fire_interrupt_hook_when_turn_is_aborted_programmatically() {
    // Given a turn flow with a recorded Interrupt hook and a system-cancelled
    // token (`cancel()` — programmatic abort, e.g. scope shutdown/timeout)
    let tmp = tempfile::tempdir().unwrap();
    let log_path = tmp.path().join("hooks.log");
    let engine = Arc::new(HookEngine::new());
    register_hook_recorders(&engine, &log_path);
    let llm = Arc::new(MockLlm::new(vec![MockResponse::end_turn("Hello")]));
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    cancel.cancel();

    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());
    turn_flow.set_external_hooks(engine, HashMap::new());

    // When the turn runs with the programmatically-aborted token
    let result = turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Hello".to_string(),
            }],
            PromptOrigin::User,
            memory,
            &tools,
            Arc::new(FixedLlmResolver::new(llm)),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("aborted turn should return Ok with Aborted stop reason");

    // Then the turn aborted but neither Interrupt nor StopFailure fired.
    // The log file may not exist at all (no hook fired → sed never ran),
    // which is the correct outcome for a programmatic abort.
    assert_eq!(result.stop_reason, TurnStopReason::Aborted);
    let log = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        !log.contains("INTERRUPT"),
        "Interrupt must not fire for programmatic aborts: {log}"
    );
    assert!(
        !log.contains("STOPFAILURE"),
        "StopFailure must not fire for aborts: {log}"
    );
}

/// should deliver the interruption reminder and record the turn.cancel /
/// context.append_message / interruptionReminder.recorded wire ops when the
/// turn is cancelled by the user
#[tokio::test]
async fn should_deliver_interruption_reminder_and_wire_ops_when_turn_cancelled_by_user() {
    // Given a turn flow with a wire recorder installed and a user-cancelled
    // token (cancelled before the turn starts, so the loop-top abort check
    // returns Ok(Aborted) with zero steps)
    let llm = Arc::new(MockLlm::new(vec![MockResponse::end_turn("Hello")]));
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    cancel.cancel_user();

    let recorded = Arc::new(Mutex::new(Vec::new()));
    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());
    turn_flow.set_wire_recorder(Some(
        Arc::new(CollectingWireRecorder(Arc::clone(&recorded))) as Arc<dyn WireOpRecorder>,
    ));

    // When the turn runs with the user-cancelled token
    let result = turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Hello".to_string(),
            }],
            PromptOrigin::User,
            Arc::clone(&memory),
            &tools,
            Arc::new(FixedLlmResolver::new(llm)),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("user-cancelled turn should return Ok with Aborted stop reason");

    // Then the turn aborted and the memory's last message is the
    // interruption reminder wrapped in <system-reminder> with the Injection
    // origin
    assert_eq!(result.stop_reason, TurnStopReason::Aborted);
    {
        let mem = memory.lock().unwrap_or_else(|e| e.into_inner());
        let last = mem.history().last().expect("reminder appended to memory");
        assert_eq!(
            last.origin,
            Some(PromptOrigin::Injection),
            "reminder must carry the Injection origin"
        );
        assert_eq!(
            last.message.text_content(),
            mirri_core::system_reminder_wire::system_reminder_text(
                mirri_core::interruption_reminder::INTERRUPTION_REMINDER_TEXT
            ),
            "reminder text must be the <system-reminder>-wrapped interruption text"
        );
    }

    // And the wire recorded, in order: turn.cancel (target "active", reason
    // "user_cancelled"), the reminder's context.append_message, then
    // interruptionReminder.recorded — all carrying the turn id
    {
        let ops = recorded.lock().unwrap_or_else(|e| e.into_inner());
        let types: Vec<&str> = ops
            .iter()
            .map(|op| match op {
                WireOp::TurnCancel { .. } => "turn.cancel",
                WireOp::ContextAppendMessage { .. } => "context.append_message",
                WireOp::InterruptionReminderRecorded { .. } => "interruptionReminder.recorded",
                other => panic!("unexpected op recorded: {other:?}"),
            })
            .collect();
        assert_eq!(
            types,
            vec![
                "turn.cancel",
                "context.append_message",
                "interruptionReminder.recorded",
            ],
            "user cancellation must record the cancel fact, the reminder append, and the delivery marker, in order"
        );
        match &ops[0] {
            WireOp::TurnCancel {
                turn_id,
                target,
                reason,
            } => {
                assert_eq!(*turn_id, Some(result.turn_id));
                assert_eq!(target.as_deref(), Some("active"));
                assert_eq!(reason.as_deref(), Some("user_cancelled"));
            }
            other => panic!("expected turn.cancel, got {other:?}"),
        }
        match &ops[1] {
            WireOp::ContextAppendMessage { message } => {
                assert_eq!(message["role"], "user");
                assert_eq!(message["origin"]["kind"], "injection");
                assert_eq!(
                    message["content"][0]["text"],
                    mirri_core::system_reminder_wire::system_reminder_text(
                        mirri_core::interruption_reminder::INTERRUPTION_REMINDER_TEXT
                    )
                );
            }
            other => panic!("expected context.append_message, got {other:?}"),
        }
        match &ops[2] {
            WireOp::InterruptionReminderRecorded { turn_id } => {
                assert_eq!(*turn_id, result.turn_id);
            }
            other => panic!("expected interruptionReminder.recorded, got {other:?}"),
        }
    }

    // When a second consecutive user cancellation arrives with no message in
    // between (the retry shape — no new prompt is appended)
    let cancel2 = CancelToken::new();
    cancel2.cancel_user();
    let history_len_before = memory
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .history()
        .len();
    let result2 = turn_flow
        .retry_one_turn(
            Arc::clone(&memory),
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(MockLlm::new(vec![
                MockResponse::end_turn("Hello"),
            ])))),
            &cancel2,
            &mut validators,
            None,
        )
        .await
        .expect("second user-cancelled turn should return Ok with Aborted");

    // Then no second reminder is appended (the last message is still the
    // first reminder — two consecutive user cancellations do not stack
    // reminders), but the turn.cancel and interruptionReminder.recorded ops
    // are still recorded for the second turn
    assert_eq!(result2.stop_reason, TurnStopReason::Aborted);
    {
        let mem = memory.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            mem.history().len(),
            history_len_before,
            "dedup: no second reminder may be appended"
        );
        let last = mem.history().last().expect("history non-empty");
        assert_eq!(
            last.origin,
            Some(PromptOrigin::Injection),
            "last message must still be the first reminder"
        );
    }
    {
        let ops = recorded.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(ops.len(), 5, "second turn records cancel + marker only");
        match &ops[3] {
            WireOp::TurnCancel {
                turn_id,
                target,
                reason,
            } => {
                assert_eq!(*turn_id, Some(result2.turn_id));
                assert_eq!(target.as_deref(), Some("active"));
                assert_eq!(reason.as_deref(), Some("user_cancelled"));
            }
            other => panic!("expected second turn.cancel, got {other:?}"),
        }
        match &ops[4] {
            WireOp::InterruptionReminderRecorded { turn_id } => {
                assert_eq!(*turn_id, result2.turn_id);
            }
            other => panic!("expected second interruptionReminder.recorded, got {other:?}"),
        }
    }
}

/// should not deliver the interruption reminder nor record user-cancel wire
/// ops when the turn is cancelled by the system
#[tokio::test]
async fn should_not_deliver_interruption_reminder_when_cancelled_by_system() {
    // Given a turn flow with a wire recorder installed and a
    // system-cancelled token (`cancel()` — programmatic abort, e.g. scope
    // shutdown/timeout)
    let llm = Arc::new(MockLlm::new(vec![MockResponse::end_turn("Hello")]));
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    cancel.cancel();

    let recorded = Arc::new(Mutex::new(Vec::new()));
    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());
    turn_flow.set_wire_recorder(Some(
        Arc::new(CollectingWireRecorder(Arc::clone(&recorded))) as Arc<dyn WireOpRecorder>,
    ));

    // When the turn runs with the system-cancelled token
    let result = turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Hello".to_string(),
            }],
            PromptOrigin::User,
            Arc::clone(&memory),
            &tools,
            Arc::new(FixedLlmResolver::new(llm)),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("system-cancelled turn should return Ok with Aborted stop reason");

    // Then the turn aborted but no reminder was appended (the memory's last
    // message is still the user prompt) and no wire op was recorded
    assert_eq!(result.stop_reason, TurnStopReason::Aborted);
    {
        let mem = memory.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            mem.history().len(),
            1,
            "only the user prompt may be present"
        );
        let last = mem.history().last().expect("prompt appended");
        assert_eq!(last.origin, Some(PromptOrigin::User));
        assert_eq!(last.message.text_content(), "Hello");
    }
    {
        let ops = recorded.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            ops.is_empty(),
            "system cancellation must not record user-cancel wire ops: {ops:?}"
        );
    }
}

/// A tool whose execution cancels the shared token with user semantics
/// (`cancel_user()`) — simulates the user pressing "stop" mid-turn.
///
/// The token is shared via `Arc` (not a clone) because `CancelToken` records
/// its cancellation reason on the exact instance that was cancelled — a clone
/// would report a system abort instead of a user cancellation.
struct CancellingTool {
    cancel: Arc<CancelToken>,
}

impl Tool for CancellingTool {
    fn name(&self) -> &str {
        "CancelTokenTool"
    }

    fn description(&self) -> &str {
        "Cancels the current turn with user-cancellation semantics"
    }

    fn parameters(&self) -> &Value {
        static SCHEMA: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            })
        })
    }

    fn resolve_execution(&self, _input: Value) -> EngineResult<ToolExecution> {
        let cancel = self.cancel.clone();
        Ok(ToolExecution::Runnable {
            plan: ExecutionPlan {
                accesses: Vec::new(),
                display: ToolInputDisplay::Generic {
                    summary: "CancelTokenTool".to_string(),
                },
                approval_rule: "CancelTokenTool".to_string(),
                stop_batch_after_this: false,
            },
            execute: Box::new(move |_ctx| {
                let cancel = cancel.clone();
                Box::pin(async move {
                    cancel.cancel_user();
                    ToolResult::success("cancelled")
                })
            }),
        })
    }
}

/// should fire Interrupt when user cancels mid-turn (Ok(Aborted) arm)
#[tokio::test]
async fn should_fire_interrupt_hook_when_user_cancels_mid_turn() {
    // Given a turn flow with a recorded Interrupt hook, and a tool that
    // cancels the token (user semantics) during execution — the cancellation
    // is observed by `run_turn`'s loop-top check, which returns
    // Ok(Aborted) with the partial step (the Ok arm of run_one_turn).
    // The token is shared via `Arc` so the tool cancels the SAME instance
    // that `run_one_turn` holds (as a real user "stop" would).
    let tmp = tempfile::tempdir().unwrap();
    let log_path = tmp.path().join("hooks.log");
    let engine = Arc::new(HookEngine::new());
    register_hook_recorders(&engine, &log_path);
    let llm = Arc::new(MockLlm::new(vec![MockResponse::tool_use(vec![
        make_tool_call("call-1", "CancelTokenTool", serde_json::json!({})),
    ])]));
    let cancel = Arc::new(CancelToken::new());
    let mut tm = ToolManager::new();
    tm.set_active_tools(vec!["CancelTokenTool".to_string()]);
    tm.register_builtin(Arc::new(CancellingTool {
        cancel: Arc::clone(&cancel),
    }));
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));

    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());
    turn_flow.set_external_hooks(engine, HashMap::new());

    // When the turn runs and the tool cancels it mid-step
    let result = turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Hello".to_string(),
            }],
            PromptOrigin::User,
            memory,
            &tm,
            Arc::new(FixedLlmResolver::new(llm)),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("user-cancelled turn should return Ok with Aborted stop reason");

    // Then the turn aborted with the accumulated step and Interrupt fired
    // from the Ok(Aborted) arm
    assert_eq!(result.stop_reason, TurnStopReason::Aborted);
    assert_eq!(result.steps, 1);
    let log = std::fs::read_to_string(&log_path).unwrap();
    assert!(
        log.contains("INTERRUPT "),
        "expected Interrupt in log: {log}"
    );
    assert!(
        log.contains("\"turn_id\":1"),
        "expected turn_id in log: {log}"
    );
    assert!(
        log.contains("\"reason\":\"cancelled\""),
        "expected reason in log: {log}"
    );
}

/// should deliver the interruption reminder and record the user-cancel wire
/// ops when the user cancels mid-turn (Ok(Aborted) arm)
#[tokio::test]
async fn should_deliver_interruption_reminder_and_wire_ops_when_user_cancels_mid_turn() {
    // Given a turn flow with a wire recorder installed, and a tool that
    // cancels the token (user semantics) during execution — the
    // cancellation is observed by `run_turn`'s loop-top check, which
    // returns Ok(Aborted) with the partial step (the Ok arm of
    // run_one_turn, the production "user presses stop" shape). The token is
    // shared via `Arc` so the tool cancels the SAME instance that
    // `run_one_turn` holds.
    let llm = Arc::new(MockLlm::new(vec![MockResponse::tool_use(vec![
        make_tool_call("call-1", "CancelTokenTool", serde_json::json!({})),
    ])]));
    let cancel = Arc::new(CancelToken::new());
    let mut tm = ToolManager::new();
    tm.set_active_tools(vec!["CancelTokenTool".to_string()]);
    tm.register_builtin(Arc::new(CancellingTool {
        cancel: Arc::clone(&cancel),
    }));
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));

    let recorded = Arc::new(Mutex::new(Vec::new()));
    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());
    turn_flow.set_wire_recorder(Some(
        Arc::new(CollectingWireRecorder(Arc::clone(&recorded))) as Arc<dyn WireOpRecorder>,
    ));

    // When the turn runs and the tool cancels it mid-step
    let result = turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Hello".to_string(),
            }],
            PromptOrigin::User,
            Arc::clone(&memory),
            &tm,
            Arc::new(FixedLlmResolver::new(llm)),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("mid-turn user-cancelled turn should return Ok with Aborted");

    // Then the turn aborted with the accumulated step (the Ok(Aborted) arm)
    // and the memory's last message is the interruption reminder wrapped in
    // <system-reminder> with the Injection origin
    assert_eq!(result.stop_reason, TurnStopReason::Aborted);
    assert_eq!(result.steps, 1);
    {
        let mem = memory.lock().unwrap_or_else(|e| e.into_inner());
        let last = mem.history().last().expect("reminder appended to memory");
        assert_eq!(
            last.origin,
            Some(PromptOrigin::Injection),
            "reminder must carry the Injection origin"
        );
        assert_eq!(
            last.message.text_content(),
            mirri_core::system_reminder_wire::system_reminder_text(
                mirri_core::interruption_reminder::INTERRUPTION_REMINDER_TEXT
            ),
            "reminder text must be the <system-reminder>-wrapped interruption text"
        );
    }

    // And the wire recorded, in order (after the step's own loop-event ops):
    // turn.cancel (target "active", reason "user_cancelled"), the reminder's
    // context.append_message, then interruptionReminder.recorded — all
    // carrying the turn id
    {
        let ops = recorded.lock().unwrap_or_else(|e| e.into_inner());
        let reminder_text = mirri_core::system_reminder_wire::system_reminder_text(
            mirri_core::interruption_reminder::INTERRUPTION_REMINDER_TEXT,
        );
        let interruption_ops: Vec<&WireOp> = ops
            .iter()
            .filter(|op| match op {
                WireOp::TurnCancel { .. } | WireOp::InterruptionReminderRecorded { .. } => true,
                // The mid-step path also records the capability-gap
                // injection (`<tools_added>`) as a context.append_message —
                // only the interruption reminder belongs to this sequence.
                WireOp::ContextAppendMessage { message } => {
                    message["content"][0]["text"] == reminder_text
                }
                _ => false,
            })
            .collect();
        assert_eq!(
            interruption_ops.len(),
            3,
            "exactly the cancel fact, the reminder append, and the delivery marker: {ops:?}"
        );
        match interruption_ops[0] {
            WireOp::TurnCancel {
                turn_id,
                target,
                reason,
            } => {
                assert_eq!(*turn_id, Some(result.turn_id));
                assert_eq!(target.as_deref(), Some("active"));
                assert_eq!(reason.as_deref(), Some("user_cancelled"));
            }
            other => panic!("expected turn.cancel, got {other:?}"),
        }
        match interruption_ops[1] {
            WireOp::ContextAppendMessage { message } => {
                assert_eq!(message["role"], "user");
                assert_eq!(message["origin"]["kind"], "injection");
                assert_eq!(
                    message["content"][0]["text"],
                    mirri_core::system_reminder_wire::system_reminder_text(
                        mirri_core::interruption_reminder::INTERRUPTION_REMINDER_TEXT
                    )
                );
            }
            other => panic!("expected context.append_message, got {other:?}"),
        }
        match interruption_ops[2] {
            WireOp::InterruptionReminderRecorded { turn_id } => {
                assert_eq!(*turn_id, result.turn_id);
            }
            other => panic!("expected interruptionReminder.recorded, got {other:?}"),
        }
        // The interruption ops sit at the tail of the recorded stream (after
        // the cancelled step's own loop-event/usage ops), preserving the
        // append-only order the embedded layer's turn.ended follows.
        assert!(
            matches!(
                ops.last(),
                Some(WireOp::InterruptionReminderRecorded { .. })
            ),
            "interruptionReminder.recorded must be the last recorded op: {ops:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Prune-first compaction (P8a-2 修剪优先于摘要)
// ---------------------------------------------------------------------------

/// Pre-populate a memory with one old tool exchange carrying a huge result,
/// followed by `recent` settled assistant messages. The leading
/// user + plain-assistant exchange gives auto-compaction a legal
/// retention-tail cut (a compactable prefix may not end on a user message
/// or an assistant whose tool calls are still open above the cut).
fn memory_with_old_huge_tool_result(
    recent: usize,
    result_chars: usize,
) -> Arc<Mutex<ContextMemory>> {
    use mirri_core::context_memory::LoopEvent;

    let mut mem = ContextMemory::new();
    mem.append_user_message(
        vec![ContentPart::Text {
            text: "initial question".to_string(),
        }],
        PromptOrigin::User,
    );
    mem.append_message(mirri_core::context::ContextMessage {
        message: mirri_core::message::Message::assistant("initial answer"),
        origin: None,
        is_error: false,
        note: None,
        with_skills: None,
        entry_seq: 0,
    });
    mem.append_loop_event(LoopEvent::StepBegin {
        step_id: "step-1".to_string(),
        entry_seq: None,
    });
    mem.append_loop_event(LoopEvent::ToolCall {
        step_id: "step-1".to_string(),
        tool_call_id: "call-1".to_string(),
        tool_name: "Echo".to_string(),
        args: serde_json::json!({}),
    });
    mem.append_loop_event(LoopEvent::ToolResult {
        tool_call_id: "call-1".to_string(),
        output: ToolOutput::Text("x".repeat(result_chars)),
        is_error: false,
        note: None,
        entry_seq: None,
    });
    mem.append_loop_event(LoopEvent::StepEnd {
        step_id: "step-1".to_string(),
        input_tokens: 0,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
    });
    for i in 0..recent {
        let step_id = format!("step-recent-{i}");
        mem.append_loop_event(LoopEvent::StepBegin {
            step_id: step_id.clone(),
            entry_seq: None,
        });
        mem.append_loop_event(LoopEvent::ContentPart {
            step_id: step_id.clone(),
            text: format!("recent assistant message {i}"),
        });
        mem.append_loop_event(LoopEvent::StepEnd {
            step_id,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        });
    }
    Arc::new(Mutex::new(mem))
}

#[tokio::test]
async fn should_make_a_summarizer_call_when_pressure_lives_in_old_tool_results() {
    // Given a flow whose raw token measure trips the compaction trigger,
    // with the pressure concentrated in an old tool result. The normal
    // projection no longer prunes old tool results (mutating the request
    // prefix destroyed the provider's prompt cache), so the pruned
    // re-measure cannot relieve the pressure anymore.
    let compaction_strategy = CompactionStrategy {
        max_context_tokens: 100_000,
        trigger_ratio: 0.85,
        ..CompactionStrategy::default()
    };
    let mut turn_flow = TurnFlow::new(TurnFlowConfig {
        compaction_strategy,
        ..TurnFlowConfig::default()
    });

    let memory = memory_with_old_huge_tool_result(21, 400_000);
    {
        let mem = memory.lock().unwrap();
        assert!(mem.token_count_with_pending() >= 85_000);
        assert!(mem.pruned_request_token_estimate() >= 85_000);
    }

    // And an LLM scripted with the summarizer's summary, then the turn's
    // own answer
    let llm = MockLlm::new(vec![
        MockResponse::end_turn("The old tool returned a huge payload."),
        MockResponse::end_turn("Answer."),
    ]);
    let chat_calls = Arc::clone(&llm.chat_calls);
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let cancel = CancelToken::new();

    // When a turn runs
    let result = turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "What did the old tool return?".to_string(),
            }],
            PromptOrigin::User,
            Arc::clone(&memory),
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(llm))),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("turn should succeed");

    // Then the turn completes normally with exactly TWO LLM calls — the
    // summarizer call (the only pressure release without pruning) plus
    // the turn's own step call.
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    assert_eq!(
        *chat_calls.lock().unwrap(),
        2,
        "the summarizer call plus the turn's own step call must be the only LLM calls"
    );
}

// ---------------------------------------------------------------------------
// Interrupted-turn closure red line (P11a F-P8-9)
// ---------------------------------------------------------------------------

/// A mock LLM whose `chat` call cancels the turn's token and then fails —
/// reproducing the "stream error racing a cancel" interleaving: the step's
/// `step.begin` was already recorded, and the error surfaces (re-classified
/// as an abort) before `finalize_step` could settle the step.
struct CancelRacingLlm;

impl Llm for CancelRacingLlm {
    fn system_prompt(&self) -> &str {
        "You are a test assistant."
    }

    fn model_name(&self) -> &str {
        "test-model"
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

    fn chat(&self, params: LlmChatParams) -> EngineResult<LlmEventStream> {
        // The cancellation lands mid-step — after the loop's top-of-loop
        // abort check passed and after `step.begin` was dispatched, before
        // any step content was delivered.
        params.cancel.cancel();
        Err(EngineError::ProviderError(
            "connection dropped mid-stream".to_string(),
        ))
    }
}

/// Given a turn whose LLM call fails while the cancel token flips (the
/// stream-error-races-cancel interleaving, aborted before `finalize_step`
/// runs — leaving a vacuous open step behind), When the turn closes, Then
/// the interrupted closure settles the open step: every `step.begin` on the
/// wire has a paired `step.end`, and no open step or pending tool result
/// survives in memory.
#[tokio::test]
async fn should_close_the_interrupted_turn_when_a_stream_error_races_cancel_before_finalize_step() {
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    let recorded = Arc::new(Mutex::new(Vec::new()));
    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());
    turn_flow.set_wire_recorder(Some(
        Arc::new(CollectingWireRecorder(Arc::clone(&recorded))) as Arc<dyn WireOpRecorder>,
    ));

    // When a turn runs against an LLM that cancels then errors mid-stream
    let result = turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Hello".to_string(),
            }],
            PromptOrigin::User,
            Arc::clone(&memory),
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(CancelRacingLlm))),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("the aborted turn must return Ok(Aborted), not an error");

    // Then the turn aborted with the step it had started
    assert_eq!(result.stop_reason, TurnStopReason::Aborted);
    assert_eq!(result.steps, 1);

    // And the wire log tells a closed story: the dangling `step.begin`
    // (recorded before the failing chat) is settled by the interrupted
    // closure's `step.end` — no step bracket is left open for a resume to
    // trip over.
    let ops = recorded.lock().unwrap_or_else(|e| e.into_inner());
    let mut open_steps: Vec<String> = Vec::new();
    let mut closed_steps: Vec<String> = Vec::new();
    for op in ops.iter() {
        match op {
            WireOp::ContextAppendLoopEvent { event } => {
                match event.get("type").and_then(serde_json::Value::as_str) {
                    Some("step.begin") => open_steps.push(
                        event
                            .get("uuid")
                            .and_then(serde_json::Value::as_str)
                            .expect("step.begin carries a uuid")
                            .to_string(),
                    ),
                    Some("step.end") => closed_steps.push(
                        event
                            .get("uuid")
                            .and_then(serde_json::Value::as_str)
                            .expect("step.end carries a uuid")
                            .to_string(),
                    ),
                    _ => {}
                }
            }
            WireOp::UsageRecord { .. } => {}
            other => panic!("unexpected op recorded by the loop: {other:?}"),
        }
    }
    assert!(
        !open_steps.is_empty(),
        "the failing step must have recorded its step.begin"
    );
    for step_id in &open_steps {
        assert!(
            closed_steps.contains(step_id),
            "step.begin {step_id} has no paired step.end — the interrupted turn was left open"
        );
    }

    // And the memory is fully closed: no pending tool result survived and
    // the deferred queue is empty (nothing strands a resume).
    {
        let mem = memory.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            !mem.has_open_tool_exchange(),
            "no tool exchange may stay open after the interrupted closure"
        );
        // (`deferred_messages` is crate-private; the wire pairing above is
        // the client-visible closure invariant, and the memory tests in
        // context_memory cover the deferred flush separately.)
    }
}

/// Given a context memory that still carries an open tool exchange with
/// a deferred message parked behind it (the state a crash mid-exchange
/// leaves behind — one step issued a tool call whose result never
/// arrived, and a message appended while the exchange was open was
/// buffered), When `run_turn` starts against an already-cancelled token,
/// Then the loop-top abort arm closes the interrupted turn before
/// returning `Ok(Aborted)`: the pending tool call is answered with the
/// interrupted result and the deferred message is flushed into history
/// (F-P11a-7: deleting the loop-top `close_interrupted_turn` call alone
/// must fail this test — the `Err(is_abort)` arm already has its own red
/// line at
/// `should_close_the_interrupted_turn_when_a_stream_error_races_cancel_before_finalize_step`).
///
/// The test drives `run_turn` directly (not `TurnFlow::run_one_turn`)
/// because the turn-flow wrapper's own top-of-loop `cancel.check()?`
/// returns before `run_turn` is entered — the loop-top arm is only
/// reachable through the inner loop itself.
#[tokio::test]
async fn should_close_the_interrupted_turn_at_the_loop_top_when_a_cancelled_turn_opens_with_deferred_messages()
 {
    use mirri_core::context_memory::LoopEvent;
    use mirri_core::hooks::NoopHooks;
    use mirri_core::loop_engine::{MessageBuilders, RunTurnConfig, run_turn};
    use mirri_core::message::Message;

    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    // Seed the crash-mid-exchange state: a step issued a tool call whose
    // result never arrived, and a message that landed while the exchange
    // was open went into the deferred buffer.
    {
        let mut mem = memory.lock().unwrap_or_else(|e| e.into_inner());
        mem.append_loop_event(LoopEvent::StepBegin {
            step_id: "seeded-step".to_string(),
            entry_seq: None,
        });
        mem.append_loop_event(LoopEvent::ToolCall {
            step_id: "seeded-step".to_string(),
            tool_call_id: "seeded-call".to_string(),
            tool_name: "Echo".to_string(),
            args: serde_json::json!({}),
        });
        mem.append_user_message(
            vec![ContentPart::Text {
                text: "parked while the exchange was open".to_string(),
            }],
            PromptOrigin::User,
        );
        assert!(
            mem.has_open_tool_exchange(),
            "the seeded memory must carry an open tool exchange"
        );
    }

    // The cancellation is observed before the loop begins, so the very
    // first loop-top abort check fires — no step, no LLM call.
    cancel.cancel();

    let llm = Arc::new(MockLlm::new(Vec::new()));
    let hooks = NoopHooks;
    let message_builders = MessageBuilders {
        normal: Box::new(|| Ok(Vec::<Message>::new())),
        media_degraded: None,
        media_stripped: None,
        strict: None,
        format_rejected_flag: Arc::new(AtomicBool::new(false)),
    };
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let recorder = CollectingWireRecorder(Arc::clone(&recorded));

    let result = run_turn(
        RunTurnConfig::new("seeded-turn"),
        &FixedLlmResolver::new(Arc::clone(&llm) as Arc<dyn Llm>),
        &hooks,
        &tools,
        &mut validators,
        Arc::clone(&memory),
        &cancel,
        &message_builders,
        None,
        None,
        Some(&recorder),
    )
    .await
    .expect("the aborted turn must return Ok(Aborted), not an error");

    // Then the turn aborted before any step ran
    assert_eq!(result.stop_reason, TurnStopReason::Aborted);
    assert_eq!(result.steps, 0, "the loop-top arm fires before step 1");
    assert_eq!(
        *llm.chat_calls.lock().unwrap(),
        0,
        "the LLM must never be called on a pre-cancelled turn"
    );

    // And the memory is closed: the pending tool call got its interrupted
    // result and the deferred message was flushed into history — nothing
    // strands a resume.
    {
        let mem = memory.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            !mem.has_open_tool_exchange(),
            "the loop-top closure must answer the pending tool call"
        );
        let context = mem
            .messages()
            .iter()
            .map(|m| m.text_content())
            .collect::<String>();
        assert!(
            context.contains("parked while the exchange was open"),
            "the deferred message must be flushed into history: {context}"
        );
    }

    // And the wire log records the closure's loop events (the interrupted
    // tool result), so a resume replays the closed state.
    let ops = recorded.lock().unwrap_or_else(|e| e.into_inner());
    let tool_results = ops
        .iter()
        .filter(|op| matches!(op, WireOp::ContextAppendLoopEvent { .. }))
        .count();
    assert!(
        tool_results >= 1,
        "the loop-top closure must persist its loop events: {:?}",
        *ops
    );
}

/// Given a turn flow with a wire recorder and a steer parked in the shared
/// buffer, When the turn runs, Then the first `before_step` flush appends the
/// steered content into context memory AND records the durable consumption
/// marker `turn.steer_flushed` for the exact count taken — so a resume
/// restores only steers that never reached context (T3, F-P7-12).
#[tokio::test]
async fn should_record_steer_flush_marker_when_parked_steer_flushes() {
    // Given a turn flow with a wire recorder installed and a parked steer
    let llm = MockLlm::new(vec![MockResponse::end_turn("Hello!")]);
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    let recorded = Arc::new(Mutex::new(Vec::new()));
    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());
    turn_flow.set_wire_recorder(Some(
        Arc::new(CollectingWireRecorder(Arc::clone(&recorded))) as Arc<dyn WireOpRecorder>,
    ));
    turn_flow
        .steer_buffer_handle()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(
            vec![ContentPart::Text {
                text: "and focus on the rust side".to_string(),
            }],
            PromptOrigin::User,
        );

    // When the turn runs (the first before_step flushes the parked steer)
    turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "original question".to_string(),
            }],
            PromptOrigin::User,
            Arc::clone(&memory),
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(llm))),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("turn should succeed");

    // Then the steer buffer is drained and the steered content landed in
    // context memory as a user message
    assert!(
        !turn_flow.has_pending_steers(),
        "the parked steer must be consumed by the turn"
    );
    {
        let mem = memory.lock().unwrap_or_else(|e| e.into_inner());
        let context = mem
            .messages()
            .iter()
            .map(|m| m.text_content())
            .collect::<String>();
        assert!(
            context.contains("and focus on the rust side"),
            "the steered content must be flushed into history: {context}"
        );
    }

    // And the recorder saw exactly one consumption marker with the exact
    // count — the durable steer buffer stays consistent with the wire log.
    let ops = recorded.lock().unwrap_or_else(|e| e.into_inner());
    let markers: Vec<&WireOp> = ops
        .iter()
        .filter(|op| matches!(op, WireOp::TurnSteerFlushed { .. }))
        .collect();
    assert_eq!(markers.len(), 1, "recorded ops: {:?}", *ops);
    match markers[0] {
        WireOp::TurnSteerFlushed { count } => assert_eq!(*count, 1),
        other => panic!("expected a steer flush marker, got {other:?}"),
    }
}

/// Given a turn flow with no wire recorder (unit tests, replay), When a turn
/// flushes a parked steer, Then the flush still lands in context memory —
/// the durable marker is best-effort and never blocks the turn.
#[tokio::test]
async fn should_flush_parked_steer_into_memory_when_no_wire_recorder_installed() {
    // Given a turn flow without a recorder and a parked steer
    let llm = MockLlm::new(vec![MockResponse::end_turn("Hello!")]);
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());
    turn_flow
        .steer_buffer_handle()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(
            vec![ContentPart::Text {
                text: "unrecorded steer".to_string(),
            }],
            PromptOrigin::User,
        );

    // When the turn runs
    turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "prompt".to_string(),
            }],
            PromptOrigin::User,
            Arc::clone(&memory),
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(llm))),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("turn should succeed without a recorder");

    // Then the steer still reached context memory
    let mem = memory.lock().unwrap_or_else(|e| e.into_inner());
    let context = mem
        .messages()
        .iter()
        .map(|m| m.text_content())
        .collect::<String>();
    assert!(
        context.contains("unrecorded steer"),
        "the flush must not depend on the recorder: {context}"
    );
}

// ---------------------------------------------------------------------------
// Capability-gap injection × model catalog wiring
// ---------------------------------------------------------------------------

/// A test catalog double whose single entry declares the "vision"
/// capability for the model the vision profile points at.
struct VisionOnlyCatalog;

impl mirri_core::ModelCatalog for VisionOnlyCatalog {
    fn list_models(&self) -> Vec<mirri_core::ModelCatalogEntry> {
        vec![self.vision_entry()]
    }

    fn get_by_id(&self, id: &str) -> Option<mirri_core::ModelCatalogEntry> {
        (id == "vision-model").then(|| self.vision_entry())
    }
}

impl VisionOnlyCatalog {
    fn vision_entry(&self) -> mirri_core::ModelCatalogEntry {
        mirri_core::ModelCatalogEntry {
            id: "vision-model".to_string(),
            model: "gpt-4-vision".to_string(),
            provider: "test".to_string(),
            description: None,
            max_context_size: None,
            capabilities: vec!["vision".to_string()],
            default_effort: None,
        }
    }
}

/// A profile manager with one registered profile whose default model is
/// the catalog's vision-capable model.
fn gap_profile_manager() -> Arc<mirri_core::ProfileManager> {
    let manager = mirri_core::ProfileManager::new();
    manager
        .register(mirri_core::AgentProfile {
            name: "vision-agent".to_string(),
            model: "vision-model".to_string(),
            ..Default::default()
        })
        .expect("profile registration should succeed");
    Arc::new(manager)
}

/// The user prompt carrying an image (the modality the turn's mock LLM —
/// text-only — cannot handle).
fn image_prompt() -> Vec<ContentPart> {
    vec![ContentPart::ImageUrl {
        image_url: mirri_core::message::ImageUrlData {
            url: "data:image/png;base64,AA".to_string(),
            detail: None,
            id: None,
        },
        detail: None,
    }]
}

/// Every injection-origin reminder text in memory after the turn.
fn injected_reminder_texts(memory: &Arc<Mutex<ContextMemory>>) -> Vec<String> {
    memory
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .history()
        .iter()
        .filter(|m| m.origin == Some(PromptOrigin::Injection))
        .map(|m| m.message.text_content())
        .collect()
}

#[tokio::test]
async fn should_name_capable_profiles_in_gap_injection_when_catalog_wired() {
    // Given a text-only main model, a vision-capable model in the catalog,
    // and a subagent profile whose default model is that vision model
    let llm = MockLlm::new(vec![MockResponse::end_turn("Got it!")]);
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());
    turn_flow.set_profile_manager(gap_profile_manager());
    turn_flow.set_model_catalog(Some(Arc::new(VisionOnlyCatalog)));

    // When a turn runs on an image-bearing user prompt
    turn_flow
        .run_one_turn(
            image_prompt(),
            PromptOrigin::User,
            Arc::clone(&memory),
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(llm))),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("turn should succeed");

    // Then the injected hint names the vision profile (its capabilities were
    // resolved through the wired catalog instead of defaulting to none)
    let reminders = injected_reminder_texts(&memory);
    let hint = reminders
        .iter()
        .find(|t| t.contains("dispatch a subagent"))
        .expect("a capability-gap hint should be injected");
    assert!(
        hint.contains("vision-agent"),
        "the hint should name the vision-capable profile: {hint}"
    );
    assert!(
        hint.contains("image"),
        "the hint should name the image modality: {hint}"
    );
}

#[tokio::test]
async fn should_not_name_profiles_when_catalog_absent() {
    // Given the same text-only model and vision profile, but no catalog
    // wired (standalone flow shape — capabilities default to unknown)
    let llm = MockLlm::new(vec![MockResponse::end_turn("Got it!")]);
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());
    turn_flow.set_profile_manager(gap_profile_manager());

    // When a turn runs on an image-bearing user prompt
    turn_flow
        .run_one_turn(
            image_prompt(),
            PromptOrigin::User,
            Arc::clone(&memory),
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(llm))),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("turn should succeed");

    // Then a capability-oriented hint is still injected — an empty catalog
    // snapshot is permissive (standalone flow cannot verify feasibility, TS
    // parity) — but no profile name appears (capabilities are unknown)
    let reminders = injected_reminder_texts(&memory);
    let hint = reminders
        .iter()
        .find(|t| t.contains("dispatch a subagent"))
        .expect("a capability-gap hint should be injected without a catalog");
    assert!(
        !hint.contains("vision-agent"),
        "no profile should be named without a catalog: {hint}"
    );
    assert!(
        hint.contains("Available models"),
        "the hint should stay capability-oriented: {hint}"
    );
}

// -----------------------------------------------------------------------
// tools_added announcement scope (TS parity, ADR-3-R2)
// -----------------------------------------------------------------------

/// An MCP-shaped probe tool — announcement material under disclosure.
struct McpProbeTool;

impl Tool for McpProbeTool {
    fn name(&self) -> &str {
        "mcp__srv__probe"
    }
    fn description(&self) -> &str {
        "An MCP probe tool"
    }
    fn parameters(&self) -> &Value {
        static SCHEMA: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| serde_json::json!({ "type": "object", "properties": {} }))
    }
    fn resolve_execution(&self, _input: Value) -> EngineResult<ToolExecution> {
        Ok(ToolExecution::Error(mirri_core::tool::ToolErrorResult {
            message: "never executed in these tests".to_string(),
        }))
    }
}

#[tokio::test]
async fn should_not_inject_tools_added_when_disclosure_is_disabled() {
    // Given a static tool table (a registered builtin, disclosure off —
    // the production default) and a flow with no other injectors
    let llm = MockLlm::new(vec![MockResponse::end_turn("Hello!")]);
    let mut tools = ToolManager::new();
    tools.register_builtin(Arc::new(EchoTool));
    tools.enable_all_registered();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());

    // When a turn runs
    turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "hi".to_string(),
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

    // Then no tools_added announcement lands in memory — the static
    // table's full definitions are already in every request
    let guard = memory.lock().unwrap_or_else(|e| e.into_inner());
    assert!(
        guard
            .history()
            .iter()
            .all(|entry| !entry.message.text_content().contains("<tools_added>")),
        "no tools_added reminder may be injected for static tables"
    );
}

#[tokio::test]
async fn should_announce_only_dynamic_tools_when_disclosure_is_enabled() {
    // Given a disclosure-mode tool table with one builtin and one MCP tool
    let llm = MockLlm::new(vec![MockResponse::end_turn("Hello!")]);
    let mut tools = ToolManager::new();
    tools.register_builtin(Arc::new(EchoTool));
    tools.register_mcp_tool("srv".to_string(), Arc::new(McpProbeTool));
    tools.enable_all_registered();
    tools.set_disclosure(true);
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());

    // When a turn runs
    turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "hi".to_string(),
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

    // Then exactly one tools_added announcement lands in memory, naming
    // the loadable dynamic (MCP) tool only — the builtin's definition is
    // already in every request and must not be announced
    let guard = memory.lock().unwrap_or_else(|e| e.into_inner());
    let announcements: Vec<String> = guard
        .history()
        .iter()
        .map(|entry| entry.message.text_content())
        .filter(|text| text.contains("<tools_added>"))
        .collect();
    assert_eq!(announcements.len(), 1, "exactly one announcement");
    assert!(
        announcements[0].contains("mcp__srv__probe"),
        "the announcement names the MCP tool: {}",
        announcements[0]
    );
    assert!(
        !announcements[0].contains("Echo"),
        "builtins are not announced: {}",
        announcements[0]
    );
}

// ---------------------------------------------------------------------------
// External command hooks — Stop lifecycle event
// ---------------------------------------------------------------------------

/// Register a Stop hook on a fresh engine and wire it into the turn flow.
fn make_stop_hook_turn_flow(command: &str) -> (TurnFlow, Arc<HookEngine>) {
    let engine = Arc::new(HookEngine::new());
    engine.register(HookDef::new(HookEventType::Stop, command.to_string()));
    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());
    turn_flow.set_external_hooks(engine.clone(), HashMap::new());
    (turn_flow, engine)
}

/// Given a turn flow wired with a blocking Stop hook, When the model reaches
/// its natural stop, Then the turn continues exactly once with the hook's
/// reason injected as a system-triggered user message, and the second
/// natural stop ends the turn (TS `stopHookContinuationUsed` parity: the
/// block reason is appended to context and the turn is forced onward, at
/// most once per turn).
#[tokio::test]
async fn should_continue_turn_once_with_reason_injected_when_stop_hook_blocks() {
    // Given a Stop hook that dumps its JSON input to a marker file and then
    // blocks with "keep working" on stderr (the exit-2 reason chain)
    let marker_path =
        std::env::temp_dir().join(format!("stop_hook_input_{}.json", std::process::id()));
    let _ = std::fs::remove_file(&marker_path);

    let command = format!("cat > {marker_path:?}; echo keep working >&2; exit 2");
    let (mut turn_flow, _engine) = make_stop_hook_turn_flow(&command);

    // And an LLM that answers with terminal stops (end_turn) twice: the
    // first natural stop is overridden by the hook's block, the second ends
    // the turn (the once-per-turn guard).
    let llm = MockLlm::new(vec![
        MockResponse::end_turn("First answer"),
        MockResponse::end_turn("Second answer"),
    ]);
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    // When a user turn runs
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

    // Then the turn ran two steps: one forced continuation, then the turn
    // ended at the second natural stop.
    assert_eq!(result.steps, 2);
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);

    // And the hook saw the TS input shape — `stop_hook_active: false` on
    // every Stop trigger (TS `runStop` always sends false).
    let content = std::fs::read_to_string(&marker_path).expect("hook input file should exist");
    let input: Value = serde_json::from_str(&content).expect("hook input should be JSON");
    assert_eq!(input["stop_hook_active"], serde_json::json!(false));
    let _ = std::fs::remove_file(&marker_path);

    // And exactly one reason message was injected as a system-triggered
    // user message, carrying the hook's stderr reason.
    let history = {
        let guard = memory.lock().unwrap();
        guard.history().to_vec()
    };
    let reasons: Vec<_> = history
        .iter()
        .filter(|m| m.origin.as_ref() == Some(&PromptOrigin::SystemTrigger))
        .collect();
    assert_eq!(
        reasons.len(),
        1,
        "exactly one injected stop reason, got {history:?}"
    );
    assert_eq!(
        reasons[0].message.role,
        mirri_core::message::Role::User,
        "the injected reason is a user message"
    );
    assert_eq!(reasons[0].message.text_content().trim(), "keep working");

    // And it lands between the two assistant answers so the continuation
    // step consumes it.
    let first_answer_idx = history
        .iter()
        .position(|m| m.message.text_content() == "First answer")
        .expect("first assistant answer present");
    let second_answer_idx = history
        .iter()
        .position(|m| m.message.text_content() == "Second answer")
        .expect("second assistant answer present");
    let reason_idx = history
        .iter()
        .position(|m| m.origin.as_ref() == Some(&PromptOrigin::SystemTrigger))
        .expect("injected reason present");
    assert!(
        first_answer_idx < reason_idx && reason_idx < second_answer_idx,
        "reason must land between the two answers: {history:?}"
    );
}

/// Given a turn flow wired with a passing Stop hook, When the model reaches
/// its natural stop, Then the turn ends immediately — a passing Stop hook
/// never forces a continuation and injects nothing.
#[tokio::test]
async fn should_stop_normally_when_stop_hook_passes() {
    // Given a Stop hook that passes (exit 0)
    let (mut turn_flow, _engine) = make_stop_hook_turn_flow("echo ok");

    let llm = MockLlm::new(vec![MockResponse::end_turn("Done")]);
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    // When a user turn runs
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

    // Then the turn ended at its first natural stop with nothing injected
    assert_eq!(result.steps, 1);
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    let history = {
        let guard = memory.lock().unwrap();
        guard.history().to_vec()
    };
    assert!(
        history
            .iter()
            .all(|m| m.origin.as_ref() != Some(&PromptOrigin::SystemTrigger)),
        "no stop reason injected for a passing hook, got {history:?}"
    );
}

// ---------------------------------------------------------------------------
// External command hooks — PreToolUse lifecycle event
// ---------------------------------------------------------------------------

/// Register a PreToolUse hook on a fresh engine and wire it into the turn
/// flow.
fn make_pre_tool_use_turn_flow(command: &str) -> (TurnFlow, Arc<HookEngine>) {
    let engine = Arc::new(HookEngine::new());
    engine.register(HookDef::new(HookEventType::PreToolUse, command.to_string()));
    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());
    turn_flow.set_external_hooks(engine.clone(), HashMap::new());
    (turn_flow, engine)
}

/// Given a passing PreToolUse hook that returns additionalContext, When a
/// tool call executes, Then the context is injected into conversation memory
/// as a user message with `HookAdditionalContext` origin and the tool still
/// runs (TS parity: `injectHookAdditionalContext` on the allow path).
#[tokio::test]
async fn should_inject_additional_context_when_pre_tool_use_hook_passes() {
    // Given a PreToolUse hook returning structured additionalContext
    let command = r#"echo '{"hookSpecificOutput":{"additionalContext":"ctx-from-pretool"}}'"#;
    let (mut turn_flow, _engine) = make_pre_tool_use_turn_flow(command);

    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![make_tool_call(
            "call-1",
            "Echo",
            serde_json::json!({"message": "hi"}),
        )]),
        MockResponse::end_turn("done"),
    ]);
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    // When a turn with a tool call runs
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

    // Then the turn succeeded with the tool executed
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    let history = {
        let guard = memory.lock().unwrap();
        guard.history().to_vec()
    };
    assert!(
        history
            .iter()
            .any(|m| m.message.role == mirri_core::message::Role::Tool
                && m.message.text_content().contains("hi")),
        "the Echo tool should have run, got {history:?}"
    );

    // And the additionalContext landed as a user message with
    // HookAdditionalContext origin
    let ctx_msgs: Vec<_> = history
        .iter()
        .filter(|m| m.origin.as_ref() == Some(&PromptOrigin::HookAdditionalContext))
        .collect();
    assert_eq!(ctx_msgs.len(), 1, "one injected context, got {history:?}");
    assert_eq!(ctx_msgs[0].message.role, mirri_core::message::Role::User);
    assert_eq!(ctx_msgs[0].message.text_content(), "ctx-from-pretool");
}

/// Given a denying PreToolUse hook that also returns additionalContext, When
/// a tool call runs, Then the call is blocked with the deny reason and the
/// context is still injected (TS parity: `includeBlock: true` on the
/// PreToolUse deny path).
#[tokio::test]
async fn should_block_tool_and_still_inject_context_when_pre_tool_use_hook_denies() {
    // Given a PreToolUse hook with a structured deny plus additionalContext
    let command = r#"echo '{"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"not allowed","additionalContext":"deny-ctx"}}'"#;
    let (mut turn_flow, _engine) = make_pre_tool_use_turn_flow(command);

    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![make_tool_call(
            "call-1",
            "Echo",
            serde_json::json!({"message": "hi"}),
        )]),
        MockResponse::end_turn("done"),
    ]);
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    // When a turn with a tool call runs
    turn_flow
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

    let history = {
        let guard = memory.lock().unwrap();
        guard.history().to_vec()
    };

    // Then the tool never ran: the tool result is an error carrying the deny
    // reason instead of the echoed input
    assert!(
        history
            .iter()
            .any(|m| m.message.role == mirri_core::message::Role::Tool
                && m.message.text_content().contains("not allowed")),
        "the blocked call should surface the deny reason, got {history:?}"
    );
    assert!(
        !history
            .iter()
            .any(|m| m.message.role == mirri_core::message::Role::Tool
                && m.message.text_content().trim() == "hi"),
        "the Echo tool must not have executed, got {history:?}"
    );

    // And the deny-path additionalContext was injected anyway
    let ctx_msgs: Vec<_> = history
        .iter()
        .filter(|m| m.origin.as_ref() == Some(&PromptOrigin::HookAdditionalContext))
        .collect();
    assert_eq!(
        ctx_msgs.len(),
        1,
        "deny-path context must be injected, got {history:?}"
    );
    assert_eq!(ctx_msgs[0].message.text_content(), "deny-ctx");
}

/// The env var gating PreToolUse `updatedInput` rewriting (TS parity: the
/// `hook-command-rewrite` flag).
const HOOK_COMMAND_REWRITE_ENV: &str = "MIRRICODE_EXPERIMENTAL_HOOK_COMMAND_REWRITE";

/// Serializes tests that mutate the hook-command-rewrite env var — the gate
/// reads process environment, so concurrent tests in this binary must not
/// race on it.
static HOOK_REWRITE_ENV_LOCK: Mutex<()> = Mutex::new(());

/// Saves and restores the hook-command-rewrite env var; every env-mutating
/// test holds [`HOOK_REWRITE_ENV_LOCK`] for its whole body.
struct HookRewriteEnvGuard(Option<String>);

impl HookRewriteEnvGuard {
    fn set(value: Option<&str>) -> Self {
        let saved = std::env::var(HOOK_COMMAND_REWRITE_ENV).ok();
        // SAFETY: guarded by HOOK_REWRITE_ENV_LOCK; restored on drop.
        match value {
            Some(value) => unsafe { std::env::set_var(HOOK_COMMAND_REWRITE_ENV, value) },
            None => unsafe { std::env::remove_var(HOOK_COMMAND_REWRITE_ENV) },
        }
        Self(saved)
    }
}

impl Drop for HookRewriteEnvGuard {
    fn drop(&mut self) {
        // SAFETY: same lock discipline as `set`.
        match self.0.take() {
            Some(value) => unsafe { std::env::set_var(HOOK_COMMAND_REWRITE_ENV, value) },
            None => unsafe { std::env::remove_var(HOOK_COMMAND_REWRITE_ENV) },
        }
    }
}

/// Given the enabled hook-command-rewrite flag and a passing PreToolUse hook
/// that returns updatedInput, When a tool call runs, Then the tool executes
/// with the rewritten arguments AND the live `ToolCallStarted` event (the
/// UI's real-time tool-call args source) carries the rewritten arguments
/// instead of the model's original ones (TS parity: the `hook-command-rewrite`
/// enabled behavior — `event.rewriteArgs` + `dispatchToolCall`).
#[allow(clippy::await_holding_lock)] // intentional: the env gate must hold across the awaited turn
#[tokio::test]
async fn should_execute_tool_with_rewritten_args_when_pre_tool_use_hook_returns_updated_input() {
    // Given the rewrite flag enabled and a PreToolUse hook returning
    // structured updatedInput
    let _env_lock = HOOK_REWRITE_ENV_LOCK.lock().unwrap();
    let _env = HookRewriteEnvGuard::set(Some("1"));
    let command = r#"echo '{"hookSpecificOutput":{"updatedInput":{"message":"rewritten-input"}}}'"#;
    let (mut turn_flow, _engine) = make_pre_tool_use_turn_flow(command);

    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![make_tool_call(
            "call-1",
            "Echo",
            serde_json::json!({"message": "original"}),
        )]),
        MockResponse::end_turn("done"),
    ]);
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let live_events = Arc::new(Mutex::new(Vec::new()));
    let live_sink = Arc::clone(&live_events);
    let live_callback: mirri_core::loop_engine::LiveEventCallback =
        Arc::new(move |event| live_sink.lock().unwrap().push(event));

    // When a turn with a tool call runs
    turn_flow
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
            Some(live_callback),
            None,
        )
        .await
        .expect("turn should succeed");

    // Then the tool ran with the rewritten input, not the original
    let history = {
        let guard = memory.lock().unwrap();
        guard.history().to_vec()
    };
    assert!(
        history
            .iter()
            .any(|m| m.message.role == mirri_core::message::Role::Tool
                && m.message.text_content().contains("rewritten-input")),
        "the Echo tool should echo the rewritten input, got {history:?}"
    );
    assert!(
        !history
            .iter()
            .any(|m| m.message.role == mirri_core::message::Role::Tool
                && m.message.text_content().trim() == "original"),
        "the original input must not have been echoed, got {history:?}"
    );

    // And the live ToolCallStarted event (what the UI renders as the tool
    // call's arguments) carried the rewritten input — the dispatch happens
    // after the PreToolUse rewrite, with the effective args.
    let live_events = live_events.lock().unwrap();
    let started: Vec<&serde_json::Value> = live_events
        .iter()
        .filter_map(|event| match event {
            mirri_core::loop_engine::LiveEvent::ToolCallStarted { args, .. } => Some(args),
            _ => None,
        })
        .collect();
    assert_eq!(
        started,
        vec![&serde_json::json!({"message": "rewritten-input"})],
        "the ToolCallStarted live event should carry the rewritten args, got {started:?}"
    );
}

/// Given the disabled hook-command-rewrite flag (the default) and a passing
/// PreToolUse hook that returns updatedInput, When a tool call runs, Then
/// the updatedInput is ignored and the tool executes with its original
/// arguments (TS parity: the flag-off path never calls `rewriteArgs`).
#[allow(clippy::await_holding_lock)] // intentional: the env gate must hold across the awaited turn
#[tokio::test]
async fn should_execute_tool_with_original_args_when_pre_tool_use_hook_returns_updated_input_and_flag_disabled()
 {
    // Given the rewrite flag disabled (env absent) and a PreToolUse hook
    // returning structured updatedInput
    let _env_lock = HOOK_REWRITE_ENV_LOCK.lock().unwrap();
    let _env = HookRewriteEnvGuard::set(None);
    let command = r#"echo '{"hookSpecificOutput":{"updatedInput":{"message":"rewritten-input"}}}'"#;
    let (mut turn_flow, _engine) = make_pre_tool_use_turn_flow(command);

    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![make_tool_call(
            "call-1",
            "Echo",
            serde_json::json!({"message": "original"}),
        )]),
        MockResponse::end_turn("done"),
    ]);
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let live_events = Arc::new(Mutex::new(Vec::new()));
    let live_sink = Arc::clone(&live_events);
    let live_callback: mirri_core::loop_engine::LiveEventCallback =
        Arc::new(move |event| live_sink.lock().unwrap().push(event));

    // When a turn with a tool call runs
    turn_flow
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
            Some(live_callback),
            None,
        )
        .await
        .expect("turn should succeed");

    // Then the tool ran with the original input — the rewrite was ignored
    let history = {
        let guard = memory.lock().unwrap();
        guard.history().to_vec()
    };
    assert!(
        history
            .iter()
            .any(|m| m.message.role == mirri_core::message::Role::Tool
                && m.message.text_content().contains("original")),
        "the Echo tool should echo the original input, got {history:?}"
    );
    assert!(
        !history
            .iter()
            .any(|m| m.message.role == mirri_core::message::Role::Tool
                && m.message.text_content().contains("rewritten-input")),
        "the rewritten input must not have been executed, got {history:?}"
    );

    // And the live ToolCallStarted event carried the original args
    let live_events = live_events.lock().unwrap();
    let started: Vec<&serde_json::Value> = live_events
        .iter()
        .filter_map(|event| match event {
            mirri_core::loop_engine::LiveEvent::ToolCallStarted { args, .. } => Some(args),
            _ => None,
        })
        .collect();
    assert_eq!(
        started,
        vec![&serde_json::json!({"message": "original"})],
        "the ToolCallStarted live event should carry the original args, got {started:?}"
    );
}

/// Given a turn flow wired with a flag registry whose config layer enables
/// hook-command-rewrite (env untouched) and a PreToolUse hook returning
/// updatedInput, When a tool call runs, Then the tool executes with the
/// rewritten arguments — the config `[experimental]` section drives the
/// gate without any environment variable (TS parity: the `FlagService`
/// env → config → defaults chain).
#[tokio::test]
#[allow(clippy::await_holding_lock)] // intentional: the env gate must hold across the awaited turn
async fn should_execute_tool_with_rewritten_args_when_flag_enabled_via_config_layer() {
    // Given the rewrite flag enabled through the registry's CONFIG layer
    // only (the env stays untouched) and a PreToolUse hook returning
    // structured updatedInput
    let _env_lock = HOOK_REWRITE_ENV_LOCK.lock().unwrap();
    let _env = HookRewriteEnvGuard::set(None);
    let command = r#"echo '{"hookSpecificOutput":{"updatedInput":{"message":"rewritten-input"}}}'"#;
    let (mut turn_flow, _engine) = make_pre_tool_use_turn_flow(command);
    let flag_registry = Arc::new(mirri_core::FlagRegistry::new());
    let mut overrides = std::collections::HashMap::new();
    overrides.insert(mirri_core::HOOK_COMMAND_REWRITE_FLAG.to_string(), true);
    flag_registry.set_config_overrides(overrides);
    turn_flow.set_flag_registry(flag_registry);

    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![make_tool_call(
            "call-1",
            "Echo",
            serde_json::json!({"message": "original"}),
        )]),
        MockResponse::end_turn("done"),
    ]);
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    // When a turn with a tool call runs
    turn_flow
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

    // Then the tool ran with the rewritten input — the config layer alone
    // (no env) enabled the gate
    let history = {
        let guard = memory.lock().unwrap();
        guard.history().to_vec()
    };
    assert!(
        history
            .iter()
            .any(|m| m.message.role == mirri_core::message::Role::Tool
                && m.message.text_content().contains("rewritten-input")),
        "the Echo tool should echo the rewritten input, got {history:?}"
    );
}

// ---------------------------------------------------------------------------
// External command hooks — PostToolUse / PostToolUseFailure lifecycle events
// ---------------------------------------------------------------------------

/// Register a pair of hooks (PostToolUse writing to one marker,
/// PostToolUseFailure writing to the other) and wire them into the turn flow.
fn make_post_tool_hooks_turn_flow(
    post_command: String,
    post_failure_command: String,
) -> (TurnFlow, Arc<HookEngine>) {
    let engine = Arc::new(HookEngine::new());
    engine.register(HookDef::new(HookEventType::PostToolUse, post_command));
    engine.register(HookDef::new(
        HookEventType::PostToolUseFailure,
        post_failure_command,
    ));
    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());
    turn_flow.set_external_hooks(engine.clone(), HashMap::new());
    (turn_flow, engine)
}

fn post_tool_marker(tag: &str) -> std::path::PathBuf {
    // Both PostToolUse lifecycle tests run in parallel within this binary and
    // would otherwise share the same pid-keyed paths, clobbering each other's
    // markers; the counter keeps every call's path unique.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "post_tool_{}_{}_{}.json",
        tag,
        std::process::id(),
        seq
    ));
    let _ = std::fs::remove_file(&path);
    path
}

/// Given a successful tool call, When the result finalizes, Then exactly
/// `PostToolUse` fires (never `PostToolUseFailure`) with the real tool
/// input and the tool output in its payload (TS parity:
/// `notifyPostToolUse`).
#[tokio::test]
async fn should_fire_only_post_tool_use_with_input_and_output_when_tool_succeeds() {
    // Given a PostToolUse hook dumping its input and a PostToolUseFailure
    // hook that must never run
    let success_marker = post_tool_marker("success");
    let failure_marker = post_tool_marker("failure");
    let (mut turn_flow, _engine) = make_post_tool_hooks_turn_flow(
        format!("cat > {:?}", success_marker),
        format!("cat > {:?}", failure_marker),
    );

    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![make_tool_call(
            "call-1",
            "Echo",
            serde_json::json!({"message": "hi"}),
        )]),
        MockResponse::end_turn("done"),
    ]);
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    // When a turn with a tool call runs
    turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Hello".to_string(),
            }],
            PromptOrigin::User,
            memory,
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(llm))),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("turn should succeed");

    // Then only PostToolUse fired, carrying the real input and the output
    assert!(
        success_marker.exists(),
        "PostToolUse hook should have fired"
    );
    assert!(
        !failure_marker.exists(),
        "PostToolUseFailure must not fire for a successful call"
    );
    let content = std::fs::read_to_string(&success_marker).expect("hook input file should exist");
    let input: Value = serde_json::from_str(&content).expect("hook input should be JSON");
    assert_eq!(input["tool_name"], serde_json::json!("Echo"));
    assert_eq!(input["tool_input"]["message"], serde_json::json!("hi"));
    assert_eq!(input["tool_output"], serde_json::json!("hi"));
    let _ = std::fs::remove_file(&success_marker);
}

/// Given a tool call whose result is an error, When the result finalizes,
/// Then exactly `PostToolUseFailure` fires (never `PostToolUse`) with the
/// real tool input and an error payload (TS parity: the failure branch
/// carries `error` and no `tool_output`).
#[tokio::test]
async fn should_fire_only_post_tool_use_failure_when_tool_result_is_error() {
    // Given a denying PreToolUse hook (its denial produces an error tool
    // result) plus the PostToolUse / PostToolUseFailure pair
    let success_marker = post_tool_marker("success");
    let failure_marker = post_tool_marker("failure");
    let (mut turn_flow, engine) = make_post_tool_hooks_turn_flow(
        format!("cat > {:?}", success_marker),
        format!("cat > {:?}", failure_marker),
    );
    engine.register(HookDef::new(
        HookEventType::PreToolUse,
        r#"echo '{"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"not allowed"}}'"#.to_string(),
    ));

    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![make_tool_call(
            "call-1",
            "Echo",
            serde_json::json!({"message": "hi"}),
        )]),
        MockResponse::end_turn("done"),
    ]);
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();

    // When a turn with a tool call runs
    turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Hello".to_string(),
            }],
            PromptOrigin::User,
            memory,
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(llm))),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("turn should succeed");

    // Then only PostToolUseFailure fired, carrying the real input and the
    // error payload
    assert!(
        failure_marker.exists(),
        "PostToolUseFailure hook should have fired"
    );
    assert!(
        !success_marker.exists(),
        "PostToolUse must not fire for an error result"
    );
    let content = std::fs::read_to_string(&failure_marker).expect("hook input file should exist");
    let input: Value = serde_json::from_str(&content).expect("hook input should be JSON");
    assert_eq!(input["tool_name"], serde_json::json!("Echo"));
    assert_eq!(input["tool_input"]["message"], serde_json::json!("hi"));
    assert!(
        input["error"]["message"]
            .as_str()
            .expect("error payload message")
            .contains("not allowed"),
        "error payload carries the deny reason: {input}"
    );
    assert!(
        input.get("tool_output").is_none(),
        "the failure payload must not carry tool_output: {input}"
    );
    let _ = std::fs::remove_file(&failure_marker);
}

// ---------------------------------------------------------------------------
// Session-scoped turn-id allocation: a flow wired to a shared counter draws
// its turn ids from it, so a flow rebuild (dispose → recreate, the
// close→resume shape) never resets the numbering.
// ---------------------------------------------------------------------------

/// Given two flows wired to the same shared turn-id source (a session's
/// allocator seeded past a replayed wire log's maximum), When each runs one
/// turn, Then both ids come from the shared counter in order — the second
/// flow (a rebuild) continues where the first left off instead of restarting
/// at 1.
#[tokio::test]
async fn should_draw_turn_ids_from_the_shared_session_counter_when_one_is_wired() {
    let source = Arc::new(std::sync::atomic::AtomicU64::new(33));
    let tools = ToolManager::new();
    let cancel = CancelToken::new();

    let turn_id = |flow: &mut TurnFlow| {
        flow.set_turn_id_source(Arc::clone(&source));
        let memory = Arc::new(Mutex::new(ContextMemory::new()));
        let mut validators = ValidatorCache::new();
        let llm = MockLlm::new(vec![MockResponse::end_turn("reply")]);
        futures::executor::block_on(flow.run_one_turn(
            vec![ContentPart::Text {
                text: "Hello".to_string(),
            }],
            PromptOrigin::User,
            memory,
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(llm))),
            &cancel,
            &mut validators,
            None,
            None,
        ))
        .expect("turn should succeed")
        .turn_id
    };

    // The first flow draws 33; a REBUILT flow (the close→resume shape — a
    // fresh TurnFlow pointed at the same session counter) draws 34, never
    // restarting at 1.
    let mut first = TurnFlow::new(TurnFlowConfig::default());
    assert_eq!(turn_id(&mut first), 33);
    let mut rebuilt = TurnFlow::new(TurnFlowConfig::default());
    assert_eq!(
        turn_id(&mut rebuilt),
        34,
        "a rebuilt flow must continue the shared counter, not restart at 1"
    );
}

/// Given a flow with NO shared source (a standalone test flow), When turns
/// run, Then the local counter applies exactly as before — the local
/// fallback keeps standalone numbering stable.
#[tokio::test]
async fn should_keep_the_local_turn_id_counter_when_no_source_is_wired() {
    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());
    let tools = ToolManager::new();
    let cancel = CancelToken::new();
    let run = |flow: &mut TurnFlow| {
        let llm = MockLlm::new(vec![MockResponse::end_turn("reply")]);
        let memory = Arc::new(Mutex::new(ContextMemory::new()));
        let mut validators = ValidatorCache::new();
        futures::executor::block_on(flow.run_one_turn(
            vec![ContentPart::Text {
                text: "Hello".to_string(),
            }],
            PromptOrigin::User,
            memory,
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::new(llm))),
            &cancel,
            &mut validators,
            None,
            None,
        ))
        .expect("turn should succeed")
        .turn_id
    };
    assert_eq!(run(&mut turn_flow), 1);
    assert_eq!(run(&mut turn_flow), 2);
}

// ---------------------------------------------------------------------------
// Pending skill-reminder flush (ADR-010 decision 5: park-and-flush)
// ---------------------------------------------------------------------------

/// A `Tool` double named `Skill` so the flush gate (`is_tool_active`) can
/// be exercised without the real skill tool's catalog wiring.
struct StubSkillTool;

impl Tool for StubSkillTool {
    fn name(&self) -> &str {
        "Skill"
    }

    fn description(&self) -> &str {
        "Stub skill tool for the flush gate test"
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
                    summary: "stub skill".to_string(),
                },
                approval_rule: "Skill".to_string(),
                stop_batch_after_this: false,
            },
            execute: Box::new(|_ctx| Box::pin(async move { ToolResult::success("ok") })),
        })
    }
}

/// A tool manager with the `Skill` tool active (the interactive main-agent
/// tool face).
fn make_tool_manager_with_skill() -> ToolManager {
    let mut tm = ToolManager::new();
    tm.set_active_tools(vec!["Skill".to_string()]);
    tm.register_builtin(Arc::new(StubSkillTool));
    tm
}

/// Run one turn with the given flow wiring and return the memory (the
/// reminder lands in the conversation history).
async fn run_flush_turn(
    turn_flow: &mut TurnFlow,
    tools: &ToolManager,
    memory: Arc<Mutex<ContextMemory>>,
) {
    let llm = MockLlm::new(vec![MockResponse::end_turn("Done")]);
    let mut validators = ValidatorCache::new();
    let cancel = CancelToken::new();
    turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Hello".to_string(),
            }],
            PromptOrigin::User,
            memory,
            tools,
            Arc::new(FixedLlmResolver::new(Arc::new(llm))),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("turn should succeed");
}

/// Given a session whose skill catalog was refreshed (pending flagged) and
/// whose tool face carries the Skill tool, when the next turn runs on a
/// flow wired with the pending handle, then the conversation receives ONE
/// system-reminder carrying the fresh listing with its `DISREGARD any
/// earlier skill listings` head and the leading change-summary line, and
/// the wire records the durable `context.append_message` op — once, never
/// twice on a second turn (the pending was consumed).
#[tokio::test]
async fn should_inject_the_refreshed_skill_listing_when_a_pending_turn_starts() {
    use mirri_core::skill::SkillCatalog;
    use mirri_core::skill::SkillSource;

    let catalog = Arc::new(SkillCatalog::new());
    let session = mirri_core::scope::SessionScope::new(
        mirri_core::scope::ids::SessionId::new("session-flush"),
        Default::default(),
        Default::default(),
        Arc::clone(&catalog),
        None,
        None,
    );
    // One skill in the catalog so the listing renders.
    let tmp = tempfile::tempdir().unwrap();
    let skill_dir = tmp.path().join(".agents/skills/flushed-skill");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\ndescription: A flushed skill.\n---\n# Flushed\n",
    )
    .unwrap();
    catalog
        .discover(&tmp.path().join(".agents/skills"), SkillSource::Project)
        .unwrap();
    session.mark_skill_catalog_refreshed();

    let recorded = Arc::new(Mutex::new(Vec::new()));
    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());
    turn_flow.set_wire_recorder(Some(
        Arc::new(CollectingWireRecorder(Arc::clone(&recorded))) as Arc<dyn WireOpRecorder>,
    ));
    turn_flow.set_pending_skill_reminder(Some(session.pending_skill_reminder()));
    let tools = make_tool_manager_with_skill();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));

    run_flush_turn(&mut turn_flow, &tools, Arc::clone(&memory)).await;

    // Then the memory holds exactly one injected reminder with the listing.
    let reminders: Vec<String> = memory
        .lock()
        .unwrap()
        .history()
        .iter()
        .filter(|m| m.origin == Some(PromptOrigin::Injection))
        .map(|m| m.message.text_content())
        .filter(|t| t.contains("DISREGARD any earlier skill listings"))
        .collect();
    assert_eq!(reminders.len(), 1, "exactly one listing reminder");
    let reminder = &reminders[0];
    assert!(
        reminder.contains("<system-reminder>"),
        "the reminder is system-reminder wrapped: {reminder}"
    );
    assert!(
        reminder.contains("Skill catalog updated"),
        "the change-summary line leads the listing: {reminder}"
    );
    assert!(
        reminder.contains("- flushed-skill: A flushed skill."),
        "the current catalog's listing rides the reminder: {reminder}"
    );

    // And the wire recorded the durable append for the reminder.
    {
        let ops = recorded.lock().unwrap();
        let durable = ops
            .iter()
            .filter(|op| matches!(op, WireOp::ContextAppendMessage { message }
                if message["content"][0]["text"]
                    .as_str()
                    .is_some_and(|t| t.contains("DISREGARD any earlier skill listings"))))
            .count();
        assert_eq!(durable, 1, "one durable append for the reminder");
    }

    // And a second turn without a new refresh injects nothing new.
    run_flush_turn(&mut turn_flow, &tools, Arc::clone(&memory)).await;
    let count = memory
        .lock()
        .unwrap()
        .history()
        .iter()
        .filter(|m| m.origin == Some(PromptOrigin::Injection))
        .map(|m| m.message.text_content())
        .filter(|t| t.contains("DISREGARD any earlier skill listings"))
        .count();
    assert_eq!(count, 1, "the consumed pending never re-injects");
}

/// Given a session whose tool face does NOT carry the Skill tool, when a
/// pending turn starts, then no skill reminder is injected (the gate is
/// the tool face — a session that cannot invoke skills gets no listing).
#[tokio::test]
async fn should_not_inject_the_skill_reminder_when_the_skill_tool_is_not_active() {
    use mirri_core::skill::SkillCatalog;

    let catalog = Arc::new(SkillCatalog::new());
    let session = mirri_core::scope::SessionScope::new(
        mirri_core::scope::ids::SessionId::new("session-nogate"),
        Default::default(),
        Default::default(),
        catalog,
        None,
        None,
    );
    session.mark_skill_catalog_refreshed();

    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());
    turn_flow.set_pending_skill_reminder(Some(session.pending_skill_reminder()));
    // No Skill tool registered or active.
    let tools = ToolManager::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));

    run_flush_turn(&mut turn_flow, &tools, Arc::clone(&memory)).await;

    let count = memory
        .lock()
        .unwrap()
        .history()
        .iter()
        .filter(|m| m.origin == Some(PromptOrigin::Injection))
        .count();
    assert_eq!(count, 0, "no reminder without the Skill tool face");
    // The pending flag stays set — the gate refused, not the absence of a
    // refresh (a later turn with the tool active can still flush).
    assert!(session.take_pending_skill_reminder());
}

/// Given a flow with NO pending handle wired (a subagent/cron/goal-driven
/// TurnFlow), when a turn runs against a session with a pending refresh,
/// then nothing is injected and the session's flag is untouched — the
/// non-interactive flows never consume the interactive session's pending.
#[tokio::test]
async fn should_not_inject_or_consume_when_the_flow_carries_no_pending_handle() {
    use mirri_core::skill::SkillCatalog;

    let catalog = Arc::new(SkillCatalog::new());
    let session = mirri_core::scope::SessionScope::new(
        mirri_core::scope::ids::SessionId::new("session-nohandle"),
        Default::default(),
        Default::default(),
        catalog,
        None,
        None,
    );
    session.mark_skill_catalog_refreshed();

    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());
    let tools = make_tool_manager_with_skill();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));

    run_flush_turn(&mut turn_flow, &tools, Arc::clone(&memory)).await;

    let count = memory
        .lock()
        .unwrap()
        .history()
        .iter()
        .filter(|m| m.origin == Some(PromptOrigin::Injection))
        .count();
    assert_eq!(count, 0, "no reminder without the pending handle");
    assert!(
        session.take_pending_skill_reminder(),
        "the session's pending flag was not consumed"
    );
}

/// Given a refresh whose diff left the listing EMPTY (all skills removed),
/// when the pending turn starts, then the reminder is the cleared-catalog
/// declaration — a non-empty text, not a stale listing and not an empty
/// string (ADR-010 decision 4's clear-declaration branch).
#[tokio::test]
async fn should_inject_the_cleared_catalog_declaration_when_the_listing_is_empty() {
    use mirri_core::skill::SkillCatalog;

    let catalog = Arc::new(SkillCatalog::new());
    let session = mirri_core::scope::SessionScope::new(
        mirri_core::scope::ids::SessionId::new("session-cleared"),
        Default::default(),
        Default::default(),
        catalog,
        None,
        None,
    );
    session.mark_skill_catalog_refreshed();

    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());
    turn_flow.set_pending_skill_reminder(Some(session.pending_skill_reminder()));
    let tools = make_tool_manager_with_skill();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));

    run_flush_turn(&mut turn_flow, &tools, Arc::clone(&memory)).await;

    let reminders: Vec<String> = memory
        .lock()
        .unwrap()
        .history()
        .iter()
        .filter(|m| m.origin == Some(PromptOrigin::Injection))
        .map(|m| m.message.text_content())
        .collect();
    assert_eq!(reminders.len(), 1, "exactly one cleared-catalog reminder");
    let reminder = &reminders[0];
    assert!(
        reminder.contains("<system-reminder>"),
        "the cleared declaration is a system reminder: {reminder}"
    );
    assert!(
        reminder.contains("Skill catalog updated"),
        "the change-summary line still leads: {reminder}"
    );
    assert!(
        !reminder.is_empty() && !reminder.contains("- "),
        "no stale listing entries ride the cleared declaration: {reminder}"
    );
    assert!(
        reminder.to_lowercase().contains("no longer available")
            || reminder.to_lowercase().contains("unavailable"),
        "the cleared declaration states the skills are unavailable: {reminder}"
    );
}
