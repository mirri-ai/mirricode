//! Comprehensive tests for the compaction module.
//!
//! These tests cover:
//! - [`CompactionStrategy`] — `should_compact`, `should_block`, reserved context.
//! - [`FullCompaction`] — turn reset, auto-compaction checks, per-turn limits,
//!   overflow observation, background compaction lifecycle.
//! - [`compaction_round`] — basic summary, 4-level fallback chain (media strip,
//!   overflow shrink, drop oldest, retry), prefix-race detection, empty/truncated
//!   handling.
//! - Helper functions — `build_compaction_summary_text`, `post_process_summary`,
//!   `replace_media_with_markers`, `shrink_history_after_overflow`,
//!   `drop_oldest_and_leading_tool_results`.

use std::sync::{Arc, Mutex};

use futures::stream;

use mirri_core::cancel::CancelToken;
use mirri_core::compaction::retention::{RetentionCut, can_split_after, compute_retention_tail};
use mirri_core::compaction::{
    CompactionBeginParams, CompactionRoundInput, CompactionSource, CompactionStrategy,
    FullCompaction, compaction_round, hook_additional_context_messages,
    wait_manual_compaction_settle,
};
use mirri_core::context::ContextMessage;
use mirri_core::context_memory::{
    ApplyOutcome, CompactionInput, ContextMemory, DEFAULT_MAX_DEFERRED_MESSAGES,
    DEFAULT_MAX_HISTORY_MESSAGES, LoopEvent,
};
use mirri_core::error::{EngineError, EngineResult};
use mirri_core::llm::*;
use mirri_core::message::{ContentPart, ImageUrlData, Message, Role, ToolCall, ToolCallFunction};
use mirri_core::origin::PromptOrigin;
use mirri_core::session::event_projection::project;
use mirri_core::session::reconstruct_context_memory;
use mirri_core::tool::ToolOutput;
use mirri_core::tool_registry::ToolManager;
use mirri_core::{SessionEvent, TurnFlow, TurnFlowConfig};
use mirri_proto::state::TimelineEntry;
use mirri_proto::{AgentState, ReplayLimits, WireOp, reduce, reduce_with_limits};

// ---------------------------------------------------------------------------
// Mock LLM
// ---------------------------------------------------------------------------

/// A pre-configured response for the MockLlm to return.
#[derive(Clone)]
struct MockResponse {
    text: String,
    tool_calls: Vec<LlmToolCall>,
    stop_reason: String,
    usage: LlmUsage,
    error: Option<String>,
    /// If true, `chat()` returns an `Err(ProviderError)` instead of a stream.
    chat_error: Option<String>,
}

impl MockResponse {
    fn end_turn(text: &str) -> Self {
        Self {
            text: text.to_string(),
            tool_calls: Vec::new(),
            stop_reason: "completed".to_string(),
            usage: LlmUsage {
                input_tokens: 100,
                output_tokens: 50,
                ..Default::default()
            },
            error: None,
            chat_error: None,
        }
    }

    fn truncated() -> Self {
        Self {
            text: "partial summary...".to_string(),
            tool_calls: Vec::new(),
            stop_reason: "truncated".to_string(),
            usage: LlmUsage {
                input_tokens: 100,
                output_tokens: 50,
                ..Default::default()
            },
            error: None,
            chat_error: None,
        }
    }

    fn empty() -> Self {
        Self {
            text: String::new(),
            tool_calls: Vec::new(),
            stop_reason: "completed".to_string(),
            usage: LlmUsage {
                input_tokens: 100,
                output_tokens: 0,
                ..Default::default()
            },
            error: None,
            chat_error: None,
        }
    }

    #[allow(dead_code)]
    fn stream_error(message: &str) -> Self {
        Self {
            text: String::new(),
            tool_calls: Vec::new(),
            stop_reason: String::new(),
            usage: LlmUsage::default(),
            error: Some(message.to_string()),
            chat_error: None,
        }
    }

    fn chat_error(message: &str) -> Self {
        Self {
            text: String::new(),
            tool_calls: Vec::new(),
            stop_reason: String::new(),
            usage: LlmUsage::default(),
            error: None,
            chat_error: Some(message.to_string()),
        }
    }
}

/// A mock LLM that pops responses from a queue.
struct MockLlm {
    system_prompt: String,
    model_name: String,
    responses: Arc<Mutex<Vec<MockResponse>>>,
    retryable: bool,
    /// Every chat call's messages, in call order — lets tests assert on what
    /// the summarizer (or turn loop) was fed (e.g. hook additional context).
    captured_messages: Arc<Mutex<Vec<Vec<Message>>>>,
}

impl MockLlm {
    fn new(responses: Vec<MockResponse>) -> Self {
        Self {
            system_prompt: "You are a test assistant.".to_string(),
            model_name: "test-model".to_string(),
            responses: Arc::new(Mutex::new(responses)),
            retryable: false,
            captured_messages: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// The messages of the n-th chat call (0-based), empty if out of range.
    fn messages_of_call(&self, call: usize) -> Vec<Message> {
        self.captured_messages
            .lock()
            .unwrap()
            .get(call)
            .cloned()
            .unwrap_or_default()
    }

    /// Number of chat calls the mock has served so far.
    fn call_count(&self) -> usize {
        self.captured_messages.lock().unwrap().len()
    }

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

    fn chat(&self, params: LlmChatParams) -> EngineResult<LlmEventStream> {
        self.captured_messages
            .lock()
            .unwrap()
            .push(params.messages.clone());
        let mut queue = self.responses.lock().unwrap();
        if queue.is_empty() {
            return Err(EngineError::ProviderError(
                "No more mock responses".to_string(),
            ));
        }
        let resp = queue.remove(0);
        drop(queue);

        if let Some(ref err_msg) = resp.chat_error {
            return Err(EngineError::ProviderError(err_msg.clone()));
        }

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
// Test helpers
// ---------------------------------------------------------------------------

/// Build a ContextMemory with two user/assistant exchanges.
///
/// Two exchanges (not one) so a legal retention-tail cut exists: with a
/// single `[user, assistant]` pair the whole history is one indivisible
/// unit and compaction must give up (US2.6).
fn make_memory_with_exchange() -> ContextMemory {
    let mut mem = ContextMemory::new();
    for _ in 0..2 {
        mem.append_user_message(
            vec![ContentPart::Text {
                text: "Please help me write a function".to_string(),
            }],
            PromptOrigin::User,
        );
        mem.append_message(ContextMessage {
            message: Message::assistant("I'll help you write a function."),
            origin: None,
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        });
    }
    mem
}

/// Build a ContextMemory with enough messages to exercise compaction.
fn make_memory_with_many_messages() -> ContextMemory {
    let mut mem = ContextMemory::new();
    for i in 0..10 {
        // Large bodies so the compaction's keep-window (20k tokens of user
        // messages) genuinely trims — a summary of a tiny memory cannot pass
        // the shrink check (P8a-3 变小校验) and is rejected by design.
        mem.append_user_message(
            vec![ContentPart::Text {
                text: format!("User message number {i}: {}", "x".repeat(16_000)),
            }],
            PromptOrigin::User,
        );
        mem.append_message(ContextMessage {
            message: Message::assistant(format!("Assistant response number {}", i)),
            origin: None,
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        });
    }
    mem
}

// ---------------------------------------------------------------------------
// CompactionStrategy tests
// ---------------------------------------------------------------------------

#[test]
fn should_compact_when_token_count_above_trigger_ratio() {
    let strategy = CompactionStrategy {
        max_context_tokens: 100_000,
        trigger_ratio: 0.85,
        ..Default::default()
    };
    // 85% = 85,000 tokens
    assert!(strategy.should_compact(85_000));
    assert!(strategy.should_compact(90_000));
    assert!(strategy.should_compact(100_000));
}

#[test]
fn should_not_compact_when_token_count_below_trigger_ratio() {
    let strategy = CompactionStrategy {
        max_context_tokens: 100_000,
        trigger_ratio: 0.85,
        reserved_context_size: 0,
        ..Default::default()
    };
    assert!(!strategy.should_compact(84_999));
    assert!(!strategy.should_compact(50_000));
    assert!(!strategy.should_compact(0));
}

#[test]
fn should_not_compact_when_max_context_tokens_unknown() {
    let strategy = CompactionStrategy {
        max_context_tokens: 0,
        ..Default::default()
    };
    // When max_context_tokens is 0 (unknown), never trigger by ratio.
    assert!(!strategy.should_compact(1_000_000));
}

#[test]
fn should_block_when_token_count_at_or_above_block_ratio() {
    let strategy = CompactionStrategy {
        max_context_tokens: 100_000,
        block_ratio: 0.85,
        reserved_context_size: 0,
        ..Default::default()
    };
    assert!(strategy.should_block(85_000));
    assert!(strategy.should_block(90_000));
    assert!(!strategy.should_block(84_999));
}

#[test]
fn should_not_block_when_max_context_tokens_unknown() {
    let strategy = CompactionStrategy {
        max_context_tokens: 0,
        ..Default::default()
    };
    assert!(!strategy.should_block(1_000_000));
}

#[test]
fn should_compact_when_reserved_context_plus_tokens_exceeds_max() {
    let strategy = CompactionStrategy {
        max_context_tokens: 100_000,
        trigger_ratio: 0.99, // Set high so only reserved triggers
        reserved_context_size: 50_000,
        ..Default::default()
    };
    // 50,000 + 50,000 = 100,000 >= 100,000 → triggers
    assert!(strategy.should_compact(50_000));
    // 49,000 + 50,000 = 99,000 < 100,000 → no trigger
    assert!(!strategy.should_compact(49_000));
}

#[test]
fn should_not_compact_when_reserved_context_size_is_zero() {
    let strategy = CompactionStrategy {
        max_context_tokens: 100_000,
        trigger_ratio: 0.99,
        reserved_context_size: 0,
        ..Default::default()
    };
    // With reserved_context_size = 0, no reserved-based triggering.
    assert!(!strategy.should_compact(98_000));
}

#[test]
fn should_not_compact_when_reserved_context_exceeds_max_context_tokens() {
    let strategy = CompactionStrategy {
        max_context_tokens: 100_000,
        trigger_ratio: 0.99,
        reserved_context_size: 200_000,
        ..Default::default()
    };
    // reserved > max → no reserved-based triggering.
    assert!(!strategy.should_compact(98_000));
}

// ---------------------------------------------------------------------------
// FullCompaction::reset_for_turn tests
// ---------------------------------------------------------------------------

#[test]
fn should_reset_compaction_counters_when_turn_resets() {
    let strategy = CompactionStrategy {
        max_context_tokens: 100_000,
        ..Default::default()
    };
    let mut fc = FullCompaction::new(strategy);

    // Simulate state from a previous turn.
    fc.set_compaction_count_in_turn_for_test(3);
    fc.observe_context_overflow(90_000);

    // Reset.
    fc.reset_for_turn();

    assert_eq!(fc.compaction_count_in_turn(), 0);
    assert_eq!(fc.consecutive_overflow_compactions(), 0);
    // observed_max_context_tokens persists across turns (learned).
    assert!(fc.observed_max_context_tokens().is_some());
}

// ---------------------------------------------------------------------------
// FullCompaction::check_auto_compaction tests
// ---------------------------------------------------------------------------

#[test]
fn should_trigger_auto_compaction_when_token_count_exceeds_trigger_ratio() {
    let strategy = CompactionStrategy {
        max_context_tokens: 100_000,
        trigger_ratio: 0.85,
        ..Default::default()
    };
    let mut fc = FullCompaction::new(strategy);
    let mut mem = ContextMemory::new();

    // Add a large message so token_count_with_pending exceeds 85,000.
    // Each ASCII char ≈ 0.25 tokens, so we need ~340,000 chars for 85,000 tokens.
    let big_text = "a".repeat(340_000);
    mem.append_user_message(
        vec![ContentPart::Text { text: big_text }],
        PromptOrigin::User,
    );

    let result = fc.check_auto_compaction(&mem, false).unwrap();
    assert!(result);
}

#[test]
fn should_drive_compaction_pressure_from_cached_usage() {
    // Given a strategy whose trigger threshold sits at 250 tokens
    let strategy = CompactionStrategy {
        max_context_tokens: 250,
        trigger_ratio: 1.0,
        reserved_context_size: 0,
        ..Default::default()
    };
    let mut fc = FullCompaction::new(strategy);
    let mut mem = ContextMemory::new();

    // And a settled step whose usage carries non-zero cache buckets on top
    // of a plain input/output pair: 15 + 5 + 200 + 40 = 260 (the pruned
    // projection of the history also clears 250, so the prune-first check
    // does not mask the trigger)
    mem.append_user_message(
        vec![ContentPart::Text {
            text: "a".repeat(1_200), // ≈ 300 estimated tokens
        }],
        PromptOrigin::User,
    );
    mem.append_loop_event(LoopEvent::StepBegin {
        step_id: "step-1".to_string(),
        entry_seq: None,
    });
    mem.append_loop_event(LoopEvent::ContentPart {
        step_id: "step-1".to_string(),
        text: "answer".to_string(),
    });
    mem.append_loop_event(LoopEvent::StepEnd {
        step_id: "step-1".to_string(),
        input_tokens: 15,
        output_tokens: 5,
        cache_read_tokens: 200,
        cache_creation_tokens: 40,
    });

    // Then the cached usage counts as context pressure: the four-bucket
    // total (260) crosses the threshold. Under the old input+output-only
    // formula the pressure read 20 and compaction never fired on this
    // cache-heavy session (F7; contract 5 three-bucket evidence).
    assert_eq!(mem.token_count_with_pending(), 260);
    let result = fc.check_auto_compaction(&mem, false).unwrap();
    assert!(
        result,
        "cached buckets must drive the auto-compaction trigger"
    );
}

#[tokio::test]
async fn should_skip_auto_compaction_when_no_new_content_since_last_check() {
    // US1.7 无新内容不重复压缩: after a successful compaction, a re-check
    // with no new content must not trigger again; new content re-arms it.
    //
    // The strategy is tuned so the guard is the ONLY gate that can suppress
    // the re-check: the trigger threshold (0.5 × 100k = 50k tokens) stays
    // below the post-compaction token count (the ~85k-token big user
    // message is retained verbatim in the retention tail), so threshold,
    // pruning, and per-turn-limit gates all pass — deleting the
    // `last_compacted_token_count` baseline check would flip phase 3 red.
    let strategy = CompactionStrategy {
        max_context_tokens: 100_000,
        trigger_ratio: 0.5,
        reserved_context_size: 0,
        ..Default::default()
    };
    let mut fc = FullCompaction::new(strategy);
    let mut mem = make_memory_with_many_messages();

    // ~40k tokens of exchanges plus an ~85k-token user message. The big
    // message lands in the retention tail, so the post-compaction history
    // still sits above the 50k trigger threshold.
    let big_text = "a".repeat(340_000);
    mem.append_user_message(
        vec![ContentPart::Text { text: big_text }],
        PromptOrigin::User,
    );

    let tokens_before = mem.token_count_with_pending();

    // Phase 1 — the pressure is real: the check wants to compact.
    assert!(
        fc.check_auto_compaction(&mem, false).unwrap(),
        "the oversized history must trigger auto-compaction"
    );

    // Phase 2 — run a full begin→block cycle; the apply must succeed and
    // set the `last_compacted_token_count` baseline (block() writes it from
    // the post-apply token count).
    let llm: Arc<dyn Llm> = Arc::new(MockLlm::new(vec![MockResponse::end_turn(
        "Summary of the conversation.",
    )]));
    let cancel = CancelToken::new();

    fc.begin(
        CompactionSource::Auto,
        CompactionBeginParams {
            llm,
            cancel: cancel.clone(),
            custom_instruction: None,
            max_output_size: None,
            hook_additional_context: Vec::new(),
        },
        &mem,
    )
    .unwrap();

    let result = fc.block(&mut mem, &cancel).await.unwrap();
    assert!(
        result.applied,
        "the compaction must apply — otherwise no baseline is set and the guard is untested"
    );
    assert!(result.tokens_after < tokens_before);
    assert!(!fc.is_compacting());

    // The retained history still exceeds the trigger threshold, so only the
    // no-new-content guard can suppress a re-compaction here.
    let tokens_after = mem.token_count_with_pending();
    assert!(
        tokens_after >= 50_000,
        "post-compaction tokens ({tokens_after}) must stay above the 50k trigger \
         threshold, otherwise the skip below would be caused by the threshold, \
         not the no-new-content guard"
    );

    // Phase 3 — no new content since the compaction: must NOT re-compact.
    assert_eq!(
        mem.token_count_with_pending(),
        tokens_after,
        "no content may be appended between the compaction and this check"
    );
    assert!(
        !fc.check_auto_compaction(&mem, false).unwrap(),
        "no new content since the last compaction must skip re-compaction (US1.7)"
    );

    // Phase 4 — one new user message re-arms the check: the token count has
    // grown beyond the post-compaction baseline.
    mem.append_user_message(
        vec![ContentPart::Text {
            text: "One more thing.".to_string(),
        }],
        PromptOrigin::User,
    );
    assert!(
        fc.check_auto_compaction(&mem, false).unwrap(),
        "new content since the last compaction must re-arm auto-compaction"
    );
}

#[test]
fn should_not_trigger_auto_compaction_when_token_count_below_threshold() {
    let strategy = CompactionStrategy {
        max_context_tokens: 100_000,
        trigger_ratio: 0.85,
        ..Default::default()
    };
    let mut fc = FullCompaction::new(strategy);
    let mem = ContextMemory::new();

    // Empty memory → token count is 0 → below threshold.
    let result = fc.check_auto_compaction(&mem, false).unwrap();
    assert!(!result);
}

#[test]
fn should_throw_error_when_max_compaction_per_turn_exceeded_with_throw_enabled() {
    let strategy = CompactionStrategy {
        max_context_tokens: 100_000,
        trigger_ratio: 0.85,
        max_compaction_per_turn: 2,
        ..Default::default()
    };
    let mut fc = FullCompaction::new(strategy);
    let mut mem = ContextMemory::new();

    let big_text = "a".repeat(340_000);
    mem.append_user_message(
        vec![ContentPart::Text { text: big_text }],
        PromptOrigin::User,
    );

    // First two compactions are allowed.
    assert!(fc.check_auto_compaction(&mem, false).unwrap());

    // Simulate two compactions having happened.
    fc.set_compaction_count_in_turn_for_test(2);

    // Third compaction with throw_on_limit=true → error.
    let result = fc.check_auto_compaction(&mem, true);
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(matches!(err, EngineError::ContextOverflow { .. }));

    // With throw_on_limit=false → just returns false.
    let result = fc.check_auto_compaction(&mem, false).unwrap();
    assert!(!result);
}

#[tokio::test]
async fn should_skip_auto_compaction_when_already_compacting() {
    let strategy = CompactionStrategy {
        max_context_tokens: 100_000,
        trigger_ratio: 0.85,
        ..Default::default()
    };
    let mut fc = FullCompaction::new(strategy);

    // A memory with a legal retention cut and enough tokens to trigger —
    // begin refuses (US2.6) on a history with no legal cut.
    let mut mem = make_memory_with_many_messages();

    // Simulate an active compaction by calling begin.
    // We need an Arc<dyn Llm> for this.
    let llm: Arc<dyn Llm> = Arc::new(MockLlm::new(vec![MockResponse::end_turn("summary")]));
    let cancel = CancelToken::new();

    // Add enough tokens to trigger.
    let big_text = "a".repeat(340_000);
    mem.append_user_message(
        vec![ContentPart::Text { text: big_text }],
        PromptOrigin::User,
    );

    fc.begin(
        CompactionSource::Auto,
        CompactionBeginParams {
            llm,
            cancel,
            custom_instruction: None,
            max_output_size: None,
            hook_additional_context: Vec::new(),
        },
        &mem,
    )
    .unwrap();

    assert!(fc.is_compacting());

    // While compacting, check_auto_compaction returns false.
    let result = fc.check_auto_compaction(&mem, false).unwrap();
    assert!(!result);
}

// ---------------------------------------------------------------------------
// FullCompaction::observe_context_overflow tests
// ---------------------------------------------------------------------------

#[test]
fn should_learn_max_context_tokens_when_overflow_observed() {
    let strategy = CompactionStrategy::default();
    let mut fc = FullCompaction::new(strategy);

    fc.observe_context_overflow(85_000);

    // observed = floor(85000 * 0.85) = 72250 (conservative: below the
    // true limit, which is itself below the failed request's 85000)
    let observed = fc.observed_max_context_tokens().unwrap();
    assert_eq!(observed, 72_250);
}

#[test]
fn should_keep_smaller_observed_max_when_multiple_overflows_observed() {
    let strategy = CompactionStrategy::default();
    let mut fc = FullCompaction::new(strategy);

    fc.observe_context_overflow(85_000); // → 72,250
    fc.observe_context_overflow(170_000); // → 144,500

    // Should keep the smaller (72,250).
    assert_eq!(fc.observed_max_context_tokens().unwrap(), 72_250);
}

#[test]
fn should_return_min_of_configured_and_observed_when_both_present() {
    let strategy = CompactionStrategy {
        max_context_tokens: 150_000,
        ..Default::default()
    };
    let mut fc = FullCompaction::new(strategy);

    assert_eq!(fc.effective_max_context_tokens(), 150_000);

    fc.observe_context_overflow(85_000); // observed = 72,250

    // min(150,000, 72,250) = 72,250
    assert_eq!(fc.effective_max_context_tokens(), 72_250);
}

#[test]
fn should_return_observed_max_when_configured_max_is_zero() {
    let strategy = CompactionStrategy {
        max_context_tokens: 0,
        ..Default::default()
    };
    let mut fc = FullCompaction::new(strategy);

    assert_eq!(fc.effective_max_context_tokens(), 0);

    fc.observe_context_overflow(85_000);
    assert_eq!(fc.effective_max_context_tokens(), 72_250);
}

#[test]
fn should_update_effective_window_in_place_when_model_switches() {
    let mut fc = FullCompaction::new(CompactionStrategy {
        max_context_tokens: 0,
        ..Default::default()
    });
    assert_eq!(fc.effective_max_context_tokens(), 0);

    // A model switch installs the new window…
    fc.set_max_context_tokens(200_000);
    assert_eq!(fc.effective_max_context_tokens(), 200_000);

    // …and the observed (learned) window still wins as the min:
    fc.observe_context_overflow(85_000); // observed = 72,250
    assert_eq!(fc.effective_max_context_tokens(), 72_250);

    // A smaller configured window takes over again:
    fc.set_max_context_tokens(50_000);
    assert_eq!(fc.effective_max_context_tokens(), 50_000);
}

// ---------------------------------------------------------------------------
// FullCompaction::handle_overflow_error tests
// ---------------------------------------------------------------------------

#[test]
fn should_increment_overflow_counter_when_overflow_error_handled() {
    let strategy = CompactionStrategy {
        max_overflow_compaction_attempts: 3,
        ..Default::default()
    };
    let mut fc = FullCompaction::new(strategy);
    let mem = ContextMemory::new();
    let cancel = CancelToken::new();

    assert_eq!(fc.consecutive_overflow_compactions(), 0);

    fc.handle_overflow_error(&mem, &cancel, 85_000).unwrap();
    assert_eq!(fc.consecutive_overflow_compactions(), 1);

    fc.handle_overflow_error(&mem, &cancel, 85_000).unwrap();
    assert_eq!(fc.consecutive_overflow_compactions(), 2);

    fc.handle_overflow_error(&mem, &cancel, 85_000).unwrap();
    assert_eq!(fc.consecutive_overflow_compactions(), 3);

    // 4th attempt > 3 → error.
    let result = fc.handle_overflow_error(&mem, &cancel, 85_000);
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        EngineError::ContextOverflow { .. }
    ));
}

/// Given `max_overflow_compaction_attempts = 0`, when the first overflow
/// error is handled, then it is refused outright (`ContextOverflow`) with no
/// recovery budget — and the zero cap disables ONLY the overflow-recovery
/// path: threshold-triggered auto compaction still fires and a manual
/// compaction still begins (US1.3-C4).
#[tokio::test]
async fn should_disable_only_overflow_recovery_when_max_overflow_attempts_is_zero() {
    let strategy = CompactionStrategy {
        max_context_tokens: 100_000,
        trigger_ratio: 0.85,
        max_overflow_compaction_attempts: 0,
        ..Default::default()
    };
    let mut fc = FullCompaction::new(strategy);
    let mut mem = make_memory_with_many_messages();
    let big_text = "a".repeat(340_000);
    mem.append_user_message(
        vec![ContentPart::Text { text: big_text }],
        PromptOrigin::User,
    );
    let cancel = CancelToken::new();

    // The FIRST overflow already exhausts the zero budget: the recovery
    // call errors instead of granting a compaction attempt. The overflow
    // still propagates — it is never silently retried through compaction.
    let result = fc.handle_overflow_error(&mem, &cancel, 85_000);
    assert!(
        matches!(result, Err(EngineError::ContextOverflow { attempts: 1 })),
        "cap=0 must fail on the first overflow, got {:?}",
        result.err()
    );
    assert_eq!(fc.consecutive_overflow_compactions(), 1);

    // The zero cap must not leak into the threshold-triggered auto path:
    // the same memory still passes the auto-compaction check.
    assert!(
        fc.check_auto_compaction(&mem, false).unwrap(),
        "threshold-triggered auto compaction must survive cap=0"
    );

    // And a manual compaction still begins — the manual path never
    // consults the overflow counter (a legal cut exists, US2.6).
    fc.begin(
        CompactionSource::Manual,
        CompactionBeginParams {
            llm: Arc::new(MockLlm::new(vec![MockResponse::end_turn("summary")])),
            cancel: cancel.clone(),
            custom_instruction: None,
            max_output_size: None,
            hook_additional_context: Vec::new(),
        },
        &mem,
    )
    .expect("manual compaction must begin with cap=0");
    assert!(fc.manual_in_flight());

    // Settle the manual attempt so the orchestrator is left clean.
    let handle = fc.take_manual_task().expect("manual task handed out");
    let _ = handle.task.await;
    fc.settle_manual(handle.attempt_id);
    assert!(!fc.manual_in_flight());
}

// ---------------------------------------------------------------------------
// FullCompaction::is_compacting tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_return_false_when_idle() {
    let fc = FullCompaction::new(CompactionStrategy::default());
    assert!(!fc.is_compacting());
}

#[tokio::test]
async fn should_set_and_clear_compacting_flag_when_begin_and_block_called() {
    let strategy = CompactionStrategy {
        max_context_tokens: 100_000,
        trigger_ratio: 0.85,
        ..Default::default()
    };
    let mut fc = FullCompaction::new(strategy);

    // A memory with a legal retention cut (begin refuses on a history
    // with no legal cut — US2.6).
    let mut mem = make_memory_with_many_messages();

    let big_text = "a".repeat(340_000);
    mem.append_user_message(
        vec![ContentPart::Text { text: big_text }],
        PromptOrigin::User,
    );

    let llm: Arc<dyn Llm> = Arc::new(MockLlm::new(vec![MockResponse::end_turn("summary")]));
    let cancel = CancelToken::new();

    fc.begin(
        CompactionSource::Auto,
        CompactionBeginParams {
            llm,
            cancel: cancel.clone(),
            custom_instruction: None,
            max_output_size: None,
            hook_additional_context: Vec::new(),
        },
        &mem,
    )
    .unwrap();

    assert!(fc.is_compacting());

    let result = fc.block(&mut mem, &cancel).await;
    assert!(result.is_ok());

    assert!(!fc.is_compacting());
}

// ---------------------------------------------------------------------------
// FullCompaction begin/block integration test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_apply_compaction_when_begin_and_block_called() {
    let strategy = CompactionStrategy {
        max_context_tokens: 100_000,
        trigger_ratio: 0.85,
        ..Default::default()
    };
    let mut fc = FullCompaction::new(strategy);
    let mut mem = make_memory_with_many_messages();

    // Add enough tokens to trigger.
    let big_text = "a".repeat(340_000);
    mem.append_user_message(
        vec![ContentPart::Text { text: big_text }],
        PromptOrigin::User,
    );

    let tokens_before = mem.token_count_with_pending();

    let llm: Arc<dyn Llm> = Arc::new(MockLlm::new(vec![MockResponse::end_turn(
        "Summary of the conversation.",
    )]));
    let cancel = CancelToken::new();

    fc.begin(
        CompactionSource::Auto,
        CompactionBeginParams {
            llm,
            cancel: cancel.clone(),
            custom_instruction: None,
            max_output_size: None,
            hook_additional_context: Vec::new(),
        },
        &mem,
    )
    .unwrap();

    let result = fc.block(&mut mem, &cancel).await.unwrap();

    assert!(!result.summary.is_empty());
    assert!(result.tokens_after < tokens_before);
    assert!(!fc.is_compacting());

    // History should be much shorter after compaction.
    assert!(mem.history().len() < 20);
}

// ---------------------------------------------------------------------------
// compaction_round basic test
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_produce_summary_when_compaction_round_runs() {
    let llm = MockLlm::new(vec![MockResponse::end_turn("This is the summary.")]);
    let mut memory = make_memory_with_many_messages();
    let cancel = CancelToken::new();
    let tools = ToolManager::new();

    let tokens_before = memory.token_count_with_pending();
    assert!(tokens_before > 0, "memory should have non-zero tokens");

    let result = compaction_round(CompactionRoundInput {
        llm: &llm,
        memory: &mut memory,
        cancel: &cancel,
        tools: &tools,
        custom_instruction: None,
        max_output_size: None,
    })
    .await
    .expect("compaction should succeed");

    assert_eq!(result.summary, "This is the summary.");
    assert!(result.context_summary.contains("compacted"));
    assert!(result.context_summary.contains("This is the summary."));
    assert!(result.compacted_count > 0);
    // History should be shorter after compaction (10 user messages kept + 1 summary).
    assert!(memory.history().len() < 20);
}

// ---------------------------------------------------------------------------
// compaction_round with custom instruction
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_produce_summary_when_custom_instruction_provided() {
    let llm = MockLlm::new(vec![MockResponse::end_turn("Summary with instruction.")]);
    let mut memory = make_memory_with_exchange();
    let cancel = CancelToken::new();
    let tools = ToolManager::new();

    let result = compaction_round(CompactionRoundInput {
        llm: &llm,
        memory: &mut memory,
        cancel: &cancel,
        tools: &tools,
        custom_instruction: Some("Focus on the file paths mentioned."),
        max_output_size: None,
    })
    .await
    .expect("compaction should succeed");

    assert_eq!(result.summary, "Summary with instruction.");
}

// ---------------------------------------------------------------------------
// compaction_round media strip fallback (Level 1)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_strip_media_and_retry_when_413_error_received() {
    // First call returns a 413 error, second succeeds.
    let llm = MockLlm::new(vec![
        MockResponse::chat_error("413 Request Entity Too Large: image too large"),
        MockResponse::end_turn("Summary after media strip."),
    ]);
    let mut memory = ContextMemory::new();
    memory.append_user_message(
        vec![
            ContentPart::Text {
                text: "Check this image".to_string(),
            },
            ContentPart::ImageUrl {
                image_url: ImageUrlData {
                    url: "data:image/png;base64,abc".to_string(),
                    detail: None,
                    id: None,
                },
                detail: None,
            },
        ],
        PromptOrigin::User,
    );
    memory.append_message(ContextMessage {
        message: Message::assistant("I see the image."),
        origin: None,
        is_error: false,
        note: None,
        with_skills: None,
        entry_seq: 0,
    });
    // A follow-up exchange so a legal retention-tail cut exists — with
    // only `[image user, assistant]` the whole history is one indivisible
    // unit and the round must give up (US2.6) before reaching the media
    // fallback chain.
    memory.append_user_message(
        vec![ContentPart::Text {
            text: "And now the follow-up".to_string(),
        }],
        PromptOrigin::User,
    );
    memory.append_message(ContextMessage {
        message: Message::assistant("Noted."),
        origin: None,
        is_error: false,
        note: None,
        with_skills: None,
        entry_seq: 0,
    });
    let cancel = CancelToken::new();
    let tools = ToolManager::new();

    let result = compaction_round(CompactionRoundInput {
        llm: &llm,
        memory: &mut memory,
        cancel: &cancel,
        tools: &tools,
        custom_instruction: None,
        max_output_size: None,
    })
    .await
    .expect("compaction should succeed with media strip");

    assert_eq!(result.summary, "Summary after media strip.");
}

// ---------------------------------------------------------------------------
// compaction_round overflow shrink fallback (Level 2)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_shrink_history_and_retry_when_context_overflow_received() {
    // First call returns context overflow, second succeeds.
    // Use a message that matches overflow but NOT media/image error.
    let llm = MockLlm::new(vec![
        MockResponse::chat_error("context length exceeded maximum context window"),
        MockResponse::end_turn("Summary after shrink."),
    ]);
    let mut memory = make_memory_with_many_messages();
    let cancel = CancelToken::new();
    let tools = ToolManager::new();

    let result = compaction_round(CompactionRoundInput {
        llm: &llm,
        memory: &mut memory,
        cancel: &cancel,
        tools: &tools,
        custom_instruction: None,
        max_output_size: None,
    })
    .await
    .expect("compaction should succeed with overflow shrink");

    assert_eq!(result.summary, "Summary after shrink.");
    assert!(result.dropped_count > 0);
}

// ---------------------------------------------------------------------------
// compaction_round overflow shrink ladder (Level 2 exact ratios)
// ---------------------------------------------------------------------------

/// A ladder-ratio test message body: exactly 40 ASCII characters, so the
/// estimator heuristic (`ceil(ascii_chars / 4)`, see token_estimator.rs)
/// prices every message at exactly 10 tokens.
fn ladder_message_text(prefix: &str, i: usize) -> String {
    let head = format!("{prefix}{i:02}");
    debug_assert_eq!(head.len(), 3);
    format!("{head}{}", "x".repeat(37))
}

#[tokio::test]
async fn should_shrink_to_exact_ladder_ratios_when_overflow_retries_escalate() {
    // Given three consecutive overflow errors and a fixture of uniform
    // messages, each exactly 40 ASCII chars = 10 estimated tokens.
    //
    // The sync round's retention cut (budget 1 token, cut never after a
    // user message) leaves the old segment = everything up to a9, i.e.
    // exactly the first 20 of the 22 messages. The L2 ladder applies its
    // ratio to the CURRENT working history on each rung (the rung-N shrink
    // shrinks the rung-(N-1) survivor, not the original segment):
    //
    //   rung 1 (ratio 0.7): 20 msgs = 200 tokens, budget 200×0.7 = 140
    //                       -> keeps 14 msgs from the tail (140/10)
    //   rung 2 (ratio 0.5): 14 msgs = 140 tokens, budget 140×0.5 = 70
    //                       -> keeps 7 msgs
    //   rung 3 (ratio 0.35): 7 msgs = 70 tokens, budget 70×0.35 = 24
    //                       -> keeps 2 msgs (2×10 ≤ 24 < 3×10)
    //
    // All arithmetic above is independent of the production helpers: the
    // budgets come from the fixture's literal sizes × ADR §5.3's ratios,
    // and each mock call carries the projected history + 1 instruction.
    let overflow = "context length exceeded maximum context window";
    let llm = MockLlm::new(vec![
        MockResponse::chat_error(overflow),
        MockResponse::chat_error(overflow),
        MockResponse::chat_error(overflow),
        MockResponse::end_turn("Summary after full ladder."),
    ]);
    let mut memory = ContextMemory::new();
    for i in 0..11 {
        memory.append_user_message(
            vec![ContentPart::Text {
                text: ladder_message_text("u", i),
            }],
            PromptOrigin::User,
        );
        memory.append_message(ContextMessage {
            message: Message::assistant(ladder_message_text("a", i)),
            origin: None,
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        });
    }
    let cancel = CancelToken::new();
    let tools = ToolManager::new();

    // When the round runs through all three shrink rungs
    let result = compaction_round(CompactionRoundInput {
        llm: &llm,
        memory: &mut memory,
        cancel: &cancel,
        tools: &tools,
        custom_instruction: None,
        max_output_size: None,
    })
    .await
    .expect("compaction should succeed after the full shrink ladder");

    // Then each attempt's kept count matches the exact ratio arithmetic
    // (history messages + 1 instruction per call).
    assert_eq!(llm.call_count(), 4);
    assert_eq!(
        llm.messages_of_call(0).len(),
        21,
        "first attempt must see the whole 20-message old segment + instruction"
    );
    assert_eq!(
        llm.messages_of_call(1).len(),
        15,
        "after rung 1 (0.7 of 200 tokens) exactly 14 messages survive"
    );
    assert_eq!(
        llm.messages_of_call(2).len(),
        8,
        "after rung 2 (0.5 of the surviving 140 tokens) exactly 7 survive"
    );
    assert_eq!(
        llm.messages_of_call(3).len(),
        3,
        "after rung 3 (0.35 of the surviving 70 tokens) exactly 2 survive"
    );

    // And the cumulative drops line up with the same arithmetic:
    // 6 (rung 1) + 7 (rung 2) + 5 (rung 3) = 18 of 20 old-segment slots,
    // recorded as ascending positions 0..=17.
    assert_eq!(result.dropped_count as usize, 18);
    let expected_positions: Vec<usize> = (0..18).collect();
    assert_eq!(result.dropped_entry_positions, expected_positions);
    assert_eq!(result.summary, "Summary after full ladder.");
}

#[tokio::test]
async fn should_stop_shrinking_when_level_two_budget_reached() {
    // Given four consecutive overflow errors (budget = 3): the L2 ladder
    // shrinks on the first three (ratios 0.7, 0.5, 0.35) and gives up on
    // the fourth — no fifth LLM call, the error surfaces as
    // `ContextOverflow { attempts: 4 }`.
    let overflow = "context length exceeded maximum context window";
    let responses: Vec<MockResponse> = (0..4).map(|_| MockResponse::chat_error(overflow)).collect();
    let llm = MockLlm::new(responses);
    let mut memory = make_memory_with_many_messages();
    let cancel = CancelToken::new();
    let tools = ToolManager::new();

    let result = compaction_round(CompactionRoundInput {
        llm: &llm,
        memory: &mut memory,
        cancel: &cancel,
        tools: &tools,
        custom_instruction: None,
        max_output_size: None,
    })
    .await;

    match result {
        Err(EngineError::ContextOverflow { attempts }) => {
            assert_eq!(attempts, 4);
        }
        other => panic!("expected L2 budget exhaustion on the 4th overflow, got {other:?}"),
    }
    // Exactly four calls: initial attempt + 3 shrink retries; the ladder
    // never starts a fifth against the exhausted queue (a boundary bug
    // would surface as "No more mock responses" instead).
    assert_eq!(llm.call_count(), 4);
}

// ---------------------------------------------------------------------------
// compaction_round dropped entry positions (overflow-shrink reporting)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_report_dropped_entry_positions_when_overflow_shrink_discards_oldest() {
    // Given a compaction round whose summarizer LLM overflows once so the L2
    // shrink ladder drops the oldest messages. The error matches the Level 2
    // overflow trigger but NOT the Level 1 media/image one. The old segment's
    // messages carry distinct, known entry seqs (Task 4 maps positions
    // through them; this test only pins the positions).
    let llm = MockLlm::new(vec![
        MockResponse::chat_error("context length exceeded maximum context window"),
        MockResponse::end_turn("Summary after shrink."),
    ]);
    let mut memory = ContextMemory::new();
    for i in 0..10 {
        // Large bodies so the compaction's keep-window genuinely trims —
        // same construction rationale as make_memory_with_many_messages
        // (the shrink check 变小校验 rejects a summary of a tiny memory).
        memory.append_message(ContextMessage {
            message: Message::user(format!("User message number {i}: {}", "x".repeat(16_000))),
            origin: Some(PromptOrigin::User),
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: (i * 2) as u64,
        });
        memory.append_message(ContextMessage {
            message: Message::assistant(format!("Assistant response number {i}")),
            origin: None,
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: (i * 2 + 1) as u64,
        });
    }
    let cancel = CancelToken::new();
    let tools = ToolManager::new();

    // Snapshot the old segment exactly as the round will cut it (the sync
    // round path's retention budget is the overflow-recovery floor: 1
    // token), then predict the L2 ladder's first shrink with the same
    // public helper the round uses.
    let history_before: Vec<ContextMessage> = memory.history().to_vec();
    let cut = compute_retention_tail(&history_before, 1).expect("legal retention-tail cut");
    let old_segment = &history_before[..cut.tail_start];
    let kept = mirri_core::compaction::round::shrink_history_after_overflow(old_segment, 1);
    let expected_dropped = old_segment.len() - kept.len();
    assert!(
        expected_dropped > 0,
        "the L2 ladder must actually drop something"
    );

    // When the round completes
    let result = compaction_round(CompactionRoundInput {
        llm: &llm,
        memory: &mut memory,
        cancel: &cancel,
        tools: &tools,
        custom_instruction: None,
        max_output_size: None,
    })
    .await
    .expect("compaction should succeed with overflow shrink");

    // Then dropped_entry_positions lists exactly the dropped indices in the
    // old segment — ascending, since both drop sites remove a prefix — and
    // the count matches dropped_count.
    let expected_positions: Vec<usize> = (0..expected_dropped).collect();
    assert_eq!(result.dropped_entry_positions, expected_positions);
    assert_eq!(result.dropped_count as usize, expected_dropped);
    assert_eq!(result.dropped_entry_positions.len(), result.dropped_count);
}

// ---------------------------------------------------------------------------
// compaction_round drop oldest fallback (Level 3)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_drop_oldest_messages_and_retry_when_truncated_response_received() {
    // First call returns truncated, second succeeds.
    let llm = MockLlm::new(vec![
        MockResponse::truncated(),
        MockResponse::end_turn("Summary after drop."),
    ]);
    let mut memory = make_memory_with_many_messages();
    let cancel = CancelToken::new();
    let tools = ToolManager::new();

    let result = compaction_round(CompactionRoundInput {
        llm: &llm,
        memory: &mut memory,
        cancel: &cancel,
        tools: &tools,
        custom_instruction: None,
        max_output_size: None,
    })
    .await
    .expect("compaction should succeed with drop oldest");

    assert_eq!(result.summary, "Summary after drop.");
    assert!(result.dropped_count > 0);
}

// ---------------------------------------------------------------------------
// compaction_round empty response fallback (Level 3)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_retry_when_empty_response_received() {
    // First call returns empty, second succeeds.
    let llm = MockLlm::new(vec![
        MockResponse::empty(),
        MockResponse::end_turn("Summary after empty."),
    ]);
    let mut memory = make_memory_with_many_messages();
    let cancel = CancelToken::new();
    let tools = ToolManager::new();

    let result = compaction_round(CompactionRoundInput {
        llm: &llm,
        memory: &mut memory,
        cancel: &cancel,
        tools: &tools,
        custom_instruction: None,
        max_output_size: None,
    })
    .await
    .expect("compaction should succeed after empty response");

    assert_eq!(result.summary, "Summary after empty.");
}

// ---------------------------------------------------------------------------
// compaction_round drop-oldest budget exhaustion (Level 3 boundary)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_stop_dropping_oldest_when_level_three_budget_reached() {
    // Given six consecutive truncated responses (budget = 5): the ladder
    // drops the oldest message after each of the first five and gives up on
    // the sixth — the error propagates and no seventh LLM call is made.
    let responses: Vec<MockResponse> = (0..6).map(|_| MockResponse::truncated()).collect();
    let llm = MockLlm::new(responses);
    let mut memory = make_memory_with_many_messages();
    let cancel = CancelToken::new();
    let tools = ToolManager::new();

    let result = compaction_round(CompactionRoundInput {
        llm: &llm,
        memory: &mut memory,
        cancel: &cancel,
        tools: &tools,
        custom_instruction: None,
        max_output_size: None,
    })
    .await;

    // Then the sixth truncated response fails the round with the
    // exhaustion error (not a provider error, not a queue-exhaustion
    // "No more mock responses" — which is what a 7th attempt would hit).
    match result {
        Err(EngineError::CompactionFailed(msg)) => {
            assert_eq!(msg, "compaction summary truncated too many times");
        }
        other => {
            panic!("expected L3 budget exhaustion on the 6th truncated response, got {other:?}")
        }
    }

    // And exactly six calls were made — the budget-5 ladder never starts a
    // seventh attempt.
    assert_eq!(llm.call_count(), 6);

    // The first five calls each dropped exactly one oldest message: the
    // 20-message fixture's retention cut leaves an 18-message old segment,
    // so the call histories shrink 19 -> 14 (history + instruction per
    // call, one message gone per drop).
    let counts: Vec<usize> = (0..6).map(|c| llm.messages_of_call(c).len()).collect();
    assert_eq!(counts, vec![19, 18, 17, 16, 15, 14]);
}

// ---------------------------------------------------------------------------
// compaction_round retry fallback (Level 4)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_retry_when_retryable_error_received() {
    // First call returns a retryable error, second succeeds.
    let llm = MockLlm::new(vec![
        MockResponse::chat_error("429 Too Many Requests"),
        MockResponse::end_turn("Summary after retry."),
    ])
    .with_retryable(true);
    let mut memory = make_memory_with_exchange();
    let cancel = CancelToken::new();
    let tools = ToolManager::new();

    let result = compaction_round(CompactionRoundInput {
        llm: &llm,
        memory: &mut memory,
        cancel: &cancel,
        tools: &tools,
        custom_instruction: None,
        max_output_size: None,
    })
    .await
    .expect("compaction should succeed after retry");

    assert_eq!(result.summary, "Summary after retry.");
}

// ---------------------------------------------------------------------------
// compaction_round retry exhausted
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_return_error_when_retries_exhausted() {
    // All calls return retryable errors.
    let responses: Vec<MockResponse> = (0..10)
        .map(|_| MockResponse::chat_error("429 Too Many Requests"))
        .collect();
    let llm = MockLlm::new(responses).with_retryable(true);
    let mut memory = make_memory_with_exchange();
    let cancel = CancelToken::new();
    let tools = ToolManager::new();

    let result = compaction_round(CompactionRoundInput {
        llm: &llm,
        memory: &mut memory,
        cancel: &cancel,
        tools: &tools,
        custom_instruction: None,
        max_output_size: None,
    })
    .await;

    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// compaction_round backoff budget exhaustion (Level 4 boundary)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_stop_retrying_when_level_four_budget_reached() {
    // Given consecutive retryable errors with a mock queue long enough for
    // a seventh attempt (so a boundary bug cannot hide behind queue
    // exhaustion). The L4 budget is 5 total attempts: the round performs 4
    // backoff retries and propagates the 5th error verbatim.
    //
    // NOTE (code-vs-doc finding, see the task-2 report): the audit brief
    // describes the boundary as "6th retryable error gives up" (matching
    // Level 3's shape), but the code's guard is
    // `retry_count + 1 >= MAX_COMPACTION_RETRY_ATTEMPTS`, which gives up on
    // the 5th error. This test pins the code's behavior as implemented.
    let responses: Vec<MockResponse> = (0..7)
        .map(|_| MockResponse::chat_error("429 Too Many Requests"))
        .collect();
    let llm = MockLlm::new(responses).with_retryable(true);
    let mut memory = make_memory_with_exchange();
    let cancel = CancelToken::new();
    let tools = ToolManager::new();

    let result = compaction_round(CompactionRoundInput {
        llm: &llm,
        memory: &mut memory,
        cancel: &cancel,
        tools: &tools,
        custom_instruction: None,
        max_output_size: None,
    })
    .await;

    // Then the 5th retryable error propagates verbatim...
    match result {
        Err(EngineError::ProviderError(msg)) => {
            assert_eq!(msg, "429 Too Many Requests");
        }
        other => {
            panic!("expected the L4 budget to give up with the 5th retryable error, got {other:?}")
        }
    }
    // ...after exactly 5 LLM calls (initial attempt + 4 backoff retries —
    // no 6th call against the still-stocked mock queue).
    assert_eq!(llm.call_count(), 5);
}

// ---------------------------------------------------------------------------
// compaction_round non-retryable error
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_return_error_when_non_retryable_error_received() {
    let llm = MockLlm::new(vec![MockResponse::chat_error("400 Bad Request")]);
    let mut memory = make_memory_with_exchange();
    let cancel = CancelToken::new();
    let tools = ToolManager::new();

    let result = compaction_round(CompactionRoundInput {
        llm: &llm,
        memory: &mut memory,
        cancel: &cancel,
        tools: &tools,
        custom_instruction: None,
        max_output_size: None,
    })
    .await;

    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// compaction_round cancelled
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_return_error_when_cancelled() {
    let llm = MockLlm::new(vec![MockResponse::end_turn("Summary")]);
    let mut memory = make_memory_with_exchange();
    let cancel = CancelToken::new();
    let tools = ToolManager::new();

    cancel.cancel();

    let result = compaction_round(CompactionRoundInput {
        llm: &llm,
        memory: &mut memory,
        cancel: &cancel,
        tools: &tools,
        custom_instruction: None,
        max_output_size: None,
    })
    .await;

    assert!(result.is_err());
}

// ---------------------------------------------------------------------------
// compaction_round prefix-race detection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_not_trigger_prefix_race_when_history_unchanged() {
    // When history hasn't changed, compaction succeeds normally.
    let llm = MockLlm::new(vec![MockResponse::end_turn("Summary.")]);
    let mut memory = make_memory_with_exchange();
    let cancel = CancelToken::new();
    let tools = ToolManager::new();

    let result = compaction_round(CompactionRoundInput {
        llm: &llm,
        memory: &mut memory,
        cancel: &cancel,
        tools: &tools,
        custom_instruction: None,
        max_output_size: None,
    })
    .await;

    assert!(result.is_ok());
}

// ---------------------------------------------------------------------------
// build_compaction_summary_text tests
// ---------------------------------------------------------------------------

#[test]
fn should_contain_summary_text_when_body_provided() {
    let text = mirri_core::compaction::round::build_compaction_summary_text("The summary text.");
    assert!(text.starts_with("The conversation so far has been compacted"));
    assert!(text.contains("The summary text."));
}

#[test]
fn should_show_no_summary_available_when_body_is_empty() {
    let text = mirri_core::compaction::round::build_compaction_summary_text("");
    assert!(text.starts_with("The conversation so far has been compacted"));
    assert!(text.contains("(no summary available)"));
}

#[test]
fn should_show_no_summary_available_when_body_is_whitespace_only() {
    let text = mirri_core::compaction::round::build_compaction_summary_text("   \n\t  ");
    assert!(text.contains("(no summary available)"));
}

// ---------------------------------------------------------------------------
// post_process_summary tests
// ---------------------------------------------------------------------------

#[test]
fn should_return_summary_unchanged_when_no_todos_provided() {
    let result = mirri_core::compaction::round::post_process_summary("My summary.", None);
    assert_eq!(result, "My summary.");
}

#[test]
fn should_return_summary_unchanged_when_todos_list_is_empty() {
    let result = mirri_core::compaction::round::post_process_summary("My summary.", Some(&[]));
    assert_eq!(result, "My summary.");
}

#[test]
fn should_append_todos_when_todos_provided() {
    let todos = vec![
        "Task A".to_string(),
        "Task B".to_string(),
        "Task C".to_string(),
    ];
    let result = mirri_core::compaction::round::post_process_summary("My summary.", Some(&todos));
    assert!(result.contains("My summary."));
    assert!(result.contains("## TODO List"));
    assert!(result.contains("- Task A"));
    assert!(result.contains("- Task B"));
    assert!(result.contains("- Task C"));
}

// ---------------------------------------------------------------------------
// replace_media_with_markers tests
// ---------------------------------------------------------------------------

#[test]
fn should_replace_media_with_text_markers_when_media_present() {
    let history = vec![ContextMessage {
        message: Message {
            role: Role::User,
            name: None,
            content: vec![
                ContentPart::Text {
                    text: "Check these".to_string(),
                },
                ContentPart::ImageUrl {
                    image_url: ImageUrlData {
                        url: "http://example.com/img.png".to_string(),
                        detail: None,
                        id: None,
                    },
                    detail: None,
                },
                ContentPart::AudioUrl {
                    audio_url: "http://example.com/audio.mp3".to_string(),
                },
                ContentPart::VideoUrl {
                    video_url: "http://example.com/video.mp4".to_string(),
                },
            ],
            tool_calls: Vec::new(),
            tool_call_id: None,
            partial: false,
        },
        origin: Some(PromptOrigin::User),
        is_error: false,
        note: None,
        with_skills: None,
        entry_seq: 0,
    }];

    let result = mirri_core::compaction::round::replace_media_with_markers(&history);
    let content = &result[0].message.content;

    assert_eq!(content.len(), 4);
    assert!(matches!(&content[0], ContentPart::Text { text } if text == "Check these"));
    assert!(matches!(&content[1], ContentPart::Text { text } if text == "[image]"));
    assert!(matches!(&content[2], ContentPart::Text { text } if text == "[audio]"));
    assert!(matches!(&content[3], ContentPart::Text { text } if text == "[video]"));
}

#[test]
fn should_keep_text_unchanged_when_no_media_present() {
    let history = vec![ContextMessage {
        message: Message::user("Just text"),
        origin: Some(PromptOrigin::User),
        is_error: false,
        note: None,
        with_skills: None,
        entry_seq: 0,
    }];

    let result = mirri_core::compaction::round::replace_media_with_markers(&history);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].message.content.len(), 1);
    assert!(
        matches!(&result[0].message.content[0], ContentPart::Text { text } if text == "Just text")
    );
}

// ---------------------------------------------------------------------------
// shrink_history_after_overflow tests
// ---------------------------------------------------------------------------

#[test]
fn should_keep_recent_messages_when_history_shrunk() {
    let mut history = Vec::new();
    for i in 0..10 {
        history.push(ContextMessage {
            message: Message::assistant(format!("Message {}", i)),
            origin: None,
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        });
    }

    let result = mirri_core::compaction::round::shrink_history_after_overflow(&history, 1);
    // Should keep some messages (ratio 0.7).
    assert!(!result.is_empty());
    assert!(result.len() <= history.len());
    // Should keep the most recent messages.
    let last_msg = result.last().unwrap();
    assert!(last_msg.message.text_content().contains("Message 9"));
}

#[test]
fn should_return_empty_when_shrink_called_with_empty_history() {
    let result = mirri_core::compaction::round::shrink_history_after_overflow(&[], 1);
    assert!(result.is_empty());
}

#[test]
fn should_keep_fewer_messages_when_attempt_number_increases() {
    // Build messages with distinct token counts so we can verify the shrink.
    let mut history = Vec::new();
    for i in 0..20 {
        // Each message has ~25 tokens (100 chars / 4).
        history.push(ContextMessage {
            message: Message::assistant(format!("{:0100}", i)),
            origin: None,
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        });
    }

    let attempt1 = mirri_core::compaction::round::shrink_history_after_overflow(&history, 1);
    let attempt2 = mirri_core::compaction::round::shrink_history_after_overflow(&history, 2);
    let attempt3 = mirri_core::compaction::round::shrink_history_after_overflow(&history, 3);

    // Each attempt should keep fewer or equal messages.
    assert!(attempt1.len() >= attempt2.len());
    assert!(attempt2.len() >= attempt3.len());
}

// ---------------------------------------------------------------------------
// drop_oldest_and_leading_tool_results tests
// ---------------------------------------------------------------------------

#[test]
fn should_drop_oldest_message_and_leading_tool_results_when_present() {
    let history = vec![
        ContextMessage {
            message: Message::user("first user message"),
            origin: Some(PromptOrigin::User),
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        },
        ContextMessage {
            message: Message {
                role: Role::Tool,
                name: None,
                content: vec![ContentPart::Text {
                    text: "tool result".to_string(),
                }],
                tool_calls: Vec::new(),
                tool_call_id: Some("call_1".to_string()),
                partial: false,
            },
            origin: None,
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        },
        ContextMessage {
            message: Message::assistant("response"),
            origin: None,
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        },
    ];

    let result = mirri_core::compaction::round::drop_oldest_and_leading_tool_results(&history);
    // Should drop the first message and the leading tool result.
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].message.role, Role::Assistant);
}

#[test]
fn should_return_empty_when_drop_oldest_called_with_empty_history() {
    let result = mirri_core::compaction::round::drop_oldest_and_leading_tool_results(&[]);
    assert!(result.is_empty());
}

#[test]
fn should_return_empty_when_history_has_single_message() {
    let history = vec![ContextMessage {
        message: Message::user("only message"),
        origin: Some(PromptOrigin::User),
        is_error: false,
        note: None,
        with_skills: None,
        entry_seq: 0,
    }];

    let result = mirri_core::compaction::round::drop_oldest_and_leading_tool_results(&history);
    assert!(result.is_empty());
}

// ---------------------------------------------------------------------------
// drop_leading_tool_results tests
// ---------------------------------------------------------------------------

#[test]
fn should_drop_leading_tool_results_when_present() {
    let history = vec![
        ContextMessage {
            message: Message::tool_result("call_1", "result 1"),
            origin: None,
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        },
        ContextMessage {
            message: Message::tool_result("call_2", "result 2"),
            origin: None,
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        },
        ContextMessage {
            message: Message::user("user message"),
            origin: Some(PromptOrigin::User),
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        },
    ];

    let result = mirri_core::compaction::round::drop_leading_tool_results(&history);
    assert_eq!(result.len(), 1);
    assert_eq!(result[0].message.role, Role::User);
}

#[test]
fn should_keep_all_messages_when_no_leading_tool_results() {
    let history = vec![
        ContextMessage {
            message: Message::user("first"),
            origin: Some(PromptOrigin::User),
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        },
        ContextMessage {
            message: Message::tool_result("call_1", "result"),
            origin: None,
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        },
    ];

    let result = mirri_core::compaction::round::drop_leading_tool_results(&history);
    assert_eq!(result.len(), 2);
}

// ---------------------------------------------------------------------------
// strip_dynamic_tool_context tests
// ---------------------------------------------------------------------------

#[test]
fn should_filter_injection_origin_messages_when_present() {
    let history = vec![
        ContextMessage {
            message: Message::user("real user message"),
            origin: Some(PromptOrigin::User),
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        },
        ContextMessage {
            message: Message::user("<system-reminder>injection</system-reminder>"),
            origin: Some(PromptOrigin::Injection),
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        },
        ContextMessage {
            message: Message::assistant("response"),
            origin: None,
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        },
    ];

    let result = mirri_core::compaction::round::strip_dynamic_tool_context(&history);
    // Injection-origin messages should be filtered out.
    assert_eq!(result.len(), 2);
    assert_eq!(result[0].message.role, Role::User);
    assert_eq!(result[1].message.role, Role::Assistant);
}

#[test]
fn should_keep_all_messages_when_no_injection_origin_present() {
    let history = vec![
        ContextMessage {
            message: Message::user("user msg"),
            origin: Some(PromptOrigin::User),
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        },
        ContextMessage {
            message: Message::user("skill activation"),
            origin: Some(PromptOrigin::SkillActivation {
                activation_id: None,
                skill_name: None,
                trigger: None,
                skill_args: None,
                skill_path: None,
                skill_source: None,
            }),
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        },
        ContextMessage {
            message: Message::user("compaction summary"),
            origin: Some(PromptOrigin::CompactionSummary),
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        },
    ];

    let result = mirri_core::compaction::round::strip_dynamic_tool_context(&history);
    // All non-injection origins are kept.
    assert_eq!(result.len(), 3);
}

// ---------------------------------------------------------------------------
// prefix_race_check tests
// ---------------------------------------------------------------------------

#[test]
fn should_return_true_when_history_unchanged() {
    let history = vec![ContextMessage {
        message: Message::user("hello"),
        origin: Some(PromptOrigin::User),
        is_error: false,
        note: None,
        with_skills: None,
        entry_seq: 0,
    }];

    let result = mirri_core::compaction::round::prefix_race_check(&history, &history);
    assert!(result);
}

#[test]
fn should_return_true_when_history_grew_with_user_input() {
    let original = vec![ContextMessage {
        message: Message::user("hello"),
        origin: Some(PromptOrigin::User),
        is_error: false,
        note: None,
        with_skills: None,
        entry_seq: 0,
    }];
    let current = vec![
        ContextMessage {
            message: Message::user("hello"),
            origin: Some(PromptOrigin::User),
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        },
        ContextMessage {
            message: Message::user("new message"),
            origin: Some(PromptOrigin::User),
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        },
    ];

    let result = mirri_core::compaction::round::prefix_race_check(&original, &current);
    assert!(result);
}

#[test]
fn should_return_false_when_history_grew_with_injection() {
    let original = vec![ContextMessage {
        message: Message::user("hello"),
        origin: Some(PromptOrigin::User),
        is_error: false,
        note: None,
        with_skills: None,
        entry_seq: 0,
    }];
    let current = vec![
        ContextMessage {
            message: Message::user("hello"),
            origin: Some(PromptOrigin::User),
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        },
        ContextMessage {
            message: Message::user("injection"),
            origin: Some(PromptOrigin::Injection),
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        },
    ];

    let result = mirri_core::compaction::round::prefix_race_check(&original, &current);
    assert!(!result);
}

#[test]
fn should_return_false_when_history_shrunk() {
    let original = vec![
        ContextMessage {
            message: Message::user("hello"),
            origin: Some(PromptOrigin::User),
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        },
        ContextMessage {
            message: Message::assistant("response"),
            origin: None,
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        },
    ];
    let current = vec![ContextMessage {
        message: Message::user("hello"),
        origin: Some(PromptOrigin::User),
        is_error: false,
        note: None,
        with_skills: None,
        entry_seq: 0,
    }];

    let result = mirri_core::compaction::round::prefix_race_check(&original, &current);
    assert!(!result);
}

#[test]
fn should_return_false_when_prefix_changed() {
    let original = vec![ContextMessage {
        message: Message::user("hello"),
        origin: Some(PromptOrigin::User),
        is_error: false,
        note: None,
        with_skills: None,
        entry_seq: 0,
    }];
    let current = vec![ContextMessage {
        message: Message::user("different"),
        origin: Some(PromptOrigin::User),
        is_error: false,
        note: None,
        with_skills: None,
        entry_seq: 0,
    }];

    let result = mirri_core::compaction::round::prefix_race_check(&original, &current);
    assert!(!result);
}

// ---------------------------------------------------------------------------
// CompactionStrategy::default tests
// ---------------------------------------------------------------------------

#[test]
fn should_have_default_values_when_strategy_created() {
    let s = CompactionStrategy::default();
    assert_eq!(s.trigger_ratio, 0.85);
    assert_eq!(s.block_ratio, 0.85);
    assert_eq!(s.reserved_context_size, 50_000);
    assert_eq!(s.max_compaction_per_turn, u32::MAX);
    assert_eq!(s.max_overflow_compaction_attempts, 3);
    assert_eq!(s.max_context_tokens, 0);
    // M5: the retention-tail budget defaults to the ADR-001 ratio, with no
    // absolute override in play.
    assert_eq!(
        s.retention_tail_ratio,
        mirri_core::compaction::RETENTION_TAIL_RATIO
    );
    assert_eq!(s.retention_tail_tokens, None);
}

// ---------------------------------------------------------------------------
// M5 retention-tail budget configuration (ratio / absolute, US6.2)
// ---------------------------------------------------------------------------

#[test]
fn should_reject_config_when_both_retention_ratio_and_tokens_are_set() {
    // Given a [loop_control] section with both retention_tail_ratio and
    // retention_tail_tokens
    let raw = mirri_core::config::ServerConfig::from_toml(
        "[loop_control]\nretention_tail_ratio = 0.2\nretention_tail_tokens = 5000",
    );
    // When the config loads
    // Then it is rejected with a conflict error naming both keys
    let err = raw.expect_err("mutual exclusion must reject");
    assert!(err.to_string().contains("retention_tail_ratio"));
    assert!(err.to_string().contains("retention_tail_tokens"));
}

#[test]
fn should_reject_config_when_retention_ratio_out_of_range() {
    // Given retention_tail_ratio values outside (0, 1) — the ratio must
    // be a proper fraction of the model window. TOML spellings, so the
    // float literals stay valid (`nan`/`inf` are TOML floats too).
    for bad_ratio in ["1.5", "1.0", "0.0", "-0.25", "nan", "inf"] {
        // When the config loads
        let raw = mirri_core::config::ServerConfig::from_toml(&format!(
            "[loop_control]\nretention_tail_ratio = {bad_ratio}"
        ));
        // Then it is rejected with an error naming the key
        let err = raw.expect_err("out-of-range retention ratio must reject");
        assert!(
            err.to_string().contains("retention_tail_ratio"),
            "error for ratio {bad_ratio} should name retention_tail_ratio: {err}"
        );
    }
    // And an in-range ratio still parses
    assert!(
        mirri_core::config::ServerConfig::from_toml("[loop_control]\nretention_tail_ratio = 0.99")
            .is_ok()
    );
}

#[test]
fn should_use_absolute_retention_budget_when_retention_tokens_configured() {
    // Given a compaction orchestrator with retention_tail_tokens = 5000
    // (no ratio set) and a known window
    let strategy = CompactionStrategy {
        max_context_tokens: 100_000,
        retention_tail_tokens: Some(5_000),
        ..Default::default()
    };
    let compaction = FullCompaction::new(strategy);
    // When the retention budget is computed
    let budget = compaction.retention_budget_tokens();
    // Then the absolute value wins over the default ratio (0.16 × 100_000 = 16_000)
    assert_eq!(budget, 5_000);
}

// ---------------------------------------------------------------------------
// PreCompact hook additional context — request-side plumbing
// ---------------------------------------------------------------------------

/// should produce no message when the hook additional context is blank
#[test]
fn should_produce_no_message_when_hook_additional_context_is_blank() {
    // Given blank or whitespace-only hook output
    // Then no summarizer-side message is built
    assert!(hook_additional_context_messages("").is_empty());
    assert!(hook_additional_context_messages("   \n  ").is_empty());
}

/// should build one tagged user message when hook additional context present
#[test]
fn should_build_one_tagged_user_message_when_hook_additional_context_present() {
    // Given a non-empty additionalContext payload
    let messages = hook_additional_context_messages("focus on the payment module");

    // Then exactly one user message carries it, tagged with the
    // hook_additional_context origin
    assert_eq!(messages.len(), 1);
    assert_eq!(
        messages[0].origin,
        Some(PromptOrigin::HookAdditionalContext)
    );
    assert_eq!(messages[0].message.role, Role::User);
    let text = match &messages[0].message.content[0] {
        ContentPart::Text { text } => text.as_str(),
        other => panic!("expected text content, got {other:?}"),
    };
    assert_eq!(text, "focus on the payment module");
}

/// should feed hook additional context to the summarizer when begin carries it
#[tokio::test]
async fn should_feed_hook_additional_context_to_summarizer_when_begin_carries_it() {
    // Given a memory with history and a compaction begun with a PreCompact
    // hook's additional context riding on the request
    let mut mem = make_memory_with_many_messages();
    let mock = Arc::new(MockLlm::new(vec![MockResponse::end_turn(
        "Summary of the conversation.",
    )]));
    let llm: Arc<dyn Llm> = mock.clone();
    let cancel = CancelToken::new();
    let mut fc = FullCompaction::new(CompactionStrategy::default());

    let hook_ctx = hook_additional_context_messages("HINT prioritize payment logic");
    fc.begin(
        CompactionSource::Auto,
        CompactionBeginParams {
            llm: llm.clone(),
            cancel: cancel.clone(),
            custom_instruction: None,
            max_output_size: None,
            hook_additional_context: hook_ctx,
        },
        &mem,
    )
    .unwrap();
    let result = fc.block(&mut mem, &cancel).await.unwrap();

    // Then the summarizer request included the hook context, and the
    // compaction applied cleanly
    assert!(!result.summary.is_empty());
    // (the mock handle was kept above)
    let summarizer_request = mock.messages_of_call(0);
    let saw_hint = summarizer_request.iter().any(|m| {
        m.content
            .iter()
            .any(|p| matches!(p, ContentPart::Text { text } if text.contains("HINT prioritize payment logic")))
    });
    assert!(
        saw_hint,
        "summarizer request missing hook additional context: {summarizer_request:?}"
    );

    // And the hook context never landed in session memory
    let leaked = mem.history().iter().any(|m| {
        m.message
            .content
            .iter()
            .any(|p| matches!(p, ContentPart::Text { text } if text.contains("HINT prioritize")))
    });
    assert!(
        !leaked,
        "hook additional context leaked into session memory"
    );
}

// ---------------------------------------------------------------------------
// Manual-compaction settle wait (buffer phase)
// ---------------------------------------------------------------------------

/// should return immediately when no manual compaction is in flight
#[tokio::test]
async fn should_return_immediately_when_no_manual_compaction_in_flight() {
    // Given an idle orchestrator
    let compaction: Arc<Mutex<FullCompaction>> = Arc::new(Mutex::new(FullCompaction::new(
        CompactionStrategy::default(),
    )));

    // When the settle wait runs, it returns without parking
    tokio::time::timeout(
        tokio::time::Duration::from_secs(1),
        wait_manual_compaction_settle(&compaction),
    )
    .await
    .expect("idle wait must not park");
}

/// should wake the buffered turn when a manual compaction settles
#[tokio::test]
async fn should_return_from_wait_when_manual_compaction_settles() {
    // Given a manual compaction in flight (registered, task spawned)
    let mut mem = make_memory_with_exchange();
    let mock = Arc::new(MockLlm::new(vec![MockResponse::end_turn("summary")]));
    let cancel = CancelToken::new();
    let mut fc = FullCompaction::new(CompactionStrategy::default());
    fc.begin(
        CompactionSource::Manual,
        CompactionBeginParams {
            llm: mock.clone(),
            cancel,
            custom_instruction: None,
            max_output_size: None,
            hook_additional_context: Vec::new(),
        },
        &mem,
    )
    .unwrap();
    let handle = fc.take_manual_task().expect("manual task handed out");
    let compaction: Arc<Mutex<FullCompaction>> = Arc::new(Mutex::new(fc));

    // When the settle wait parks and the attempt settles
    let wait_compaction = Arc::clone(&compaction);
    let wait_task =
        tokio::spawn(async move { wait_manual_compaction_settle(&wait_compaction).await });
    assert!(
        compaction
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .manual_in_flight()
    );
    // Let the wait task run to its park point (a JoinHandle poll does not
    // drive the task — the runtime must schedule it), then it is parked on
    // the settle watch while the compaction is in flight.
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    assert!(
        compaction
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .manual_in_flight(),
        "compaction must still be in flight while the wait is parked"
    );
    compaction
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .settle_manual(handle.attempt_id);

    // Then the wait returns
    tokio::time::timeout(tokio::time::Duration::from_secs(1), wait_task)
        .await
        .expect("wait must wake on settle")
        .expect("wait task must not panic");
    // The background summarizer task runs to completion (mock is instant).
    let _ = handle.task.await;
    let _ = &mut mem;
}

/// Given a threshold-triggered AUTO compaction in flight on a turn flow with
/// no turn active, when a manual compaction is requested, then the
/// single-flight guard rejects it ("a compaction is already in flight") —
/// the mixed auto×manual branch of `request_manual_compaction`'s
/// `is_compacting || manual_in_flight` check (US1.5). The auto-vs-auto skip
/// and manual-vs-manual rejection are covered separately; this pins the
/// cross-product. Once the auto attempt settles (take + await + apply, the
/// same sequence the before_step hook runs), a manual begin is accepted.
#[tokio::test]
async fn should_reject_manual_begin_while_an_auto_compaction_is_in_flight() {
    let strategy = CompactionStrategy {
        max_context_tokens: 100_000,
        trigger_ratio: 0.85,
        ..Default::default()
    };
    let flow = TurnFlow::new(TurnFlowConfig {
        compaction_strategy: strategy,
        ..Default::default()
    });
    // The memory lives behind the shared `Arc<Mutex<>>` — the manual
    // request seam takes the guard itself (after the compaction lock, the
    // documented order), so the test feeds it the Arc like the embedded
    // dispatch does.
    let mem = Arc::new(std::sync::Mutex::new(make_memory_with_many_messages()));
    {
        let mut mem = mem.lock().unwrap_or_else(|e| e.into_inner());
        let big_text = "a".repeat(340_000);
        mem.append_user_message(
            vec![ContentPart::Text { text: big_text }],
            PromptOrigin::User,
        );
    }
    let cancel = CancelToken::new();

    // No turn is active — the refusal below must come from the in-flight
    // guard, not the turn-active branch.
    assert!(!flow.is_turn_active());

    // A threshold-triggered AUTO compaction begins (the begin the
    // before_step hook would make inside a turn; the spawned task is
    // parked on the instant mock, but the registration stays held until
    // the caller takes it — exactly the window a manual begin can hit).
    let compaction = flow.compaction_handle();
    {
        let mut comp = compaction.lock().unwrap_or_else(|e| e.into_inner());
        let mem = mem.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            comp.check_auto_compaction(&mem, false).unwrap(),
            "the memory must be under threshold pressure"
        );
        comp.begin(
            CompactionSource::Auto,
            CompactionBeginParams {
                llm: Arc::new(MockLlm::new(vec![MockResponse::end_turn("auto summary")])),
                cancel: cancel.clone(),
                custom_instruction: None,
                max_output_size: None,
                hook_additional_context: Vec::new(),
            },
            &mem,
        )
        .unwrap();
        assert!(comp.is_compacting(), "the auto attempt must be in flight");
    }

    // The manual request is rejected by the single-flight guard.
    let refused = flow.request_manual_compaction(
        &mem,
        Arc::new(MockLlm::new(vec![MockResponse::end_turn("manual summary")])),
        None,
        None,
        Vec::new(),
    );
    match refused {
        Err(EngineError::CompactionFailed(reason)) => {
            assert!(
                reason.contains("a compaction is already in flight"),
                "the mixed auto×manual branch must reject with the in-flight reason, got: {reason}"
            );
        }
        other => panic!(
            "a manual begin while an auto compaction runs must be rejected, got: {:?}",
            other.map(|_| ())
        ),
    }

    // Once the auto compaction settles (take + await + apply, as the turn
    // loop does), the guard no longer rejects: with fresh exchanges after
    // the auto apply, the manual begin is accepted outright.
    let task = compaction
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take_active_task()
        .expect("the auto task is still registered");
    let result = task.await.expect("auto task must not panic").unwrap();
    {
        let mut comp = compaction.lock().unwrap_or_else(|e| e.into_inner());
        let mut mem = mem.lock().unwrap_or_else(|e| e.into_inner());
        comp.apply_compaction_result(&mut mem, result);
        assert!(
            !comp.is_compacting(),
            "settling the auto attempt must clear the in-flight registration"
        );
    }
    for _ in 0..2 {
        let mut mem = mem.lock().unwrap_or_else(|e| e.into_inner());
        mem.append_user_message(
            vec![ContentPart::Text {
                text: format!("Post-compaction follow-up: {}", "y".repeat(100_000)),
            }],
            PromptOrigin::User,
        );
        mem.append_message(ContextMessage {
            message: Message::assistant("Post-compaction reply."),
            origin: None,
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        });
    }
    let accepted = flow.request_manual_compaction(
        &mem,
        Arc::new(MockLlm::new(vec![MockResponse::end_turn("manual summary")])),
        None,
        None,
        Vec::new(),
    );
    let handle = accepted.expect("a manual begin after the auto settles must be accepted");

    // Settle the manual attempt so the orchestrator is left clean.
    let _ = handle.task.await;
    compaction
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .settle_manual(handle.attempt_id);
}

// ---------------------------------------------------------------------------
// Prune-first compaction check (P8a-2 修剪优先于摘要)
// ---------------------------------------------------------------------------

/// Build a memory whose history contains one old tool exchange with a huge
/// tool result, followed by `recent` small user messages. The tool result
/// sits before the micro-compaction cutoff (len - keep_recent), so the
/// pruned projection replaces it with the placeholder.
fn memory_with_old_huge_tool_result(recent: usize, result_chars: usize) -> ContextMemory {
    use mirri_core::context_memory::LoopEvent;
    use mirri_core::tool::ToolOutput;

    let mut mem = ContextMemory::new();
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
        // Settled assistant messages (never merged by projection) so the
        // projected array keeps one message per entry — mirroring a real
        // session's interleaved history.
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
    mem
}

#[test]
fn should_trigger_auto_compaction_when_pressure_lives_in_old_tool_results() {
    // Given a memory whose raw token measure trips the trigger, with the
    // pressure concentrated in one old huge tool result (25 messages, well
    // past any micro-compaction keep-recent window)
    let mem = memory_with_old_huge_tool_result(21, 400_000);
    assert!(
        mem.token_count_with_pending() >= 85_000,
        "raw measure must trip the 85k trigger, got {}",
        mem.token_count_with_pending()
    );
    // The normal projection does NOT prune old tool results anymore — the
    // prefix mutation destroyed the provider's prompt cache (re-billing the
    // whole conversation tail on every request) — so the old tool result
    // stays intact in the request projection.
    let projected = mem.messages();
    let tool_msg = projected
        .iter()
        .find(|m| m.role == Role::Tool)
        .expect("the old tool result must still be in the projection");
    assert_eq!(
        tool_msg.text_content(),
        "x".repeat(400_000),
        "the old tool result must survive the normal projection intact"
    );
    assert!(
        mem.pruned_request_token_estimate() >= 85_000,
        "the un-pruned measure must still trip the trigger, got {}",
        mem.pruned_request_token_estimate()
    );

    // When the auto-compaction check runs
    let strategy = CompactionStrategy {
        max_context_tokens: 100_000,
        trigger_ratio: 0.85,
        ..Default::default()
    };
    let mut fc = FullCompaction::new(strategy);

    // Then it proceeds to compact: without the prune-first relief, the
    // summarizer call is the pressure release.
    assert!(fc.check_auto_compaction(&mem, false).unwrap());
}

#[test]
fn should_still_trigger_auto_compaction_when_pressure_lives_in_recent_messages() {
    use mirri_core::context_memory::LoopEvent;
    // Given a memory whose pressure comes from RECENT messages (inside the
    // micro-compaction keep window — untouchable by pruning)
    let mut mem = memory_with_old_huge_tool_result(21, 400);
    for i in 0..10 {
        let step_id = format!("step-big-{i}");
        mem.append_loop_event(LoopEvent::StepBegin {
            step_id: step_id.clone(),
            entry_seq: None,
        });
        mem.append_loop_event(LoopEvent::ContentPart {
            step_id: step_id.clone(),
            text: format!("big recent message {i}: {}", "y".repeat(40_000)),
        });
        mem.append_loop_event(LoopEvent::StepEnd {
            step_id,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        });
    }
    assert!(mem.token_count_with_pending() >= 85_000);
    assert!(mem.pruned_request_token_estimate() >= 85_000);

    // When the auto-compaction check runs
    let strategy = CompactionStrategy {
        max_context_tokens: 100_000,
        trigger_ratio: 0.85,
        ..Default::default()
    };
    let mut fc = FullCompaction::new(strategy);

    // Then it still triggers — pruning cannot relieve this pressure.
    assert!(fc.check_auto_compaction(&mem, false).unwrap());
}

// ---------------------------------------------------------------------------
// T11a: the media materialization hook on the compaction path
// ---------------------------------------------------------------------------

/// A counting LLM for the compaction materialization hook: one successful
/// summarizer response, with `chat` and `materialize_media` call counters.
struct MaterializeCountingLlm {
    chat_calls: std::sync::atomic::AtomicU32,
    materialize_calls: std::sync::atomic::AtomicU32,
}

impl MaterializeCountingLlm {
    fn new() -> Self {
        Self {
            chat_calls: std::sync::atomic::AtomicU32::new(0),
            materialize_calls: std::sync::atomic::AtomicU32::new(0),
        }
    }
}

impl Llm for MaterializeCountingLlm {
    fn system_prompt(&self) -> &str {
        "You are a test assistant."
    }

    fn model_name(&self) -> &str {
        "counting"
    }

    fn capability(&self) -> &ModelCapability {
        static CAP: std::sync::OnceLock<ModelCapability> = std::sync::OnceLock::new();
        CAP.get_or_init(ModelCapability::default)
    }

    fn chat(&self, _params: LlmChatParams) -> EngineResult<LlmEventStream> {
        use std::sync::atomic::Ordering;
        self.chat_calls.fetch_add(1, Ordering::SeqCst);
        let events = vec![
            LlmEvent::TextPart {
                text: "This is the summary.".to_string(),
            },
            LlmEvent::Done {
                stop_reason: "completed".to_string(),
            },
        ];
        Ok(Box::pin(stream::iter(events)))
    }

    fn materialize_media<'a>(
        &'a self,
        _params: &'a mut LlmChatParams,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = EngineResult<()>> + Send + 'a>> {
        use std::sync::atomic::Ordering;
        self.materialize_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn should_materialize_media_once_when_a_compaction_round_summarizes() {
    use std::sync::atomic::Ordering;

    // Given a counting LLM answering one summary and a memory to compact
    let llm = MaterializeCountingLlm::new();
    let mut memory = make_memory_with_many_messages();
    let cancel = CancelToken::new();
    let tools = ToolManager::new();

    // When a compaction round runs
    compaction_round(CompactionRoundInput {
        llm: &llm,
        memory: &mut memory,
        cancel: &cancel,
        tools: &tools,
        custom_instruction: None,
        max_output_size: None,
    })
    .await
    .expect("compaction should succeed");

    // Then the summarizer call materialized exactly once — the hook ran once
    // per chat call, before the request left the engine
    assert_eq!(llm.chat_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        llm.materialize_calls.load(Ordering::SeqCst),
        1,
        "each compaction LLM call must materialize exactly once"
    );
}

// ---------------------------------------------------------------------------
// Retention tail cutting (ADR-001)
// ---------------------------------------------------------------------------

fn user_msg(text: &str) -> ContextMessage {
    ContextMessage {
        message: Message::user(text.to_string()),
        // Real user input tag — head-essence selection filters on it.
        origin: Some(PromptOrigin::User),
        is_error: false,
        note: None,
        with_skills: None,
        entry_seq: 0,
    }
}

fn assistant_msg(text: &str) -> ContextMessage {
    ContextMessage {
        message: Message::assistant(text.to_string()),
        origin: None,
        is_error: false,
        note: None,
        with_skills: None,
        entry_seq: 0,
    }
}

/// assistant message carrying one tool call (id-tagged).
fn assistant_tool_call_msg(id: &str) -> ContextMessage {
    let mut m = Message::assistant(String::new());
    m.tool_calls.push(ToolCall {
        id: id.to_string(),
        call_type: "function".to_string(),
        function: ToolCallFunction {
            name: "read".to_string(),
            arguments: String::new(),
        },
    });
    ContextMessage {
        message: m,
        origin: None,
        is_error: false,
        note: None,
        with_skills: None,
        entry_seq: 0,
    }
}

/// tool result answering the given call id.
fn tool_result_msg(id: &str, text: &str) -> ContextMessage {
    ContextMessage {
        message: Message::tool_result(id, text),
        origin: None,
        is_error: false,
        note: None,
        with_skills: None,
        entry_seq: 0,
    }
}

#[test]
fn should_not_split_after_user_message() {
    let h = vec![user_msg("hi"), assistant_msg("a"), user_msg("again")];
    assert!(!can_split_after(&h, 0)); // user
    assert!(can_split_after(&h, 1)); // assistant without tool calls
}

#[test]
fn should_not_split_after_assistant_with_open_tool_calls() {
    let h = vec![
        user_msg("q"),
        assistant_tool_call_msg("t1"),
        tool_result_msg("t1", "r"),
    ];
    assert!(!can_split_after(&h, 1)); // assistant with pending tool call
}

#[test]
fn should_return_none_when_history_fits_within_budget() {
    let h = vec![user_msg("hi"), assistant_msg("hello")];
    // tiny messages, budget far above — nothing to compact.
    assert!(compute_retention_tail(&h, 10_000).is_none());
}

#[test]
fn should_cut_at_legal_boundary_when_budget_exceeded() {
    // 3 pairs of (user, assistant) with controlled sizes: users tiny,
    // assistants 4,000 ASCII chars ≈ 1,000 tokens each.
    let mut h = Vec::new();
    for i in 0..3 {
        h.push(user_msg(&format!("user turn {i}")));
        h.push(assistant_msg(&"a".repeat(4_000)));
    }
    // Budget 512: walking backwards, the last assistant (~1,000 tokens)
    // alone meets it → initial tail_start = 5; cutting right after the
    // user message at index 4 is illegal → retreat to 4 → cutting after
    // the assistant at index 3 is legal. Tail = [user turn 2, assistant].
    let cut: RetentionCut = compute_retention_tail(&h, 512).unwrap();
    assert_eq!(cut.tail_start, 4);
    assert!(cut.tail_tokens >= 512);
}

#[test]
fn should_give_up_when_retreat_reaches_oldest() {
    let mut h = vec![user_msg("q1")];
    h.push(assistant_tool_call_msg("t1"));
    h.push(tool_result_msg("t1", "result"));
    h.push(assistant_msg("done"));
    // Budget covering only the tool result (2 tokens: "result" ≈ 6 ASCII
    // chars / 4; the tool-call assistant is 0 tokens). Walking backwards,
    // "done" alone is 1 token < 2, so the tool result (2 tokens) first
    // meets it → initial cut lands before the tool result (illegal) →
    // retreat… until index 0 — the whole history would be the tail, there
    // is no compactable prefix → give up (US2.6). (Budget 1 would be met
    // by the newest "done" alone — 1 token — whose cut after the tool
    // result is legal; the estimator carries no per-message overhead.)
    assert!(compute_retention_tail(&h, 2).is_none());
}

#[test]
fn should_return_none_when_history_is_one_indivisible_unit() {
    // user → assistant(tool call) → tool result: no legal cut exists at
    // any position (after user = illegal; after tool-call assistant =
    // illegal; before tool result = illegal).
    let h = vec![
        user_msg("q"),
        assistant_tool_call_msg("t1"),
        tool_result_msg("t1", "big result"),
    ];
    assert!(compute_retention_tail(&h, 2).is_none());
}

// ---------------------------------------------------------------------------
// apply_compaction retention-tail placement tests (ADR-001/002)
// ---------------------------------------------------------------------------

fn memory_with(n_pairs: usize) -> ContextMemory {
    let mut mem = ContextMemory::new();
    for i in 0..n_pairs {
        mem.append_message(user_msg(&format!("user turn {i}")));
        mem.append_message(assistant_msg(&format!("assistant answer {i} with detail")));
    }
    mem
}

#[test]
fn should_keep_head_essence_summary_and_tail_verbatim_when_compacted() {
    let mut mem = memory_with(3);
    let before: Vec<String> = mem
        .history()
        .iter()
        .map(|m| m.message.text_content())
        .collect();

    let outcome: ApplyOutcome = mem.apply_compaction(CompactionInput {
        summary: "sum".into(),
        context_summary: "SUMMARY".into(),
        compacted_count: 4,
        tokens_before: mem.token_count_with_pending(),
        dropped_count: 0,
        retention_tail_start: Some(4), // keep the last pair verbatim
    });

    assert!(outcome.applied);
    assert_eq!(outcome.retained_tail_count, 2);
    assert_eq!(outcome.kept_head_user_message_count, 2); // turns 0 and 1
    assert!(outcome.elision_message.is_none()); // all prefix users kept
    let history = mem.history();
    // [head users (turn 0, 1)][summary (ADR-002)][tail (turn 2 pair)]
    assert_eq!(history.len(), 5);
    assert_eq!(history[0].message.text_content(), "user turn 0");
    assert_eq!(history[1].message.text_content(), "user turn 1");
    assert_eq!(history[2].message.text_content(), "SUMMARY");
    assert_eq!(history[2].origin, Some(PromptOrigin::CompactionSummary));
    assert_eq!(history[3].message.text_content(), before[4]);
    assert_eq!(history[4].message.text_content(), before[5]);
}

#[test]
fn should_place_head_user_essence_before_summary() {
    let mut mem = memory_with(4);
    let outcome = mem.apply_compaction(CompactionInput {
        summary: "s".into(),
        context_summary: "SUMMARY".into(),
        compacted_count: 6,
        tokens_before: mem.token_count_with_pending(),
        dropped_count: 0,
        retention_tail_start: Some(6),
    });
    assert!(outcome.applied);
    let history = mem.history();
    // head = prefix real users (turns 0..2, all fit the 2000-token budget)
    // + summary + tail (turn 3 pair) = 6 entries
    assert_eq!(history.len(), 6);
    assert_eq!(history[0].message.text_content(), "user turn 0");
    assert_eq!(history[1].message.text_content(), "user turn 1");
    assert_eq!(history[2].message.text_content(), "user turn 2");
    assert_eq!(history[3].message.text_content(), "SUMMARY");
    assert_eq!(history[4].message.text_content(), "user turn 3");
    assert_eq!(
        history[5].message.text_content(),
        "assistant answer 3 with detail"
    );
}

#[test]
fn should_insert_elision_marker_when_head_budget_cannot_fit_all_user_messages() {
    let mut mem = ContextMemory::new();
    // Two large real user messages in the prefix: the first (~1,000
    // tokens, 4,000 ASCII chars) fits the 2,000-token head budget, the
    // second (~3,000 tokens, 12,000 chars) does not → kept=1 < 2 → the
    // second is elided behind the marker.
    let small_big = "a".repeat(4_000);
    let large = "a".repeat(12_000);
    mem.append_message(ContextMessage {
        message: Message::user(small_big),
        origin: Some(PromptOrigin::User),
        is_error: false,
        note: None,
        with_skills: None,
        entry_seq: 0,
    });
    mem.append_message(assistant_msg(&"b".repeat(4_000)));
    mem.append_message(ContextMessage {
        message: Message::user(large),
        origin: Some(PromptOrigin::User),
        is_error: false,
        note: None,
        with_skills: None,
        entry_seq: 0,
    });
    mem.append_message(assistant_msg(&"b".repeat(4_000)));
    // Retention tail (small).
    mem.append_message(user_msg("final question"));
    mem.append_message(assistant_msg("final answer"));

    let outcome = mem.apply_compaction(CompactionInput {
        summary: "s".into(),
        context_summary: "SUMMARY".into(),
        compacted_count: 4,
        tokens_before: mem.token_count_with_pending(),
        dropped_count: 0,
        retention_tail_start: Some(4),
    });

    assert!(outcome.applied);
    assert!(outcome.elision_message.is_some());
    let history = mem.history();
    // [big user #1][elision][SUMMARY][tail (2)] = 5 entries
    assert_eq!(history.len(), 5);
    assert_eq!(history[0].message.text_content(), "a".repeat(4_000));
    assert_eq!(history[1].origin, Some(PromptOrigin::Injection));
    assert_eq!(history[2].origin, Some(PromptOrigin::CompactionSummary));
    assert_eq!(history[3].message.text_content(), "final question");
    assert_eq!(history[4].message.text_content(), "final answer");
}

#[test]
fn should_refuse_apply_without_retention_tail_start() {
    let mut mem = memory_with(2);
    let outcome = mem.apply_compaction(CompactionInput {
        summary: "s".into(),
        context_summary: "SUMMARY".into(),
        compacted_count: 4,
        tokens_before: mem.token_count_with_pending(),
        dropped_count: 0,
        retention_tail_start: None,
    });
    assert!(!outcome.applied);
    assert_eq!(mem.history().len(), 4); // unchanged
}

#[test]
fn should_reject_compaction_when_summary_is_exactly_the_same_size() {
    // Estimator arithmetic (S7): ASCII text counts ceil(chars / 4) tokens
    // with no per-message overhead, so every figure below is a literal
    // derived from that formula — not a call into production counting.
    //
    //   head user "u"             1 ASCII char  → ceil(1/4)  = 1 token
    //   prefix assistant          8 ASCII chars → ceil(8/4)  = 2 tokens
    //   tail user "t"             1 ASCII char  → ceil(1/4)  = 1 token
    //   tail assistant            4 ASCII chars → ceil(4/4)  = 1 token
    //   context summary           8 ASCII chars → ceil(8/4)  = 2 tokens
    //
    // tokens_before (guard reads token_count_with_pending = full history
    // estimate here, nothing pending) = 1 + 2 + 1 + 1 = 5.
    // tokens_after  = head user (1) + summary (2) + tail (1 + 1) = 5.
    //
    // The rewritten history is EXACTLY the same size as the original: the
    // must-shrink guard rejects anything not strictly smaller (>=), so an
    // equal-size rewrite must be refused and the original history kept.
    let mut mem = ContextMemory::new();
    mem.append_message(user_msg("u")); // 1 token, real user input → head essence
    mem.append_message(assistant_msg(&"a".repeat(8))); // 2 tokens, dropped by the rewrite
    mem.append_message(user_msg("t")); // tail: 1 token
    mem.append_message(assistant_msg(&"z".repeat(4))); // tail: 1 token

    let outcome = mem.apply_compaction(CompactionInput {
        summary: "s".into(),
        context_summary: "S".repeat(8), // 2 tokens — exactly the dropped assistant's size
        compacted_count: 2,
        tokens_before: 5,
        dropped_count: 0,
        retention_tail_start: Some(2),
    });

    assert!(
        !outcome.applied,
        "an exactly-equal rewrite must be rejected"
    );
    assert_eq!(outcome.retained_tail_count, 0);
    let history = mem.history();
    assert_eq!(
        history.len(),
        4,
        "the original history must be kept verbatim"
    );
    assert_eq!(history[1].message.text_content(), "a".repeat(8));
}

// ---------------------------------------------------------------------------
// Orchestration retention-tail tests
// ---------------------------------------------------------------------------

/// Begin params for a background compaction over the given LLM (mock).
fn sample_begin_params(llm: Arc<dyn Llm>) -> CompactionBeginParams {
    CompactionBeginParams {
        llm,
        cancel: CancelToken::new(),
        custom_instruction: None,
        max_output_size: None,
        hook_additional_context: Vec::new(),
    }
}

#[tokio::test]
async fn should_summarize_only_the_old_segment_and_keep_tail_verbatim() {
    // 3 turns; user messages tiny, assistant answers 4,000 ASCII chars
    // (~1,000 tokens each) so token math is dominated by known
    // quantities.
    let mut mem = ContextMemory::new();
    for i in 0..3 {
        mem.append_message(user_msg(&format!("user turn {i}")));
        mem.append_message(assistant_msg(&"a".repeat(4_000)));
    }
    // Window 3,200 → retention budget floor(3200 × 0.16) = 512: walking
    // backwards, the last assistant (~1,000 tokens) alone meets 512 →
    // initial tail_start = 5; cutting right after the user at index 4 is
    // illegal → retreat to 4 → cutting after the assistant at index 3 is
    // legal. Tail = [user turn 2, big assistant].
    let mock = Arc::new(MockLlm::new(vec![MockResponse::end_turn(
        "OLD SEGMENT SUMMARY",
    )]));
    let llm: Arc<dyn Llm> = mock.clone();

    let mut fc = FullCompaction::new(CompactionStrategy {
        max_context_tokens: 3_200,
        ..Default::default()
    });
    fc.begin(CompactionSource::Auto, sample_begin_params(llm), &mem)
        .unwrap();
    let result = fc.block(&mut mem, &CancelToken::new()).await.unwrap();

    assert!(result.applied);
    assert_eq!(result.retained_tail_count, 2);
    assert_eq!(result.kept_head_user_message_count, 2); // turns 0 and 1
    let history = mem.history();
    // [head users 0,1][summary][tail (turn 2 pair)] = 5
    assert_eq!(history.len(), 5);
    assert_eq!(history[3].message.text_content(), "user turn 2");
    assert_eq!(history[2].origin, Some(PromptOrigin::CompactionSummary));
    // The summarizer only saw the old segment (turns 0..1) — turn 2's
    // content never reaches the summarizer.
    let seen: Vec<String> = mock
        .messages_of_call(0)
        .iter()
        .map(|m| m.text_content())
        .collect();
    assert!(!seen.iter().any(|t| t.contains("user turn 2")));
}

#[test]
fn should_not_begin_when_no_legal_retention_cut_exists() {
    let mut mem = ContextMemory::new();
    mem.append_message(user_msg("q"));
    mem.append_message(assistant_tool_call_msg("t1"));
    mem.append_message(tool_result_msg("t1", "r"));

    let llm: Arc<dyn Llm> = Arc::new(MockLlm::new(vec![MockResponse::end_turn("never called")]));
    let mut fc = FullCompaction::new(CompactionStrategy {
        max_context_tokens: 10,
        ..Default::default()
    });
    let began = fc.begin(CompactionSource::Auto, sample_begin_params(llm), &mem);
    // US2.6: no safe cut → give up, write nothing.
    assert!(began.is_err());
    assert!(!fc.is_compacting());
}

#[tokio::test]
async fn should_backfill_live_compacted_count_when_user_input_lands_during_summarizer_call() {
    // Given a begun compaction over 3 turns (users tiny, assistants 4,000
    // ASCII chars ≈ 1,000 tokens) with window 3,200 → retention budget
    // floor(3200 × 0.16) = 512 — the begin-time cut lands at 4 (old
    // segment = turns 0..1, see the orchestration test above).
    let mut mem = ContextMemory::new();
    for i in 0..3 {
        mem.append_message(user_msg(&format!("user turn {i}")));
        mem.append_message(assistant_msg(&"a".repeat(4_000)));
    }
    let mock = Arc::new(MockLlm::new(vec![MockResponse::end_turn(
        "OLD SEGMENT SUMMARY",
    )]));
    let llm: Arc<dyn Llm> = mock.clone();

    let mut fc = FullCompaction::new(CompactionStrategy {
        max_context_tokens: 3_200,
        ..Default::default()
    });
    fc.begin(CompactionSource::Auto, sample_begin_params(llm), &mem)
        .unwrap();

    // And a real user message lands while the summarizer runs (between
    // begin and block): 2,400 ASCII chars ≈ 600 tokens ≥ the 512 budget,
    // so the live re-cut moves the tail start from 4 to 6 — the new
    // input rides verbatim in the retention tail and turns 2's pair is
    // pulled into the compacted prefix.
    mem.append_message(user_msg(&"l".repeat(2_400)));

    // When the round applies
    let result = fc.block(&mut mem, &CancelToken::new()).await.unwrap();

    // Then the reported compacted count reflects the LIVE re-cut (6),
    // not the begin-time snapshot (4) — the wire op must persist what
    // was actually compacted.
    assert!(result.applied);
    assert_eq!(result.compacted_count, 6);
    // The late user message is the whole retention tail, kept verbatim.
    assert_eq!(result.retained_tail_count, 1);
    let history = mem.history();
    assert_eq!(
        history.last().unwrap().message.text_content(),
        "l".repeat(2_400)
    );
    // The prefix's real users (turns 0..2) are the head essence.
    assert_eq!(result.kept_head_user_message_count, 3);
    assert_eq!(history[3].origin, Some(PromptOrigin::CompactionSummary));
}

// ---------------------------------------------------------------------------
// Prefix-race CANCEL branch (audit S3 / design §5.4)
// ---------------------------------------------------------------------------

/// Serialize a history for byte-level comparison — `ContextMessage` has
/// no `PartialEq`, and the JSON form covers the serialized fields
/// (length, role, content, origins, `tool_call_id`, notes included).
/// `entry_seq` is `#[serde(skip)]` and so sits OUTSIDE the JSON form —
/// it is not covered by this comparison.
fn json_history(mem: &ContextMemory) -> Vec<serde_json::Value> {
    mem.history()
        .iter()
        .map(|m| serde_json::to_value(m).unwrap())
        .collect()
}

/// Audit S3 / design §5.4 (并发变化检查): while the summarizer runs, only
/// append-only REAL USER INPUT may land — the round re-captures it into the
/// retention tail (the continue fixture above pins that direction). Any
/// other divergence — an assistant message appended mid-flight, the prefix
/// rewritten, an injection appended — must CANCEL the round:
/// `CompactionFailed`, memory left byte-identical to the pre-compaction
/// state, no apply record on the wire, orchestrator free again.
///
/// The §5.4 check is wired at both apply seams (SDR-011, implemented):
/// the begin-time snapshot rides with the attempt
/// (`FullCompaction::pending_snapshot`), and `block` /
/// `apply_compaction_result` run `prefix_race_check` against it before the
/// live re-cut. This fixture drives the `begin → block` construction.
#[tokio::test]
async fn should_cancel_the_round_when_history_diverges_during_the_summarizer_call() {
    // Given a begun compaction over 3 turns (the same base as the
    // continue fixture above: users tiny, assistants 4,000 ASCII chars ≈
    // 1,000 tokens; window 3,200 → begin-time cut at 4, old segment =
    // turns 0..1).
    let mut mem = ContextMemory::new();
    for i in 0..3 {
        mem.append_message(user_msg(&format!("user turn {i}")));
        mem.append_message(assistant_msg(&"a".repeat(4_000)));
    }
    let llm: Arc<dyn Llm> = Arc::new(MockLlm::new(vec![MockResponse::end_turn(
        "OLD SEGMENT SUMMARY",
    )]));

    let mut fc = FullCompaction::new(CompactionStrategy {
        max_context_tokens: 3_200,
        ..Default::default()
    });
    fc.begin(CompactionSource::Auto, sample_begin_params(llm), &mem)
        .unwrap();

    // And an ASSISTANT message lands while the summarizer is in flight
    // (between begin's snapshot and block's apply — the same window the
    // continue fixture appends its user input into). Unlike real user
    // input, this divergence must cancel: the summary was computed over
    // a history the live memory no longer matches.
    mem.append_message(assistant_msg("mid-flight assistant answer"));

    // The pre-compaction state the cancelled round must leave untouched:
    // the begin-time history plus the divergent append itself (the
    // divergence is session content — only the compaction is cancelled).
    let before = json_history(&mem);

    // When the round settles
    let result = fc.block(&mut mem, &CancelToken::new()).await;

    // Then the round returns CompactionFailed (the §5.4 cancel)...
    match &result {
        Err(EngineError::CompactionFailed(msg)) => {
            assert!(
                msg.contains("prefix-race"),
                "the cancel must identify the prefix race, got: {msg}"
            );
        }
        other => panic!("expected the prefix-race cancel, got {other:?}"),
    }

    // ...memory stays byte-identical to the pre-compaction state — every
    // serialized field (length, role, content, origins, `tool_call_id`)
    // must match; the summary, head essence and elision marker must not
    // materialize. (`entry_seq` is `#[serde(skip)]` and is therefore
    // outside this comparison.)
    assert_eq!(
        json_history(&mem),
        before,
        "a cancelled round must not rewrite memory"
    );

    // ...and no apply record lands on the wire. That guarantee is a gate
    // at the real write points — hooks.rs:418-450 /
    // turn_flow/mod.rs:1393 record only when the round applied, and an
    // `Err` from this seam never reaches them. Asserting it here would
    // mean building the op list ourselves from a result the match above
    // already proved is `Err` — a vacuous assertion — so a true
    // drive-through of the write point needs the turn_flow harness
    // (left as an embedded-level follow-up).

    // The orchestrator is free to compact again.
    assert!(!fc.is_compacting());
}

// ---------------------------------------------------------------------------
// Resume round-trip fidelity (ADR-005 / US5.4)
// ---------------------------------------------------------------------------

/// Assert two memories agree message for message (US5.4): every history
/// entry's role, text, origin, tool_call_id, tool_calls count, and the
/// tool-result flag fields (`is_error`, `note`) match.
fn assert_histories_match(live: &ContextMemory, replayed: &ContextMemory) {
    let l = live.history();
    let r = replayed.history();
    assert_eq!(l.len(), r.len(), "history lengths diverge");
    for (i, (a, b)) in l.iter().zip(r.iter()).enumerate() {
        assert_eq!(a.message.role, b.message.role, "role @ {i}");
        assert_eq!(
            a.message.text_content(),
            b.message.text_content(),
            "text @ {i}"
        );
        assert_eq!(a.origin, b.origin, "origin @ {i}");
        assert_eq!(
            a.message.tool_call_id, b.message.tool_call_id,
            "tool_call_id @ {i}"
        );
        assert_eq!(
            a.message.tool_calls.len(),
            b.message.tool_calls.len(),
            "tool_calls @ {i}"
        );
        // S8: a fold rebuild that flips the error flag (or note) on a
        // retained tool result must not survive this gate — the flags ride
        // on the ContextMessage, outside the message JSON compared above.
        assert_eq!(a.is_error, b.is_error, "is_error @ {i}");
        assert_eq!(a.note, b.note, "note @ {i}");
    }
}

/// Record a message op exactly where the engine would write it: the wire
/// position is the *arrival* position, which for a message deferred behind
/// an open tool exchange is before the resolving `tool.result` — the slot
/// it finally lands in is later (the core divergence this gate guards).
fn record_message(mem: &mut ContextMemory, ops: &mut Vec<WireOp>, msg: ContextMessage) {
    // Serialize before appending: a deferred message is not in `history()`
    // at append time, so `history().last()` would read the wrong entry.
    ops.push(WireOp::ContextAppendMessage {
        message: serde_json::to_value(&msg).unwrap(),
    });
    mem.append_message(msg);
}

fn record_loop_event(mem: &mut ContextMemory, ops: &mut Vec<WireOp>, event: LoopEvent) {
    mem.append_loop_event(event.clone());
    ops.push(WireOp::ContextAppendLoopEvent {
        event: event.to_wire_value(),
    });
}

/// Push the three-op compaction record, mirroring hooks.rs:418-450
/// (RequestHeaderChanged → ContextApplyCompaction with the ADR-005
/// counts → ContextSizeMeasured).
fn record_apply_ops(
    ops: &mut Vec<WireOp>,
    result: &mirri_core::compaction::CompactionResult,
    history_len: usize,
) {
    ops.push(WireOp::RequestHeaderChanged {
        reason: "compaction_applied".to_string(),
    });
    let (first_replaced_entry_seq, last_replaced_entry_seq, dropped_entry_seqs) =
        mirri_core::compaction::apply_compaction_op_fields(result);
    ops.push(WireOp::ContextApplyCompaction {
        summary: result.summary.clone(),
        compacted_count: result.compacted_count as u32,
        tokens_before: Some(result.tokens_before),
        tokens_after: Some(result.tokens_after),
        context_summary: Some(result.context_summary.clone()),
        dropped_count: Some(result.dropped_count as u32),
        retained_tail_count: Some(result.retained_tail_count as u32),
        // TS-dialect reader field — the rust kernel never writes it.
        kept_user_message_count: None,
        kept_head_user_message_count: Some(result.kept_head_user_message_count as u32),
        elision_message: result.elision_message.clone(),
        // ADR-007 provenance, mapped exactly as the three write points do.
        first_replaced_entry_seq,
        last_replaced_entry_seq,
        dropped_entry_seqs,
    });
    ops.push(WireOp::ContextSizeMeasured {
        length: history_len as u64,
        tokens: result.tokens_after,
    });
}

/// Fold the recorded ops into an `AgentState` and rebuild the memory.
fn replay_memory(ops: &[WireOp]) -> ContextMemory {
    let mut state = AgentState::new();
    for op in ops {
        let (next, _events) = reduce(&state, op);
        state = next;
    }
    reconstruct_context_memory(&state)
}

#[tokio::test]
async fn should_replay_to_the_same_memory_when_compaction_applied_and_resumed() {
    // Given a live memory of 3 turns, each with a tool exchange (mirrors
    // what a real turn records on the wire: user message, step.begin,
    // tool.call, tool.result, content.part, step.end)
    let mut mem = ContextMemory::new();
    let mut ops: Vec<WireOp> = Vec::new();
    for turn in 0..3 {
        let question = format!("question {turn}");
        mem.append_message(user_msg(&question));
        ops.push(WireOp::ContextAppendMessage {
            message: serde_json::to_value(mem.history().last().unwrap()).unwrap(),
        });

        let step = format!("s{turn}");
        let call = format!("call-{turn}");
        // S8: the newest turn's tool exchange failed — a wire-faithful
        // shape (real tool results can be errors) that lands in the
        // retention tail, so the round-trip gate below proves the fold
        // rebuild preserves the flag and note on retained tool results.
        let failed = turn == 2;
        for event in [
            LoopEvent::StepBegin {
                step_id: step.clone(),
                entry_seq: None,
            },
            LoopEvent::ToolCall {
                step_id: step.clone(),
                tool_call_id: call.clone(),
                tool_name: "read".to_string(),
                args: serde_json::json!({"path": "/tmp/f"}),
            },
            LoopEvent::ToolResult {
                tool_call_id: call.clone(),
                output: ToolOutput::Text(format!("result {turn}")),
                is_error: failed,
                note: failed.then(|| "tool failed".to_string()),
                entry_seq: None,
            },
            LoopEvent::ContentPart {
                step_id: step.clone(),
                text: format!("answer {turn} ").repeat(300),
            },
            LoopEvent::StepEnd {
                step_id: step.clone(),
                // Usage big enough to cover the assistant bodies, so the
                // shrink check (变小校验) sees a real reduction.
                input_tokens: 2_000,
                output_tokens: 100,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
        ] {
            mem.append_loop_event(event.clone());
            ops.push(WireOp::ContextAppendLoopEvent {
                event: event.to_wire_value(),
            });
        }
    }

    // And a compaction applied through the orchestration (window small
    // enough that the retention budget covers roughly the last turn)
    let llm: Arc<dyn Llm> = Arc::new(MockLlm::new(vec![MockResponse::end_turn(
        "OLD SEGMENT SUMMARY",
    )]));
    let mut fc = FullCompaction::new(CompactionStrategy {
        max_context_tokens: 3_200,
        ..Default::default()
    });
    fc.begin(CompactionSource::Auto, sample_begin_params(llm), &mem)
        .unwrap();
    let result = fc.block(&mut mem, &CancelToken::new()).await.unwrap();
    assert!(result.applied, "compaction must apply for the round-trip");

    // Then the live memory is [head users][summary][retention tail]: the
    // tail holds turn 2's three slots, the head essence the first two
    // questions. This pins the live cut so a divergence below is
    // attributable to the replay, not to the fixture.
    let live = mem.history();
    assert_eq!(live.len(), 6);
    assert_eq!(live[0].message.text_content(), "question 0");
    assert_eq!(live[1].message.text_content(), "question 1");
    assert_eq!(live[2].origin, Some(PromptOrigin::CompactionSummary));
    assert_eq!(live[3].message.text_content(), "question 2");
    assert_eq!(live[5].message.tool_call_id.as_deref(), Some("call-2"));
    // Pin the live error flag first: the retained tail's tool result
    // (live[5]) must carry it, so a divergence below is attributable to
    // the replay, not the fixture (S8).
    assert!(
        live[5].is_error,
        "the retained tool result must be an error"
    );
    assert_eq!(live[5].note.as_deref(), Some("tool failed"));
    assert_eq!(result.retained_tail_count, 3);
    assert_eq!(result.kept_head_user_message_count, 2);

    // And the apply recorded exactly as hooks.rs:418-450 does (the
    // three-op sequence with the ADR-005 counts)
    record_apply_ops(&mut ops, &result, mem.history().len());

    // When the ops are folded and the memory reconstructed
    let replayed = replay_memory(&ops);

    // Then the replayed memory is the live post-compaction memory,
    // message for message (US5.4).
    assert_histories_match(&mem, &replayed);
}

#[tokio::test]
async fn should_replay_to_the_same_memory_when_a_vacuous_step_and_a_deferred_message_precede_the_tail()
 {
    // Given a live memory whose middle turn records two adversarial
    // shapes on the wire: (i) a vacuous step — StepBegin immediately
    // followed by StepEnd, no content part, no tool call: live removes
    // the empty partial assistant but the wire keeps both ops, so a
    // naive slot count is off by one; and (ii) a real user message
    // appended while a tool exchange is open: live defers it behind the
    // tool result while the wire op keeps its arrival position.
    let mut mem = ContextMemory::new();
    let mut ops: Vec<WireOp> = Vec::new();

    // Turn 0 — a plain tool exchange.
    record_message(&mut mem, &mut ops, user_msg("question 0"));
    for event in [
        LoopEvent::StepBegin {
            step_id: "s0".to_string(),
            entry_seq: None,
        },
        LoopEvent::ToolCall {
            step_id: "s0".to_string(),
            tool_call_id: "call-0".to_string(),
            tool_name: "read".to_string(),
            args: serde_json::json!({"path": "/tmp/f"}),
        },
        LoopEvent::ToolResult {
            tool_call_id: "call-0".to_string(),
            output: ToolOutput::Text("result 0".to_string()),
            is_error: false,
            note: None,
            entry_seq: None,
        },
        LoopEvent::ContentPart {
            step_id: "s0".to_string(),
            text: "answer 0 ".to_string().repeat(300),
        },
        LoopEvent::StepEnd {
            step_id: "s0".to_string(),
            input_tokens: 2_000,
            output_tokens: 100,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        },
    ] {
        record_loop_event(&mut mem, &mut ops, event);
    }

    // Turn 1 — the adversarial turn.
    record_message(&mut mem, &mut ops, user_msg("question 1"));
    record_loop_event(
        &mut mem,
        &mut ops,
        LoopEvent::StepBegin {
            step_id: "s1".to_string(),
            entry_seq: None,
        },
    );
    record_loop_event(
        &mut mem,
        &mut ops,
        LoopEvent::ToolCall {
            step_id: "s1".to_string(),
            tool_call_id: "call-1".to_string(),
            tool_name: "read".to_string(),
            args: serde_json::json!({"path": "/tmp/f"}),
        },
    );
    // A real user message arriving while the exchange is open: live
    // parks it in the deferred buffer, the wire op keeps the arrival
    // position (before the tool result).
    record_message(&mut mem, &mut ops, user_msg("interrupting input"));
    record_loop_event(
        &mut mem,
        &mut ops,
        LoopEvent::ToolResult {
            tool_call_id: "call-1".to_string(),
            output: ToolOutput::Text("result 1".to_string()),
            is_error: false,
            note: None,
            entry_seq: None,
        },
    );
    record_loop_event(
        &mut mem,
        &mut ops,
        LoopEvent::ContentPart {
            step_id: "s1".to_string(),
            text: "answer 1 ".to_string().repeat(300),
        },
    );
    record_loop_event(
        &mut mem,
        &mut ops,
        LoopEvent::StepEnd {
            step_id: "s1".to_string(),
            input_tokens: 2_000,
            output_tokens: 100,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        },
    );
    // The vacuous step: no content part, no tool call.
    record_loop_event(
        &mut mem,
        &mut ops,
        LoopEvent::StepBegin {
            step_id: "s2".to_string(),
            entry_seq: None,
        },
    );
    record_loop_event(
        &mut mem,
        &mut ops,
        LoopEvent::StepEnd {
            step_id: "s2".to_string(),
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        },
    );

    // Turn 2 — a plain tool exchange.
    record_message(&mut mem, &mut ops, user_msg("question 2"));
    for event in [
        LoopEvent::StepBegin {
            step_id: "s3".to_string(),
            entry_seq: None,
        },
        LoopEvent::ToolCall {
            step_id: "s3".to_string(),
            tool_call_id: "call-2".to_string(),
            tool_name: "read".to_string(),
            args: serde_json::json!({"path": "/tmp/f"}),
        },
        LoopEvent::ToolResult {
            tool_call_id: "call-2".to_string(),
            output: ToolOutput::Text("result 2".to_string()),
            is_error: false,
            note: None,
            entry_seq: None,
        },
        LoopEvent::ContentPart {
            step_id: "s3".to_string(),
            text: "answer 2 ".to_string().repeat(300),
        },
        LoopEvent::StepEnd {
            step_id: "s3".to_string(),
            input_tokens: 2_000,
            output_tokens: 100,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        },
    ] {
        record_loop_event(&mut mem, &mut ops, event);
    }

    // Sanity: the live pre-compaction memory is exactly 10 slots — the
    // vacuous step left nothing behind and the deferred message landed
    // after the tool result that closed the exchange.
    assert_eq!(mem.history().len(), 10);
    assert_eq!(
        mem.history()[6].message.text_content(),
        "interrupting input"
    );
    assert_eq!(mem.history()[6].origin, Some(PromptOrigin::User));

    // When a compaction applies (window 3,200 → budget 512: walking back,
    // turn 2's assistant alone meets it, and the cut retreats past the
    // interrupting message, so the tail is [interrupting input, question 2,
    // answer 2, result 2] — the deferred message and the vacuous step
    // straddle the retention boundary).
    let llm: Arc<dyn Llm> = Arc::new(MockLlm::new(vec![MockResponse::end_turn(
        "OLD SEGMENT SUMMARY",
    )]));
    let mut fc = FullCompaction::new(CompactionStrategy {
        max_context_tokens: 3_200,
        ..Default::default()
    });
    fc.begin(CompactionSource::Auto, sample_begin_params(llm), &mem)
        .unwrap();
    let result = fc.block(&mut mem, &CancelToken::new()).await.unwrap();
    assert!(result.applied, "compaction must apply for the round-trip");

    // Then the live memory is [head users][summary][tail]: the vacuous
    // step is nowhere in it, and the deferred message rides verbatim as
    // the first tail slot.
    let live = mem.history();
    assert_eq!(live.len(), 7);
    assert_eq!(live[0].message.text_content(), "question 0");
    assert_eq!(live[1].message.text_content(), "question 1");
    assert_eq!(live[2].origin, Some(PromptOrigin::CompactionSummary));
    assert_eq!(live[3].message.text_content(), "interrupting input");
    assert_eq!(live[3].origin, Some(PromptOrigin::User));
    assert_eq!(live[4].message.text_content(), "question 2");
    assert_eq!(live[5].message.tool_calls.len(), 1);
    assert_eq!(live[6].message.tool_call_id.as_deref(), Some("call-2"));
    assert_eq!(result.retained_tail_count, 4);
    assert_eq!(result.kept_head_user_message_count, 2);

    // And the apply recorded exactly as hooks.rs:418-450 does
    record_apply_ops(&mut ops, &result, mem.history().len());

    // When the ops are folded and the memory reconstructed
    let replayed = replay_memory(&ops);

    // Then the replayed memory is the live post-compaction memory,
    // message for message (US5.4) — in particular the deferred message
    // is replayed at its materialized position (after the tool result),
    // not at its wire-arrival position, and the vacuous step's two ops
    // contribute no phantom slot.
    assert_histories_match(&mem, &replayed);
}

// ---------------------------------------------------------------------------
// Capacity-rejection round-trip fidelity (fold-time slot simulation)
// ---------------------------------------------------------------------------

/// Record the three-op compaction sequence for an apply driven directly
/// through `ContextMemory::apply_compaction` — the same wire shape
/// `record_apply_ops` writes for an orchestrated round.
fn record_direct_apply(
    ops: &mut Vec<WireOp>,
    input: &CompactionInput,
    outcome: &ApplyOutcome,
    history_len_after: usize,
    tokens_after: u64,
    first_replaced: Option<u64>,
    last_replaced: Option<u64>,
) {
    ops.push(WireOp::RequestHeaderChanged {
        reason: "compaction_applied".to_string(),
    });
    ops.push(WireOp::ContextApplyCompaction {
        summary: input.summary.clone(),
        compacted_count: input.compacted_count,
        tokens_before: Some(input.tokens_before),
        tokens_after: Some(tokens_after),
        context_summary: Some(input.context_summary.clone()),
        dropped_count: Some(input.dropped_count),
        retained_tail_count: Some(outcome.retained_tail_count as u32),
        // TS-dialect reader field — the rust kernel never writes it.
        kept_user_message_count: None,
        kept_head_user_message_count: Some(outcome.kept_head_user_message_count as u32),
        elision_message: outcome.elision_message.clone(),
        // ADR-007 provenance: the replaced prefix's entry range, derived
        // from the pre-apply history exactly as the apply paths do (these
        // fixtures apply directly to memory, which does not derive it).
        first_replaced_entry_seq: first_replaced,
        last_replaced_entry_seq: last_replaced,
        dropped_entry_seqs: None,
    });
    ops.push(WireOp::ContextSizeMeasured {
        length: history_len_after as u64,
        tokens: tokens_after,
    });
}

/// The pre-apply replaced prefix's entry range (ADR-007): min/max `entry_seq`
/// of `history[..tail_start]` — the same derivation the apply paths thread
/// onto the wire op.
fn direct_apply_replaced_range(
    mem: &ContextMemory,
    tail_start: usize,
) -> (Option<u64>, Option<u64>) {
    (
        mem.history()[..tail_start]
            .iter()
            .map(|m| m.entry_seq)
            .min(),
        mem.history()[..tail_start]
            .iter()
            .map(|m| m.entry_seq)
            .max(),
    )
}

/// Fold a pre-compaction append-only prefix into an `AgentState` by the
/// direct-push equivalent of the reducer's append arms, then fold the
/// compaction record through the real `reduce_with_limits`.
///
/// The append fold is a pure push (message/loop-event + timeline entry), so
/// replicating it directly is behavior-identical while avoiding `reduce`'s
/// per-op full-state clone — O(n²) on the 10 001-op history-cap fixture,
/// where each clone would copy the whole growing conversation. The apply op
/// — where the fold-time slot simulation and its capacity modeling live —
/// still folds through the real reducer, under the given resume-time
/// capacity limits (`None` keeps the default-constants behavior).
fn replay_memory_after_capacity_overflow(
    ops: &[WireOp],
    apply: &WireOp,
    replay_limits: Option<ReplayLimits>,
) -> ContextMemory {
    let mut state = AgentState::new();
    for op in ops {
        match op {
            WireOp::ContextAppendMessage { message } => {
                state.conversation.messages.push(message.clone());
                state.conversation.timeline.push(TimelineEntry::Message {
                    index: state.conversation.messages.len() - 1,
                });
            }
            WireOp::ContextAppendLoopEvent { event } => {
                state.conversation.loop_events.push(event.clone());
                state.conversation.timeline.push(TimelineEntry::LoopEvent {
                    index: state.conversation.loop_events.len() - 1,
                });
            }
            _ => panic!("unexpected op before the compaction record"),
        }
    }
    let (state, _events) = reduce_with_limits(&state, apply, replay_limits);
    reconstruct_context_memory(&state)
}

#[test]
fn should_replay_to_the_same_memory_when_history_cap_rejected_overflow_appends() {
    // Given a session that pushed one message past the history cap: the
    // first `DEFAULT_MAX_HISTORY_MESSAGES` messages stored, the append
    // beyond the cap hard-rejected by `push_history` — never stored —
    // while the wire carries all 10 001 ops
    let mut mem = ContextMemory::new();
    let mut ops: Vec<WireOp> = Vec::new();
    for i in 0..=DEFAULT_MAX_HISTORY_MESSAGES {
        record_message(&mut mem, &mut ops, user_msg(&format!("question {i}")));
    }
    assert_eq!(mem.history().len(), DEFAULT_MAX_HISTORY_MESSAGES);
    assert!(
        !mem.history()
            .iter()
            .any(|m| m.message.text_content() == "question 10000")
    );

    // And a compaction applied directly, keeping the last two live slots
    // verbatim as the retention tail
    let tail_start = mem.history().len() - 2;
    let tokens_before = mem.token_count_with_pending();
    let input = CompactionInput {
        summary: "s".into(),
        context_summary: "SUMMARY".into(),
        compacted_count: tail_start as u32,
        tokens_before,
        dropped_count: 0,
        retention_tail_start: Some(tail_start),
    };
    // The replaced prefix's entry range must be captured before the apply
    // rewrites the history (ADR-007).
    let (first_replaced, last_replaced) = direct_apply_replaced_range(&mem, tail_start);
    let outcome = mem.apply_compaction(input.clone());
    assert!(outcome.applied);
    assert_eq!(outcome.retained_tail_count, 2);

    // When the ops are folded (append prefix via its direct-push
    // equivalent — see the helper — and the apply op through the real
    // reducer) and the memory reconstructed
    let apply = WireOp::ContextApplyCompaction {
        summary: input.summary.clone(),
        compacted_count: input.compacted_count,
        tokens_before: Some(input.tokens_before),
        tokens_after: Some(mem.token_count()),
        context_summary: Some(input.context_summary.clone()),
        dropped_count: Some(input.dropped_count),
        retained_tail_count: Some(outcome.retained_tail_count as u32),
        // TS-dialect reader field — the rust kernel never writes it.
        kept_user_message_count: None,
        kept_head_user_message_count: Some(outcome.kept_head_user_message_count as u32),
        elision_message: outcome.elision_message.clone(),
        first_replaced_entry_seq: first_replaced,
        last_replaced_entry_seq: last_replaced,
        dropped_entry_seqs: None,
    };
    let replayed = replay_memory_after_capacity_overflow(&ops, &apply, None);

    // Then the replayed memory is the live post-compaction memory, message
    // for message: the simulated slot count equals the live count (10 000,
    // not 10 001), so the tail boundary lands on the same live slots and
    // the message the live side rejected at the cap never leaks into the
    // rebuilt tail.
    assert_histories_match(&mem, &replayed);
    let live_tail: Vec<String> = mem.history()[mem.history().len() - 2..]
        .iter()
        .map(|m| m.message.text_content())
        .collect();
    assert_eq!(live_tail, vec!["question 9998", "question 9999"]);
    assert!(
        !replayed
            .history()
            .iter()
            .any(|m| m.message.text_content() == "question 10000")
    );
}

#[test]
fn should_replay_to_the_same_memory_when_resume_time_limits_are_tighter_than_defaults() {
    // Given a session whose turn flow ran with tightened limits
    // (set_limits smaller than the 10 000/1_000 defaults — the caps
    // turn_flow/mod.rs applies from the strategy every turn): the first
    // four bulky messages stored, the append beyond the tightened cap
    // hard-rejected by `push_history` — never stored — while the wire
    // carries all five ops
    let max_history_messages = 4;
    let mut mem = ContextMemory::new();
    mem.set_limits(max_history_messages, DEFAULT_MAX_DEFERRED_MESSAGES);
    let mut ops: Vec<WireOp> = Vec::new();
    for i in 0..=max_history_messages {
        record_message(
            &mut mem,
            &mut ops,
            user_msg(&format!("question {i} {}", "x".repeat(6_000))),
        );
    }
    assert_eq!(mem.history().len(), max_history_messages);
    assert!(
        !mem.history()
            .iter()
            .any(|m| m.message.text_content().starts_with("question 4"))
    );

    // And a compaction applied directly, keeping the last live slot
    // verbatim as the retention tail (the ~6 000-token prefix against a
    // 2 000-token head budget elides all but the first user message, so
    // the rewritten history genuinely shrinks)
    let tail_start = mem.history().len() - 1;
    let tokens_before = mem.token_count_with_pending();
    let input = CompactionInput {
        summary: "s".into(),
        context_summary: "SUMMARY".into(),
        compacted_count: tail_start as u32,
        tokens_before,
        dropped_count: 0,
        retention_tail_start: Some(tail_start),
    };
    // The replaced prefix's entry range must be captured before the apply
    // rewrites the history (ADR-007).
    let (first_replaced, last_replaced) = direct_apply_replaced_range(&mem, tail_start);
    let outcome = mem.apply_compaction(input.clone());
    assert!(outcome.applied);
    assert_eq!(outcome.retained_tail_count, 1);
    assert_eq!(outcome.kept_head_user_message_count, 1);
    assert!(outcome.elision_message.is_some());

    // Sanity: the live post-compaction memory is [head essence][elision
    // marker][summary][retained tail].
    let live = mem.history();
    assert_eq!(live.len(), 4);
    assert!(live[0].message.text_content().starts_with("question 0"));
    assert!(live[3].message.text_content().starts_with("question 3"));

    // When the ops are folded with the resume-time limits threaded through
    // the same path the production resume now uses (the fold simulation
    // must honor the tightened caps, not the default constants) and the
    // memory reconstructed
    let apply = WireOp::ContextApplyCompaction {
        summary: input.summary.clone(),
        compacted_count: input.compacted_count,
        tokens_before: Some(input.tokens_before),
        tokens_after: Some(mem.token_count()),
        context_summary: Some(input.context_summary.clone()),
        dropped_count: Some(input.dropped_count),
        retained_tail_count: Some(outcome.retained_tail_count as u32),
        // TS-dialect reader field — the rust kernel never writes it.
        kept_user_message_count: None,
        kept_head_user_message_count: Some(outcome.kept_head_user_message_count as u32),
        elision_message: outcome.elision_message.clone(),
        first_replaced_entry_seq: first_replaced,
        last_replaced_entry_seq: last_replaced,
        dropped_entry_seqs: None,
    };
    let replayed = replay_memory_after_capacity_overflow(
        &ops,
        &apply,
        Some(ReplayLimits {
            max_history_messages,
            max_deferred_messages: DEFAULT_MAX_DEFERRED_MESSAGES,
        }),
    );

    // Then the replayed memory is the live post-compaction memory, message
    // for message: the simulated slot count equals the live count (4, not
    // 5), so the tail boundary lands on the same live slot and the
    // message the live side rejected at the tightened cap never leaks into
    // the rebuilt tail.
    assert_histories_match(&mem, &replayed);
    assert!(
        replayed.history()[replayed.history().len() - 1]
            .message
            .text_content()
            .starts_with("question 3")
    );
    assert!(
        !replayed
            .history()
            .iter()
            .any(|m| m.message.text_content().starts_with("question 4"))
    );
}

#[test]
fn should_replay_to_the_same_memory_when_deferred_cap_rejected_overflow_appends() {
    // Given a tool exchange held open across 1 002 arriving messages: the
    // first `DEFAULT_MAX_DEFERRED_MESSAGES` were buffered in the deferred
    // queue, the two beyond the cap hard-rejected — never stored, never
    // flushed
    let mut mem = ContextMemory::new();
    let mut ops: Vec<WireOp> = Vec::new();
    record_message(&mut mem, &mut ops, user_msg("question 0"));
    record_loop_event(
        &mut mem,
        &mut ops,
        LoopEvent::StepBegin {
            step_id: "s0".to_string(),
            entry_seq: None,
        },
    );
    record_loop_event(
        &mut mem,
        &mut ops,
        LoopEvent::ToolCall {
            step_id: "s0".to_string(),
            tool_call_id: "call-0".to_string(),
            tool_name: "read".to_string(),
            args: serde_json::json!({"path": "/tmp/f"}),
        },
    );
    for i in 0..=(DEFAULT_MAX_DEFERRED_MESSAGES + 1) {
        record_message(&mut mem, &mut ops, user_msg(&format!("deferred {i}")));
    }
    record_loop_event(
        &mut mem,
        &mut ops,
        LoopEvent::ToolResult {
            tool_call_id: "call-0".to_string(),
            output: ToolOutput::Text("result 0".to_string()),
            is_error: false,
            note: None,
            entry_seq: None,
        },
    );
    record_loop_event(
        &mut mem,
        &mut ops,
        LoopEvent::ContentPart {
            step_id: "s0".to_string(),
            text: "answer 0".to_string(),
        },
    );
    record_loop_event(
        &mut mem,
        &mut ops,
        LoopEvent::StepEnd {
            step_id: "s0".to_string(),
            // Non-zero usage keeps the shrink check meaningful (the
            // covered span then excludes the flushed deferred messages).
            input_tokens: 5_000,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        },
    );

    // Sanity: the live memory held exactly the capped shape — the two
    // overflow messages are nowhere in history.
    assert_eq!(mem.history().len(), 3 + DEFAULT_MAX_DEFERRED_MESSAGES);
    assert!(
        !mem.history()
            .iter()
            .any(|m| m.message.text_content() == "deferred 1000")
    );
    assert!(
        !mem.history()
            .iter()
            .any(|m| m.message.text_content() == "deferred 1001")
    );

    // And a compaction applied directly, keeping the last three live slots
    // verbatim as the retention tail
    let tail_start = mem.history().len() - 3;
    let tokens_before = mem.token_count_with_pending();
    let input = CompactionInput {
        summary: "s".into(),
        context_summary: "SUMMARY".into(),
        compacted_count: tail_start as u32,
        tokens_before,
        dropped_count: 0,
        retention_tail_start: Some(tail_start),
    };
    // The replaced prefix's entry range must be captured before the apply
    // rewrites the history (ADR-007).
    let (first_replaced, last_replaced) = direct_apply_replaced_range(&mem, tail_start);
    let outcome = mem.apply_compaction(input.clone());
    assert!(outcome.applied);
    assert_eq!(outcome.retained_tail_count, 3);

    // When the ops are folded and the memory reconstructed (the full
    // per-op fold — this fixture is small enough for it)
    record_direct_apply(
        &mut ops,
        &input,
        &outcome,
        mem.history().len(),
        mem.token_count(),
        first_replaced,
        last_replaced,
    );
    let replayed = replay_memory(&ops);

    // Then the replayed memory is the live post-compaction memory, message
    // for message — the deferred-cap rejections leave no phantom slot, so
    // the tail boundary lands on the same live slots and neither overflow
    // message leaks into the rebuilt tail.
    assert_histories_match(&mem, &replayed);
    let live_tail: Vec<String> = mem.history()[mem.history().len() - 3..]
        .iter()
        .map(|m| m.message.text_content())
        .collect();
    assert_eq!(
        live_tail,
        vec!["deferred 997", "deferred 998", "deferred 999"]
    );
    assert!(
        !replayed
            .history()
            .iter()
            .any(|m| m.message.text_content() == "deferred 1000")
    );
}

#[test]
fn should_replay_elision_marker_verbatim_when_prefix_users_exceed_head_budget() {
    // Given a prefix whose real user input exceeds the 2 000-token head
    // budget (the first big user message fits, the second does not →
    // elided behind the `<system-reminder>` marker), plus a small
    // retention tail
    let mut mem = ContextMemory::new();
    let mut ops: Vec<WireOp> = Vec::new();
    record_message(
        &mut mem,
        &mut ops,
        ContextMessage {
            message: Message::user("a".repeat(4_000)),
            origin: Some(PromptOrigin::User),
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        },
    );
    record_message(&mut mem, &mut ops, assistant_msg(&"b".repeat(4_000)));
    record_message(
        &mut mem,
        &mut ops,
        ContextMessage {
            message: Message::user("c".repeat(12_000)),
            origin: Some(PromptOrigin::User),
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        },
    );
    record_message(&mut mem, &mut ops, assistant_msg(&"b".repeat(4_000)));
    record_message(&mut mem, &mut ops, user_msg("final question"));
    record_message(&mut mem, &mut ops, assistant_msg("final answer"));

    let tokens_before = mem.token_count_with_pending();
    let input = CompactionInput {
        summary: "s".into(),
        context_summary: "SUMMARY".into(),
        compacted_count: 4,
        tokens_before,
        dropped_count: 0,
        retention_tail_start: Some(4),
    };
    let (first_replaced, last_replaced) = direct_apply_replaced_range(&mem, 4);
    let outcome = mem.apply_compaction(input.clone());
    assert!(outcome.applied);
    assert!(outcome.elision_message.is_some());

    // When the ops are folded and the memory reconstructed
    record_direct_apply(
        &mut ops,
        &input,
        &outcome,
        mem.history().len(),
        mem.token_count(),
        first_replaced,
        last_replaced,
    );
    let replayed = replay_memory(&ops);

    // Then the replayed memory matches live message for message — and the
    // elision marker is byte-identical on both sides: the text live
    // inserted between head essence and summary is exactly what the wire
    // op carried and exactly what the restore materialized.
    assert_histories_match(&mem, &replayed);
    let live = mem.history();
    assert_eq!(live[1].origin, Some(PromptOrigin::Injection));
    assert!(
        live[1]
            .message
            .text_content()
            .starts_with("<system-reminder>")
    );
    assert_eq!(replayed.history()[1].origin, Some(PromptOrigin::Injection));
    assert_eq!(
        replayed.history()[1].message.text_content(),
        live[1].message.text_content()
    );
    let marker = outcome.elision_message.as_ref().unwrap();
    assert_eq!(
        marker["content"][0]["text"].as_str(),
        Some(live[1].message.text_content().as_str())
    );
}

// ---------------------------------------------------------------------------
// Session-cumulative entry numbering (ADR-007)
// ---------------------------------------------------------------------------

#[test]
fn should_assign_identical_entry_seqs_when_replayed_memory_matches_live() {
    // Given a live session of four tool-exchange turns (the wire shapes a
    // real turn records: user message, step.begin, tool.call, tool.result,
    // content.part, step.end), the fourth turn also deferring a user
    // message behind its open exchange. The live materialization order and
    // the session-cumulative entry_seq each message receives (ADR-007):
    //   q0(0) a0(1) r0(2)  q1(3) a1(4) r1(5)  q2(6) a2(7) r2(8)
    //   q3(9) a3(10) r3(11) late(12 — flushed after the tool result)
    // Turn 2's question is bulky so the head-essence budget (2 000 tokens)
    // elides it behind the `<system-reminder>` marker.
    let mut mem = ContextMemory::new();
    let mut ops: Vec<WireOp> = Vec::new();
    for turn in 0..4 {
        let question = if turn == 2 {
            format!("question {turn} {}", "c".repeat(12_000))
        } else {
            format!("question {turn}")
        };
        record_message(&mut mem, &mut ops, user_msg(&question));
        let step = format!("s{turn}");
        let call = format!("call-{turn}");
        record_loop_event(
            &mut mem,
            &mut ops,
            LoopEvent::StepBegin {
                step_id: step.clone(),
                entry_seq: None,
            },
        );
        record_loop_event(
            &mut mem,
            &mut ops,
            LoopEvent::ToolCall {
                step_id: step.clone(),
                tool_call_id: call.clone(),
                tool_name: "read".to_string(),
                args: serde_json::json!({"path": "/tmp/f"}),
            },
        );
        if turn == 3 {
            // A user message arriving while the exchange is open: live
            // defers it behind the resolving tool result, so its slot —
            // and therefore its entry number — comes after the result's.
            record_message(&mut mem, &mut ops, user_msg("late question"));
        }
        record_loop_event(
            &mut mem,
            &mut ops,
            LoopEvent::ToolResult {
                tool_call_id: call.clone(),
                output: ToolOutput::Text(format!("result {turn}")),
                is_error: false,
                note: None,
                entry_seq: None,
            },
        );
        record_loop_event(
            &mut mem,
            &mut ops,
            LoopEvent::ContentPart {
                step_id: step.clone(),
                text: format!("answer {turn} ").repeat(300),
            },
        );
        record_loop_event(
            &mut mem,
            &mut ops,
            LoopEvent::StepEnd {
                step_id: step.clone(),
                input_tokens: 2_000,
                output_tokens: 100,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
        );
    }
    assert_eq!(mem.history().len(), 13);

    // And a compaction applied directly, keeping the fourth turn's four
    // live slots verbatim as the retention tail
    let tail_start = mem.history().len() - 4;
    let tokens_before = mem.token_count_with_pending();
    let input = CompactionInput {
        summary: "s".into(),
        context_summary: "SUMMARY".into(),
        compacted_count: tail_start as u32,
        tokens_before,
        dropped_count: 0,
        retention_tail_start: Some(tail_start),
    };
    let (first_replaced, last_replaced) = direct_apply_replaced_range(&mem, tail_start);
    let outcome = mem.apply_compaction(input.clone());
    assert!(outcome.applied);
    assert_eq!(outcome.retained_tail_count, 4);
    assert_eq!(outcome.kept_head_user_message_count, 2);
    assert!(outcome.elision_message.is_some());
    record_direct_apply(
        &mut ops,
        &input,
        &outcome,
        mem.history().len(),
        mem.token_count(),
        first_replaced,
        last_replaced,
    );

    // This pins the live numbering so a divergence below is attributable
    // to the replay, not to the fixture: the head essence keeps its
    // original numbers (q0 → 0, q1 → 3), the elision marker and summary
    // take the next fresh ones (13, 14 — the counter never resets), and
    // the retention tail keeps its own (9-12).
    let live_seqs: Vec<u64> = mem.history().iter().map(|m| m.entry_seq).collect();
    assert_eq!(live_seqs, vec![0, 3, 13, 14, 9, 10, 11, 12]);

    // And a post-compaction exchange: its entries continue the counter
    // without reset — 15, 16, 17 after the summary's 14.
    record_message(&mut mem, &mut ops, user_msg("question 4"));
    let step = "s4".to_string();
    let call = "call-4".to_string();
    record_loop_event(
        &mut mem,
        &mut ops,
        LoopEvent::StepBegin {
            step_id: step.clone(),
            entry_seq: None,
        },
    );
    record_loop_event(
        &mut mem,
        &mut ops,
        LoopEvent::ToolCall {
            step_id: step.clone(),
            tool_call_id: call.clone(),
            tool_name: "read".to_string(),
            args: serde_json::json!({"path": "/tmp/f"}),
        },
    );
    record_loop_event(
        &mut mem,
        &mut ops,
        LoopEvent::ToolResult {
            tool_call_id: call.clone(),
            output: ToolOutput::Text("result 4".to_string()),
            is_error: false,
            note: None,
            entry_seq: None,
        },
    );
    record_loop_event(
        &mut mem,
        &mut ops,
        LoopEvent::ContentPart {
            step_id: step.clone(),
            text: "answer 4".to_string(),
        },
    );
    record_loop_event(
        &mut mem,
        &mut ops,
        LoopEvent::StepEnd {
            step_id: step.clone(),
            input_tokens: 2_000,
            output_tokens: 100,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        },
    );

    // When the ops are folded and the memory reconstructed — the fold
    // simulation re-derives the numbering at the same materialization
    // points the live counter assigned at
    let replayed = replay_memory(&ops);

    // Then the replayed memory is the live memory, message for message and
    // number for number: every retained entry's entry_seq equals its live
    // counterpart, and the post-compaction entries continue the counter
    // without reset (compaction removed the old prefix but never reset
    // numbering, ADR-007).
    assert_histories_match(&mem, &replayed);
    let live_seqs: Vec<u64> = mem.history().iter().map(|m| m.entry_seq).collect();
    let replayed_seqs: Vec<u64> = replayed.history().iter().map(|m| m.entry_seq).collect();
    assert_eq!(replayed_seqs, live_seqs);
    assert_eq!(replayed_seqs, vec![0, 3, 13, 14, 9, 10, 11, 12, 15, 16, 17]);
}

#[test]
fn should_consume_entry_seq_when_vacuous_step_pushes_then_removes() {
    // Given a live session whose middle turn records the think-only /
    // empty-content shape: a vacuous step — step.begin pushes the empty
    // partial assistant (taking the next session-cumulative number,
    // ADR-007), a whitespace-only text part and a whitespace-only think
    // part attach without making it non-vacuous, and step.end removes it
    // (`append_loop_event`'s StepEnd arm: `tool_calls.is_empty() &&
    // content.iter().all(is_vacuous_content_part)`). The removal never
    // rewinds the counter, so the number is burned: the next
    // materialization takes previous + 2. The numbering trace:
    //   q0(0) a0(1)  q1(2) [s1 pushed 3, removed — burned]  q2(4) a2(5)
    let mut mem = ContextMemory::new();
    let mut ops: Vec<WireOp> = Vec::new();

    // Turn 0 — a plain exchange (a0 bulky so the apply below shrinks).
    record_message(&mut mem, &mut ops, user_msg("question 0"));
    for event in [
        LoopEvent::StepBegin {
            step_id: "s0".to_string(),
            entry_seq: None,
        },
        LoopEvent::ContentPart {
            step_id: "s0".to_string(),
            text: "answer 0 ".to_string().repeat(300),
        },
        LoopEvent::StepEnd {
            step_id: "s0".to_string(),
            input_tokens: 2_000,
            output_tokens: 100,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        },
    ] {
        record_loop_event(&mut mem, &mut ops, event);
    }

    // Turn 1 — the vacuous step: live pushes the partial assistant (seq 3)
    // then removes it at step.end.
    record_message(&mut mem, &mut ops, user_msg("question 1"));
    for event in [
        LoopEvent::StepBegin {
            step_id: "s1".to_string(),
            entry_seq: None,
        },
        LoopEvent::ContentPart {
            step_id: "s1".to_string(),
            text: "   ".to_string(),
        },
        LoopEvent::ThinkPart {
            step_id: "s1".to_string(),
            text: "  ".to_string(),
        },
        LoopEvent::StepEnd {
            step_id: "s1".to_string(),
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        },
    ] {
        record_loop_event(&mut mem, &mut ops, event);
    }

    // Turn 2 — the exchange after the burn.
    record_message(&mut mem, &mut ops, user_msg("question 2"));
    for event in [
        LoopEvent::StepBegin {
            step_id: "s2".to_string(),
            entry_seq: None,
        },
        LoopEvent::ContentPart {
            step_id: "s2".to_string(),
            text: "answer 2 ".to_string().repeat(300),
        },
        LoopEvent::StepEnd {
            step_id: "s2".to_string(),
            input_tokens: 2_000,
            output_tokens: 100,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        },
    ] {
        record_loop_event(&mut mem, &mut ops, event);
    }

    // Then the live numbering burned exactly one number for the vacuous
    // step: five messages with a gap at 3 — the vacuous step left no slot
    // behind but consumed the number after q1's.
    let live = mem.history();
    assert_eq!(live.len(), 5);
    let live_seqs: Vec<u64> = live.iter().map(|m| m.entry_seq).collect();
    assert_eq!(live_seqs, vec![0, 1, 2, 4, 5]);
    // The direct pin: the message after the vacuous step (q2) carries
    // q1's number + 2 — one number consumed in between.
    assert_eq!(live[3].entry_seq, live[2].entry_seq + 2);

    // And a compaction applied directly, keeping turn 2's two slots
    // verbatim as the retention tail (the vacuous step sits in the
    // replaced prefix).
    let tail_start = mem.history().len() - 2;
    let tokens_before = mem.token_count_with_pending();
    let input = CompactionInput {
        summary: "s".into(),
        context_summary: "SUMMARY".into(),
        compacted_count: tail_start as u32,
        tokens_before,
        dropped_count: 0,
        retention_tail_start: Some(tail_start),
    };
    let (first_replaced, last_replaced) = direct_apply_replaced_range(&mem, tail_start);
    let outcome = mem.apply_compaction(input.clone());
    assert!(outcome.applied);
    assert_eq!(outcome.retained_tail_count, 2);
    assert_eq!(outcome.kept_head_user_message_count, 2);
    assert!(outcome.elision_message.is_none());
    record_direct_apply(
        &mut ops,
        &input,
        &outcome,
        mem.history().len(),
        mem.token_count(),
        first_replaced,
        last_replaced,
    );

    // The live numbering after the apply: the head essence keeps its
    // numbers (q0 → 0, q1 → 2), the summary takes the next fresh one
    // (6 — the counter never reset and never rewound past the burned 3),
    // and the tail keeps its own (4, 5).
    let live_seqs: Vec<u64> = mem.history().iter().map(|m| m.entry_seq).collect();
    assert_eq!(live_seqs, vec![0, 2, 6, 4, 5]);

    // And a post-compaction exchange continues the counter: 7, 8.
    record_message(&mut mem, &mut ops, user_msg("question 3"));
    for event in [
        LoopEvent::StepBegin {
            step_id: "s3".to_string(),
            entry_seq: None,
        },
        LoopEvent::ContentPart {
            step_id: "s3".to_string(),
            text: "answer 3".to_string(),
        },
        LoopEvent::StepEnd {
            step_id: "s3".to_string(),
            input_tokens: 2_000,
            output_tokens: 100,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
        },
    ] {
        record_loop_event(&mut mem, &mut ops, event);
    }

    // When the ops are folded and the memory reconstructed — the apply's
    // fold re-derives the tail stamps through the reducer's materialization
    // simulation, whose vacuous slot create-then-die consumes its number
    // exactly like the live push-then-remove did; without that burn the
    // tail would stamp 3/4 instead of 4/5 and the round trip would diverge.
    let replayed = replay_memory(&ops);

    // Then the replayed memory is the live memory, message for message
    // and number for number — the gap at 3 survives the round trip on the
    // replay side too.
    assert_histories_match(&mem, &replayed);
    let live_seqs: Vec<u64> = mem.history().iter().map(|m| m.entry_seq).collect();
    let replayed_seqs: Vec<u64> = replayed.history().iter().map(|m| m.entry_seq).collect();
    assert_eq!(replayed_seqs, live_seqs);
    assert_eq!(live_seqs, vec![0, 2, 6, 4, 5, 7, 8]);
    // And the burn crossed the wire: the replayed numbering also steps
    // q1 → q2 by 2, with no replayed entry occupying the number between.
    assert_eq!(replayed_seqs[3], replayed_seqs[1] + 2);
}

#[test]
fn should_keep_entry_seqs_monotonic_when_undo_removes_anchored_messages() {
    // Given a session of two user turns, each with an assistant reply —
    // entry seqs 0..3 in materialization order
    let mut mem = ContextMemory::new();
    mem.append_message(user_msg("question 0"));
    mem.append_message(assistant_msg("answer 0"));
    mem.append_message(user_msg("question 1"));
    mem.append_message(assistant_msg("answer 1"));
    let pre_undo: Vec<u64> = mem.history().iter().map(|m| m.entry_seq).collect();
    assert_eq!(pre_undo, vec![0, 1, 2, 3]);

    // When undo removes the second user turn (its anchor message and the
    // trailing assistant reply), and a new exchange materializes
    mem.undo().unwrap();
    assert_eq!(mem.history().len(), 2);
    mem.append_message(user_msg("question 2"));
    mem.append_message(assistant_msg("answer 2"));

    // Then the new entries continue the session-cumulative counter: they
    // number 4 and 5 — past every pre-undo number — so the numbers of the
    // removed entries (2, 3) are never recycled and the surviving history
    // stays strictly increasing (ADR-007: undo removes messages but never
    // rolls the counter back).
    let seqs: Vec<u64> = mem.history().iter().map(|m| m.entry_seq).collect();
    assert_eq!(seqs, vec![0, 1, 4, 5]);
    assert!(seqs[2] > *pre_undo.last().unwrap());
    for window in seqs.windows(2) {
        assert!(
            window[0] < window[1],
            "entry seqs must stay strictly increasing: {seqs:?}"
        );
    }
}

#[tokio::test]
async fn should_carry_entry_range_and_dropped_seqs_when_apply_compaction_persists() {
    // Given a live memory whose old segment mixes bulky user/assistant turns
    // with an Injection-origin reminder between the first pair (the round's
    // strip filters it from the summarizer input, so the ladder's dropped
    // positions must translate through the shifted `old_index_of` table —
    // the Injection's old-segment index never surfaces as a dropped
    // position), plus two oversized hook-context messages riding on the
    // round. The L2 overflow shrink keeps only the newest of those, so the
    // dropped count exceeds the mapped positions — pinning the mapping's
    // by-positions rule (dropped_count includes hook-context slots that
    // positions skip; never derive the seqs from the count).
    let llm = MockLlm::new(vec![
        MockResponse::chat_error("context length exceeded maximum context window"),
        MockResponse::end_turn("Summary after shrink."),
    ]);
    let mut memory = ContextMemory::new();
    let mut next_seq: u64 = 100;
    let mut push = |memory: &mut ContextMemory, msg: ContextMessage| {
        // Distinct, pre-stamped numbers (offset far from any position) so
        // the assertion below proves the mapping reads the old segment's
        // entry_seq values, never the positions themselves.
        let mut msg = msg;
        msg.entry_seq = next_seq;
        next_seq += 1;
        memory.append_message(msg);
    };
    for i in 0..10 {
        push(
            &mut memory,
            ContextMessage {
                message: Message::user(format!("User message number {i}: {}", "x".repeat(16_000))),
                origin: Some(PromptOrigin::User),
                is_error: false,
                note: None,
                with_skills: None,
                entry_seq: 0,
            },
        );
        if i == 0 {
            push(
                &mut memory,
                ContextMessage {
                    message: Message::user("system-reminder injection".to_string()),
                    origin: Some(PromptOrigin::Injection),
                    is_error: false,
                    note: None,
                    with_skills: None,
                    entry_seq: 0,
                },
            );
        }
        push(
            &mut memory,
            ContextMessage {
                message: Message::assistant(format!("Assistant response number {i}")),
                origin: None,
                is_error: false,
                note: None,
                with_skills: None,
                entry_seq: 0,
            },
        );
    }
    let hook_context = vec![
        ContextMessage {
            message: Message::user(format!("hook context one: {}", "h".repeat(400_000))),
            origin: Some(PromptOrigin::HookAdditionalContext),
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        },
        ContextMessage {
            message: Message::user(format!("hook context two: {}", "h".repeat(400_000))),
            origin: Some(PromptOrigin::HookAdditionalContext),
            is_error: false,
            note: None,
            with_skills: None,
            entry_seq: 0,
        },
    ];

    // Predict the round's cut and its first L2 shrink with the same public
    // helpers the round uses (budget 1: retention_tail_tokens floors the
    // tail at the latest indivisible unit).
    let history_before: Vec<ContextMessage> = memory.history().to_vec();
    let cut = compute_retention_tail(&history_before, 1).expect("legal retention-tail cut");
    let old_segment = &history_before[..cut.tail_start];
    let mut working: Vec<ContextMessage> = old_segment
        .iter()
        .filter(|m| m.origin.as_ref() != Some(&PromptOrigin::Injection))
        .cloned()
        .collect();
    working.extend(hook_context.iter().cloned());
    let kept = mirri_core::compaction::round::shrink_history_after_overflow(&working, 1);
    let dropped_working = working.len() - kept.len();
    assert!(dropped_working > 0, "the L2 ladder must actually drop");
    // Positions = old-segment indices of the dropped working prefix; the
    // Injection's index (1) is absent — the translation table's shift path.
    let expected_positions: Vec<usize> = old_segment
        .iter()
        .enumerate()
        .filter(|(_, m)| m.origin.as_ref() != Some(&PromptOrigin::Injection))
        .map(|(idx, _)| idx)
        .take(dropped_working)
        .collect();
    let expected_seqs: Vec<u64> = expected_positions
        .iter()
        .map(|&position| old_segment[position].entry_seq)
        .collect();
    assert!(!expected_positions.contains(&1), "Injection skipped");
    assert!(!expected_seqs.contains(&101), "Injection seq skipped");

    // When the compaction runs through the real orchestration (begin's
    // closure maps the ladder's positions through the old segment's entry
    // seqs; the apply path derives the replaced range from the live re-cut)
    let mut fc = FullCompaction::new(CompactionStrategy {
        retention_tail_tokens: Some(1),
        ..Default::default()
    });
    let llm: Arc<dyn Llm> = Arc::new(llm);
    fc.begin(
        CompactionSource::Auto,
        CompactionBeginParams {
            llm,
            cancel: CancelToken::new(),
            custom_instruction: None,
            max_output_size: None,
            hook_additional_context: hook_context,
        },
        &memory,
    )
    .unwrap();
    let handle = fc.take_active_task().expect("begin started a compaction");
    let round = handle
        .await
        .expect("compaction task must not panic")
        .expect("compaction round must succeed");
    let result = fc.apply_compaction_result(&mut memory, round);

    // Then the result carries the provenance the three write points persist
    // on the wire op (ADR-007): the replaced prefix's closed entry range
    // (the old segment's numbers, min to max) and the dropped entries' seqs
    // in ascending order.
    assert!(result.applied, "compaction must apply");
    assert_eq!(result.dropped_entry_positions, expected_positions);
    assert_eq!(result.dropped_entry_seqs, expected_seqs);
    // The count exceeds the mapped list: one hook-context slot was dropped
    // by the ladder but has no old-segment position. The seq list is
    // derived BY POSITIONS, never from dropped_count.
    assert!(result.dropped_count > result.dropped_entry_seqs.len());
    assert_eq!(result.dropped_count, dropped_working);
    // The range brackets every seq folded away: first = the prefix's
    // minimum entry number, last = its maximum (re-derivation invariant).
    assert_eq!(
        result.first_replaced_entry_seq,
        old_segment.iter().map(|m| m.entry_seq).min()
    );
    assert_eq!(
        result.last_replaced_entry_seq,
        old_segment.iter().map(|m| m.entry_seq).max()
    );
}

#[tokio::test]
async fn should_replay_to_the_same_entry_numbering_when_undo_precedes_a_compaction_apply() {
    // Given three recorded user/assistant turns (entry seqs 0..5 in
    // materialization order)
    let mut mem = ContextMemory::new();
    let mut ops: Vec<WireOp> = Vec::new();
    for turn in 0..3 {
        record_message(&mut mem, &mut ops, user_msg(&format!("question {turn}")));
        let answer = if turn == 1 {
            "a".repeat(4_000)
        } else {
            format!("answer {turn}")
        };
        record_message(&mut mem, &mut ops, assistant_msg(&answer));
    }
    let pre_undo: Vec<u64> = mem.history().iter().map(|m| m.entry_seq).collect();
    assert_eq!(pre_undo, vec![0, 1, 2, 3, 4, 5]);

    // And an undo removing the third turn (its anchor question and the
    // trailing assistant reply — messages carrying seqs 4 and 5). The live
    // counter never rewinds (ADR-007), so the fold cannot re-derive the
    // counter on its own: the apply record's interval fields close that
    // gap. The fold's undo arm truncates `count` trailing messages, so the
    // op records the message count live removed (2).
    mem.undo().unwrap();
    assert_eq!(mem.history().len(), 4);
    ops.push(WireOp::ContextUndo { count: 2 });

    // And a new turn materializing past every removed number: seqs 6 (the
    // bulky question the head budget elides), 7, and 8
    record_message(
        &mut mem,
        &mut ops,
        user_msg(&format!("question 3 {}", "b".repeat(16_000))),
    );
    record_message(&mut mem, &mut ops, assistant_msg("answer 3"));
    record_message(&mut mem, &mut ops, user_msg("question 4"));
    let after_undo: Vec<u64> = mem.history().iter().map(|m| m.entry_seq).collect();
    assert_eq!(after_undo, vec![0, 1, 2, 3, 6, 7, 8]);

    // And a compaction through the real orchestration: the retention
    // budget (floor 1) keeps the last message as the tail, so the replaced
    // prefix spans the undo's number gap — its live numbers run 0, 1, 2, 3,
    // 6, 7, bracketing the removed 4 and 5.
    let llm: Arc<dyn Llm> = Arc::new(MockLlm::new(vec![MockResponse::end_turn(
        "SEGMENT SUMMARY",
    )]));
    let mut fc = FullCompaction::new(CompactionStrategy {
        retention_tail_tokens: Some(1),
        ..Default::default()
    });
    fc.begin(CompactionSource::Auto, sample_begin_params(llm), &mem)
        .unwrap();
    let result = fc.block(&mut mem, &CancelToken::new()).await.unwrap();
    assert!(result.applied, "compaction must apply");
    // The interval fields bracket every seq folded away, undo gap included
    // (the range is a bracket, not a contiguous run).
    assert_eq!(result.first_replaced_entry_seq, Some(0));
    assert_eq!(result.last_replaced_entry_seq, Some(7));
    assert_eq!(result.retained_tail_count, 1);
    assert_eq!(result.kept_head_user_message_count, 2);
    assert!(result.elision_message.is_some());

    // This pins the live numbering so a divergence below is attributable to
    // the replay, not the fixture: the head essence keeps 0 and 2, the
    // elision marker (9) and the summary (10) take the counter's next
    // values — past the removed 4 and 5 — and the tail keeps 8.
    record_apply_ops(&mut ops, &result, mem.history().len());
    let live_seqs: Vec<u64> = mem.history().iter().map(|m| m.entry_seq).collect();
    assert_eq!(live_seqs, vec![0, 2, 9, 10, 8]);

    // And a post-compaction entry continuing the session counter
    record_message(&mut mem, &mut ops, user_msg("question 5"));

    // When the ops are folded and the memory reconstructed — the reducer
    // re-derives the numbering from the record's interval fields (the
    // simulation's own prefix bracket compared against the record's, the
    // difference being exactly the invisible undo gap)
    let replayed = replay_memory(&ops);

    // Then the replayed numbering is the live numbering, entry for entry:
    // retained entries keep their numbers, the marker/summary take the
    // live counter's values (9, 10 — not the fold's naive 7, 8), the tail
    // keeps 8, and post-compaction entries continue at 11.
    assert_histories_match(&mem, &replayed);
    let live_seqs: Vec<u64> = mem.history().iter().map(|m| m.entry_seq).collect();
    let replayed_seqs: Vec<u64> = replayed.history().iter().map(|m| m.entry_seq).collect();
    assert_eq!(replayed_seqs, live_seqs);
    assert_eq!(replayed_seqs, vec![0, 2, 9, 10, 8, 11]);
}

/// The instruction of the n-th summarizer call (0-based): the last message
/// the round pushes onto the request (the instruction rides after the
/// projected history), as full text.
fn instruction_of_call(mock: &MockLlm, call: usize) -> String {
    mock.messages_of_call(call)
        .last()
        .map(|m| m.text_content())
        .unwrap_or_default()
}

#[tokio::test]
async fn should_tell_summarizer_the_entry_range_and_dropped_list_when_instructed() {
    // Given a compaction begin over an old segment of pre-stamped entries
    // (numbers 100.., alternating bulky user / assistant pairs), where the
    // first summarizer call overflows so the ladder's L2 shrink excludes a
    // known prefix of the old segment and the retry succeeds
    let llm = MockLlm::new(vec![
        MockResponse::chat_error("context length exceeded maximum context window"),
        MockResponse::end_turn("Summary after shrink."),
    ]);
    let mock = Arc::new(llm);
    let mut memory = ContextMemory::new();
    let mut next_seq: u64 = 100;
    let mut push = |memory: &mut ContextMemory, msg: ContextMessage| {
        // Distinct, pre-stamped numbers (offset far from any position) so
        // the assertions below prove the instruction reads the old
        // segment's entry_seq values, never the positions themselves.
        let mut msg = msg;
        msg.entry_seq = next_seq;
        next_seq += 1;
        memory.append_message(msg);
    };
    for i in 0..10 {
        push(
            &mut memory,
            ContextMessage {
                message: Message::user(format!("User message number {i}: {}", "x".repeat(16_000))),
                origin: Some(PromptOrigin::User),
                is_error: false,
                note: None,
                with_skills: None,
                entry_seq: 0,
            },
        );
        push(
            &mut memory,
            ContextMessage {
                message: Message::assistant(format!("Assistant response number {i}")),
                origin: None,
                is_error: false,
                note: None,
                with_skills: None,
                entry_seq: 0,
            },
        );
    }

    // Predict the planned cut and its first L2 shrink with the same public
    // helpers the round uses (budget 1: retention_tail_tokens floors the
    // tail at the latest indivisible unit).
    let history_before: Vec<ContextMessage> = memory.history().to_vec();
    let cut = compute_retention_tail(&history_before, 1).expect("legal retention-tail cut");
    let old_segment = &history_before[..cut.tail_start];
    let expected_first = old_segment.iter().map(|m| m.entry_seq).min().unwrap();
    let expected_last = old_segment.iter().map(|m| m.entry_seq).max().unwrap();
    let kept = mirri_core::compaction::round::shrink_history_after_overflow(old_segment, 1);
    let dropped_working = old_segment.len() - kept.len();
    assert!(dropped_working > 0, "the L2 ladder must actually drop");
    let expected_dropped_seqs: Vec<u64> = old_segment[..dropped_working]
        .iter()
        .map(|m| m.entry_seq)
        .collect();

    // When the compaction runs through the real orchestration (begin plans
    // the cut and threads the range into the round; the ladder's drops map
    // to entry seqs for the retry's instruction)
    let mut fc = FullCompaction::new(CompactionStrategy {
        retention_tail_tokens: Some(1),
        ..Default::default()
    });
    let llm: Arc<dyn Llm> = mock.clone();
    fc.begin(
        CompactionSource::Auto,
        CompactionBeginParams {
            llm,
            cancel: CancelToken::new(),
            custom_instruction: None,
            max_output_size: None,
            hook_additional_context: Vec::new(),
        },
        &memory,
    )
    .unwrap();
    let handle = fc.take_active_task().expect("begin started a compaction");
    let round = handle
        .await
        .expect("compaction task must not panic")
        .expect("compaction round must succeed");
    let result = fc.apply_compaction_result(&mut memory, round);
    assert!(result.applied, "compaction must apply");

    // Then the first attempt's instruction states the planned range and the
    // reference convention — with no exclusion sentence (nothing has been
    // dropped yet)
    let first_instruction = instruction_of_call(&mock, 0);
    assert!(
        first_instruction.contains(&format!(
            "entries #{expected_first} through #{expected_last}"
        )),
        "instruction must state the planned range: {first_instruction}"
    );
    assert!(
        first_instruction.contains("they are stable"),
        "instruction must state the reference convention: {first_instruction}"
    );
    assert!(
        !first_instruction.contains("Entries excluded"),
        "no exclusion sentence before any drop: {first_instruction}"
    );

    // And the retry's instruction still states the range, lists the dropped
    // entries explicitly, and marks them not recoverable from the note
    let retry_instruction = instruction_of_call(&mock, 1);
    let dropped_list = expected_dropped_seqs
        .iter()
        .map(|seq| format!("#{seq}"))
        .collect::<Vec<_>>()
        .join(", ");
    assert!(
        retry_instruction.contains(&format!(
            "Entries excluded from your input due to size limits: {dropped_list}."
        )),
        "instruction must list the dropped entries: {retry_instruction}"
    );
    assert!(
        retry_instruction.contains("not recoverable from this note"),
        "excluded entries must be marked not recoverable: {retry_instruction}"
    );
    assert!(retry_instruction.contains(&format!(
        "entries #{expected_first} through #{expected_last}"
    )));

    // And event.compaction.completed carries the applied range and the
    // dropped count (a count, never the list)
    let projected = project(&SessionEvent::CompactionCompleted {
        agent_id: "main".to_string(),
        result: result.clone(),
    })
    .expect("compaction completed projects");
    assert_eq!(projected.method, "event.compaction.completed");
    assert_eq!(
        projected.payload["result"]["firstReplacedEntrySeq"].as_u64(),
        Some(expected_first)
    );
    assert_eq!(
        projected.payload["result"]["lastReplacedEntrySeq"].as_u64(),
        Some(expected_last)
    );
    assert_eq!(
        projected.payload["result"]["droppedEntrySeqCount"].as_u64(),
        Some(expected_dropped_seqs.len() as u64)
    );
}

#[tokio::test]
async fn should_freeze_the_provenance_instruction_text_when_composed() {
    // Given a compaction begin over an old segment of pre-stamped entries
    // (numbers 100.., alternating bulky user / assistant pairs), where the
    // first summarizer call overflows so the ladder's L2 shrink excludes a
    // known prefix of the old segment and the retry succeeds
    let llm = MockLlm::new(vec![
        MockResponse::chat_error("context length exceeded maximum context window"),
        MockResponse::end_turn("Summary after shrink."),
    ]);
    let mock = Arc::new(llm);
    let mut memory = ContextMemory::new();
    let mut next_seq: u64 = 100;
    let mut push = |memory: &mut ContextMemory, msg: ContextMessage| {
        // Distinct, pre-stamped numbers (offset far from any position) so
        // the frozen text below reads the old segment's entry_seq values,
        // never the positions themselves.
        let mut msg = msg;
        msg.entry_seq = next_seq;
        next_seq += 1;
        memory.append_message(msg);
    };
    for i in 0..10 {
        push(
            &mut memory,
            ContextMessage {
                message: Message::user(format!("User message number {i}: {}", "x".repeat(16_000))),
                origin: Some(PromptOrigin::User),
                is_error: false,
                note: None,
                with_skills: None,
                entry_seq: 0,
            },
        );
        push(
            &mut memory,
            ContextMessage {
                message: Message::assistant(format!("Assistant response number {i}")),
                origin: None,
                is_error: false,
                note: None,
                with_skills: None,
                entry_seq: 0,
            },
        );
    }

    // Predict the planned cut and its first L2 shrink with the same public
    // helpers the round uses, so the frozen expectations carry exactly the
    // numbers the live instruction will state.
    let history_before: Vec<ContextMessage> = memory.history().to_vec();
    let cut = compute_retention_tail(&history_before, 1).expect("legal retention-tail cut");
    let old_segment = &history_before[..cut.tail_start];
    let expected_first = old_segment.iter().map(|m| m.entry_seq).min().unwrap();
    let expected_last = old_segment.iter().map(|m| m.entry_seq).max().unwrap();
    let kept = mirri_core::compaction::round::shrink_history_after_overflow(old_segment, 1);
    let dropped_working = old_segment.len() - kept.len();
    assert!(dropped_working > 0, "the L2 ladder must actually drop");
    let expected_dropped_seqs: Vec<u64> = old_segment[..dropped_working]
        .iter()
        .map(|m| m.entry_seq)
        .collect();
    let dropped_list = expected_dropped_seqs
        .iter()
        .map(|seq| format!("#{seq}"))
        .collect::<Vec<_>>()
        .join(", ");

    // When the compaction runs through the real orchestration (the first
    // attempt has no drops; the retry carries the dropped list)
    let mut fc = FullCompaction::new(CompactionStrategy {
        retention_tail_tokens: Some(1),
        ..Default::default()
    });
    let llm: Arc<dyn Llm> = mock.clone();
    fc.begin(
        CompactionSource::Auto,
        CompactionBeginParams {
            llm,
            cancel: CancelToken::new(),
            custom_instruction: None,
            max_output_size: None,
            hook_additional_context: Vec::new(),
        },
        &memory,
    )
    .unwrap();
    let handle = fc.take_active_task().expect("begin started a compaction");
    let round = handle
        .await
        .expect("compaction task must not panic")
        .expect("compaction round must succeed");
    let result = fc.apply_compaction_result(&mut memory, round);
    assert!(result.applied, "compaction must apply");

    // Then the instruction text is frozen byte-for-byte (em-dash, newlines,
    // and spacing included) — the ADR-007 mandated prose. The expected
    // literals below are the authoritative record; any wording change in
    // round.rs's `build_compaction_instruction` must be mirrored here
    // deliberately. The raw strings are flush-left because every byte of
    // their content is significant.
    let frozen_no_drops = format!(
        r#"You are summarizing conversation entries #{expected_first} through #{expected_last}. You may reference
these entry numbers in your note (e.g. "see entries #60-#70") — they are stable
across compactions.

You are writing a handoff note to yourself. Summarize the conversation so far in first person, present tense.

Preserve:
- The user's intent and any explicit constraints
- What has been done: exact commands, file paths, and key results
- What is still unknown or uncertain
- The forward plan

Be concise but complete. Respond with text only. Do not call any tools.
"#
    );
    assert_eq!(
        instruction_of_call(&mock, 0),
        frozen_no_drops,
        "the no-drop instruction must match the frozen text verbatim"
    );

    let frozen_with_drops = format!(
        r#"You are summarizing conversation entries #{expected_first} through #{expected_last}. You may reference
these entry numbers in your note (e.g. "see entries #60-#70") — they are stable
across compactions. Entries excluded from your input due to size limits: {dropped_list}.
Those excluded entries are not recoverable from this note.

You are writing a handoff note to yourself. Summarize the conversation so far in first person, present tense.

Preserve:
- The user's intent and any explicit constraints
- What has been done: exact commands, file paths, and key results
- What is still unknown or uncertain
- The forward plan

Be concise but complete. Respond with text only. Do not call any tools.
"#
    );
    assert_eq!(
        instruction_of_call(&mock, 1),
        frozen_with_drops,
        "the with-drops instruction must match the frozen text verbatim"
    );
}

// ---------------------------------------------------------------------------
// Apply-record provenance mapping — every production write point (ADR-007)
// ---------------------------------------------------------------------------

/// A [`mirri_core::WireOpRecorder`] test double collecting every recorded op
/// in dispatch order (the turn_flow_tests.rs idiom).
struct ProvenanceWireRecorder(Arc<Mutex<Vec<WireOp>>>);

impl mirri_core::WireOpRecorder for ProvenanceWireRecorder {
    fn record_op<'a>(
        &'a self,
        op: WireOp,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        let collected = Arc::clone(&self.0);
        Box::pin(async move {
            collected.lock().unwrap_or_else(|e| e.into_inner()).push(op);
        })
    }
}

/// The ADR-007 provenance triple carried by a recorded compaction apply op.
fn apply_op_provenance(op: &WireOp) -> (Option<u64>, Option<u64>, Option<Vec<u64>>) {
    match op {
        WireOp::ContextApplyCompaction {
            first_replaced_entry_seq,
            last_replaced_entry_seq,
            dropped_entry_seqs,
            ..
        } => (
            *first_replaced_entry_seq,
            *last_replaced_entry_seq,
            dropped_entry_seqs.clone(),
        ),
        other => panic!("expected a compaction apply op, got {other:?}"),
    }
}

/// Seed a fresh memory with one prior exchange — entry seqs 0 through 2
/// (user message, an injection-origin reminder, assistant reply), the
/// same prefix shape an embedded session's first turn used to leave and
/// still representative of one with a mid-exchange injection. The bulky
/// assistant reply keeps the shrink check (变小校验) comfortably passing.
fn seed_provenance_memory() -> ContextMemory {
    let mut memory = ContextMemory::new();
    memory.append_user_message(
        vec![ContentPart::Text {
            text: "previous question".to_string(),
        }],
        PromptOrigin::User,
    );
    memory.append_message(ContextMessage {
        message: Message::user("system-reminder injection"),
        origin: Some(PromptOrigin::Injection),
        is_error: false,
        note: None,
        with_skills: None,
        entry_seq: 0,
    });
    memory.append_message(ContextMessage {
        message: Message::assistant(format!("previous answer: {}", "a".repeat(800))),
        origin: None,
        is_error: false,
        note: None,
        with_skills: None,
        entry_seq: 0,
    });
    memory
}

/// The provenance triple a wire write point must derive from an applied
/// [`CompactionResult`](mirri_core::compaction::CompactionResult) —
/// deliberately computed field-for-field here, never through the production
/// mapping, so a mapping bug cannot self-confirm.
fn result_provenance(
    result: &mirri_core::compaction::CompactionResult,
) -> (Option<u64>, Option<u64>, Option<Vec<u64>>) {
    (
        result.first_replaced_entry_seq,
        result.last_replaced_entry_seq,
        (!result.dropped_entry_seqs.is_empty()).then(|| result.dropped_entry_seqs.clone()),
    )
}

/// The wire apply record's ADR-007 provenance must map the applied
/// `CompactionResult` identically from every production write point: the
/// turn_flow overflow-recovery block path and the auto-compaction hooks
/// path are driven here through real turns; the embedded manual path is
/// pinned to the same triple by its own test in mirri-embedded. A first↔last
/// swap at any write point compiles silently (symmetric `Option<u64>`s) and
/// turns this red — the replaced prefix spans entry seqs 0 through 2, so
/// first < last is observable, not coincidental.
#[tokio::test]
async fn should_map_provenance_fields_identically_when_written_from_all_production_points() {
    use mirri_core::tool_registry::ValidatorCache;
    use mirri_core::turn_flow::{TurnFlow, TurnFlowConfig};

    // Both mirri-core paths compact the same prefix, so both must persist
    // the same triple the embedded manual path is pinned to as well:
    // first = Some(0), last = Some(2), nothing dropped (an empty dropped
    // list stays absent on the wire, never an empty array).
    let expected = (Some(0u64), Some(2u64), None);

    // --- Path 1: the auto-compaction hooks path (turn_flow/hooks.rs) ---
    let mut hooks_flow = TurnFlow::new(TurnFlowConfig {
        compaction_strategy: CompactionStrategy {
            max_context_tokens: 30,
            trigger_ratio: 0.5,
            ..CompactionStrategy::default()
        },
        ..TurnFlowConfig::default()
    });
    let hooks_bus = Arc::new(mirri_core::session::events::EventBus::new(64));
    hooks_flow.set_event_bus(Arc::clone(&hooks_bus));
    let mut hooks_events = hooks_bus.subscribe();
    let hooks_ops = Arc::new(Mutex::new(Vec::new()));
    hooks_flow.set_wire_recorder(Some(
        Arc::new(ProvenanceWireRecorder(Arc::clone(&hooks_ops)))
            as Arc<dyn mirri_core::WireOpRecorder>,
    ));
    let hooks_llm = MockLlm::new(vec![
        MockResponse::end_turn("Hooks-path summary of the prior exchange."),
        MockResponse::end_turn("Done!"),
    ]);
    let hooks_memory = Arc::new(Mutex::new(seed_provenance_memory()));
    let hooks_turn = hooks_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "x".repeat(200),
            }],
            PromptOrigin::User,
            Arc::clone(&hooks_memory),
            &ToolManager::new(),
            Arc::new(FixedLlmResolver::new(Arc::new(hooks_llm))),
            &CancelToken::new(),
            &mut ValidatorCache::new(),
            None, None,
        )
        .await
        .expect("the hooks-path turn should succeed");
    assert_eq!(hooks_turn.stop_reason, mirri_core::TurnStopReason::EndTurn);

    // The applied result arrives on the session bus — and its apply op is on
    // the wire with the identical provenance.
    let mut hooks_result = None;
    while let Ok(event) = hooks_events.try_recv() {
        if let SessionEvent::CompactionCompleted { result, .. } = event {
            hooks_result = Some(result);
        }
    }
    let hooks_result = hooks_result.expect("the hooks path must publish CompactionCompleted");
    let hooks_apply = hooks_ops
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|op| matches!(op, WireOp::ContextApplyCompaction { .. }))
        .expect("the hooks path must record a ContextApplyCompaction op")
        .clone();
    let hooks_triple = apply_op_provenance(&hooks_apply);
    assert_eq!(
        hooks_triple,
        result_provenance(&hooks_result),
        "the hooks-path wire op must mirror the applied CompactionResult"
    );
    assert_eq!(hooks_triple, expected);

    // --- Path 2: the overflow-recovery block path (turn_flow/mod.rs) ---
    // The threshold trips identically, but the summarizer first exhausts
    // the L2 overflow ladder: every attempt reports a provider context
    // overflow, the round gives up with EngineError::ContextOverflow,
    // before_step propagates it, run_turn fails, and the turn_flow
    // overflow-recovery block path runs its own begin → apply → record.
    let mut block_flow = TurnFlow::new(TurnFlowConfig {
        compaction_strategy: CompactionStrategy {
            max_context_tokens: 30,
            trigger_ratio: 0.5,
            ..CompactionStrategy::default()
        },
        ..TurnFlowConfig::default()
    });
    let block_bus = Arc::new(mirri_core::session::events::EventBus::new(64));
    block_flow.set_event_bus(Arc::clone(&block_bus));
    let mut block_events = block_bus.subscribe();
    let block_ops = Arc::new(Mutex::new(Vec::new()));
    block_flow.set_wire_recorder(Some(
        Arc::new(ProvenanceWireRecorder(Arc::clone(&block_ops)))
            as Arc<dyn mirri_core::WireOpRecorder>,
    ));
    let overflow_error =
        || MockResponse::chat_error("context length exceeded maximum context window");
    let block_llm = MockLlm::new(vec![
        // Round 1 (begun by before_step): four overflow reports exhaust the
        // L2 ladder (MAX_COMPACTION_OVERFLOW_SHRINK_ATTEMPTS + 1) so the
        // round fails with EngineError::ContextOverflow.
        overflow_error(),
        overflow_error(),
        overflow_error(),
        overflow_error(),
        // Round 2 (begun by the block path itself): succeeds and applies.
        MockResponse::end_turn("Block-path summary of the prior exchange."),
        // The retried turn's LLM step.
        MockResponse::end_turn("Done!"),
    ]);
    let block_memory = Arc::new(Mutex::new(seed_provenance_memory()));
    let block_turn = block_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "x".repeat(200),
            }],
            PromptOrigin::User,
            Arc::clone(&block_memory),
            &ToolManager::new(),
            Arc::new(FixedLlmResolver::new(Arc::new(block_llm))),
            &CancelToken::new(),
            &mut ValidatorCache::new(),
            None, None,
        )
        .await
        .expect("the block-path turn should recover and succeed");
    assert_eq!(block_turn.stop_reason, mirri_core::TurnStopReason::EndTurn);

    // The recovered apply's result arrives on the session bus — and its
    // apply op is on the wire with the identical provenance.
    let mut block_result = None;
    while let Ok(event) = block_events.try_recv() {
        if let SessionEvent::CompactionCompleted { result, .. } = event {
            block_result = Some(result);
        }
    }
    let block_result = block_result.expect("the block path must publish CompactionCompleted");
    let block_apply = block_ops
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .find(|op| matches!(op, WireOp::ContextApplyCompaction { .. }))
        .expect("the block path must record a ContextApplyCompaction op")
        .clone();
    let block_triple = apply_op_provenance(&block_apply);
    assert_eq!(
        block_triple,
        result_provenance(&block_result),
        "the block-path wire op must mirror the applied CompactionResult"
    );

    // And both production write points persisted the identical triple.
    assert_eq!(hooks_triple, block_triple);
    assert_eq!(block_triple, expected);
}

/// Snapshot a history as comparable JSON — byte-identical means identical
/// `serde_json` encodings of every `ContextMessage`, field for field.
fn history_snapshot(memory: &ContextMemory) -> Vec<serde_json::Value> {
    memory
        .history()
        .iter()
        .map(|entry| serde_json::to_value(entry).expect("ContextMessage serializes"))
        .collect()
}

/// A compaction the must-shrink guard (变小校验, P8a-3) refuses must never
/// persist an apply record (S6): `apply_compaction` keeps the original
/// history and reports `applied: false`, so a write point that recorded
/// `WireOp::ContextApplyCompaction` anyway would corrupt `session.resume` —
/// the replay would apply a summary that never landed in memory. Both
/// mirri-core write points (the auto-compaction hooks path and the
/// overflow-recovery block path) are driven through real turns; the
/// embedded manual path is pinned to the same contract by its own test in
/// mirri-embedded. Removing the `if applied` gate at either write point
/// turns this red — the refused apply would appear on the wire.
#[tokio::test]
async fn should_not_persist_an_apply_record_when_the_must_shrink_guard_refuses() {
    use mirri_core::tool_registry::ValidatorCache;
    use mirri_core::turn_flow::{TurnFlow, TurnFlowConfig};

    // A summary far larger than the whole seeded history: the rewritten
    // history (head essence + summary + retained tail) cannot be smaller
    // than the original, so the shrink check rejects it no matter where
    // the retention cut lands.
    let oversized_summary = "z".repeat(20_000);

    // --- Path 1: the auto-compaction hooks path (turn_flow/hooks.rs) ---
    let mut hooks_flow = TurnFlow::new(TurnFlowConfig {
        compaction_strategy: CompactionStrategy {
            max_context_tokens: 30,
            trigger_ratio: 0.5,
            ..CompactionStrategy::default()
        },
        ..TurnFlowConfig::default()
    });
    let hooks_bus = Arc::new(mirri_core::session::events::EventBus::new(64));
    hooks_flow.set_event_bus(Arc::clone(&hooks_bus));
    let mut hooks_events = hooks_bus.subscribe();
    let hooks_ops = Arc::new(Mutex::new(Vec::new()));
    hooks_flow.set_wire_recorder(Some(
        Arc::new(ProvenanceWireRecorder(Arc::clone(&hooks_ops)))
            as Arc<dyn mirri_core::WireOpRecorder>,
    ));
    let hooks_llm = MockLlm::new(vec![
        // The summarizer's refusal-sized summary — the shrink check rejects
        // it at apply time (context_memory/mod.rs), the round itself
        // succeeds.
        MockResponse::end_turn(&oversized_summary),
        // The turn's own LLM step runs on the un-compacted context.
        MockResponse::end_turn("Done!"),
    ]);
    let hooks_memory = Arc::new(Mutex::new(seed_provenance_memory()));
    // The pre-compaction prefix the guard must preserve byte-for-byte: the
    // seed's three entries (the compacted-away bulky reply among them).
    let hooks_pre = history_snapshot(&hooks_memory.lock().unwrap_or_else(|e| e.into_inner()));
    assert_eq!(hooks_pre.len(), 3);

    let hooks_turn = hooks_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "x".repeat(200),
            }],
            PromptOrigin::User,
            Arc::clone(&hooks_memory),
            &ToolManager::new(),
            Arc::new(FixedLlmResolver::new(Arc::new(hooks_llm))),
            &CancelToken::new(),
            &mut ValidatorCache::new(),
            None, None,
        )
        .await
        .expect("the turn must survive a refused compaction");
    assert_eq!(hooks_turn.stop_reason, mirri_core::TurnStopReason::EndTurn);

    // (1) The refusal is reported: exactly one completed compaction, and it
    // was not applied.
    let mut hooks_completed = Vec::new();
    while let Ok(event) = hooks_events.try_recv() {
        if let SessionEvent::CompactionCompleted { result, .. } = event {
            hooks_completed.push(result);
        }
    }
    assert_eq!(
        hooks_completed.len(),
        1,
        "exactly one compaction must complete on the hooks path"
    );
    assert!(
        !hooks_completed[0].applied,
        "the oversized summary must fail the shrink check: {:?}",
        hooks_completed[0]
    );

    // (2) The wire log carries no apply record — neither the apply op nor
    // the request-header change both write points gate on `applied`.
    let recorded = hooks_ops.lock().unwrap_or_else(|e| e.into_inner()).clone();
    for op in &recorded {
        assert!(
            !matches!(op, WireOp::ContextApplyCompaction { .. }),
            "a refused compaction must not persist an apply record: {op:?}"
        );
        if let WireOp::RequestHeaderChanged { reason } = op {
            assert_ne!(
                reason, "compaction_applied",
                "a refused compaction must not announce a compaction-applied header change"
            );
        }
    }

    // (3) The history is byte-identical to the pre-compaction prefix: the
    // seed's entries survive verbatim (nothing was rewritten, the bulky
    // compacted-away reply included), followed by the turn's own user
    // prompt and reply — never a CompactionSummary-origin entry.
    {
        let mem = hooks_memory.lock().unwrap_or_else(|e| e.into_inner());
        let history = mem.history();
        assert!(
            history.len() >= 4,
            "seed prefix + the turn's user prompt at minimum, got {}",
            history.len()
        );
        let snapshot = history_snapshot(&mem);
        assert_eq!(
            &snapshot[..3],
            &hooks_pre[..],
            "the refused compaction must leave the pre-compaction history byte-identical"
        );
        assert_eq!(history[3].origin, Some(PromptOrigin::User));
        assert_eq!(history[3].message.text_content(), "x".repeat(200));
        assert!(
            history
                .iter()
                .all(|entry| entry.origin != Some(PromptOrigin::CompactionSummary)),
            "a refused compaction must not materialize a summary entry"
        );
    }

    // --- Path 2: the overflow-recovery block path (turn_flow/mod.rs) ---
    let mut block_flow = TurnFlow::new(TurnFlowConfig {
        compaction_strategy: CompactionStrategy {
            max_context_tokens: 30,
            trigger_ratio: 0.5,
            ..CompactionStrategy::default()
        },
        ..TurnFlowConfig::default()
    });
    let block_bus = Arc::new(mirri_core::session::events::EventBus::new(64));
    block_flow.set_event_bus(Arc::clone(&block_bus));
    let mut block_events = block_bus.subscribe();
    let block_ops = Arc::new(Mutex::new(Vec::new()));
    block_flow.set_wire_recorder(Some(
        Arc::new(ProvenanceWireRecorder(Arc::clone(&block_ops)))
            as Arc<dyn mirri_core::WireOpRecorder>,
    ));
    let overflow_error =
        || MockResponse::chat_error("context length exceeded maximum context window");
    let block_llm = MockLlm::new(vec![
        // Round 1 (begun by before_step): four overflow reports exhaust the
        // L2 ladder so the round fails with EngineError::ContextOverflow
        // and the overflow-recovery block path takes over.
        overflow_error(),
        overflow_error(),
        overflow_error(),
        overflow_error(),
        // Round 2 (begun by the block path itself): the refusal-sized
        // summary the shrink check rejects.
        MockResponse::end_turn(&oversized_summary),
        // The retried turn's LLM step.
        MockResponse::end_turn("Done!"),
    ]);
    let block_memory = Arc::new(Mutex::new(seed_provenance_memory()));
    let block_pre = history_snapshot(&block_memory.lock().unwrap_or_else(|e| e.into_inner()));

    let block_turn = block_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "x".repeat(200),
            }],
            PromptOrigin::User,
            Arc::clone(&block_memory),
            &ToolManager::new(),
            Arc::new(FixedLlmResolver::new(Arc::new(block_llm))),
            &CancelToken::new(),
            &mut ValidatorCache::new(),
            None, None,
        )
        .await
        .expect("the block-path turn must recover from a refused compaction");
    assert_eq!(block_turn.stop_reason, mirri_core::TurnStopReason::EndTurn);

    // (1) The block path's own compaction completed — refused.
    let mut block_completed = Vec::new();
    while let Ok(event) = block_events.try_recv() {
        if let SessionEvent::CompactionCompleted { result, .. } = event {
            block_completed.push(result);
        }
    }
    assert_eq!(
        block_completed.len(),
        1,
        "exactly one compaction must complete on the block path"
    );
    assert!(
        !block_completed[0].applied,
        "the oversized summary must fail the shrink check: {:?}",
        block_completed[0]
    );

    // (2) No apply record from the block-path write point either.
    let recorded = block_ops.lock().unwrap_or_else(|e| e.into_inner()).clone();
    for op in &recorded {
        assert!(
            !matches!(op, WireOp::ContextApplyCompaction { .. }),
            "a refused compaction must not persist an apply record: {op:?}"
        );
        if let WireOp::RequestHeaderChanged { reason } = op {
            assert_ne!(
                reason, "compaction_applied",
                "a refused compaction must not announce a compaction-applied header change"
            );
        }
    }

    // (3) And its history is byte-identical to the pre-compaction prefix.
    {
        let mem = block_memory.lock().unwrap_or_else(|e| e.into_inner());
        let history = mem.history();
        assert!(history.len() >= 4);
        let snapshot = history_snapshot(&mem);
        assert_eq!(
            &snapshot[..3],
            &block_pre[..],
            "the refused compaction must leave the pre-compaction history byte-identical"
        );
        assert_eq!(history[3].origin, Some(PromptOrigin::User));
        assert_eq!(history[3].message.text_content(), "x".repeat(200));
        assert!(
            history
                .iter()
                .all(|entry| entry.origin != Some(PromptOrigin::CompactionSummary)),
            "a refused compaction must not materialize a summary entry"
        );
    }
}

// ---------------------------------------------------------------------------
// Configurable capacity caps — end-to-end resume through the session manager
// ---------------------------------------------------------------------------

/// The end-to-end resume gate for the configurable capacity keys (ROADMAP
/// deferred items: the mirri-server restore face, the manager-level
/// integration, the close-time-snapshot/resume ceiling symmetry).
mod capacity_resume_e2e {
    use super::*;
    use mirri_core::scope::SessionScopeConfig;
    use mirri_core::{
        AgentId, AgentScopeConfig, AppScope, AppScopeConfig, CloseReason, ServerConfig, SessionId,
        SessionManager, SessionMetadata, WorkspaceId, WorkspaceScopeConfig, WorkspaceTrustLevel,
        config::LoopControlConfig,
    };
    use mirri_persistence::{AppendLogStore, JsonlAppendLog};

    /// A session manager over a temp workspace carrying a server config whose
    /// `[loop_control]` tightens the capacity caps — the composition-root
    /// wiring (`assemble` sets the same shared snapshot) the close-time
    /// snapshot and the mirri-server restore handler read their replay
    /// limits from.
    struct CapacityTestEnv {
        manager: SessionManager,
        _tmp: tempfile::TempDir,
    }

    impl CapacityTestEnv {
        async fn new(max_history_messages: u32, max_deferred_messages: u32) -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let app = AppScope::new(AppScopeConfig::default());
            let workspace = app
                .create_workspace_with_config(
                    WorkspaceId::new("capacity-e2e"),
                    tmp.path().to_path_buf(),
                    WorkspaceScopeConfig {
                        trust_level: WorkspaceTrustLevel::Trusted,
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
            let server_config = std::sync::Arc::new(std::sync::RwLock::new(ServerConfig {
                loop_control: Some(LoopControlConfig {
                    max_history_messages: Some(max_history_messages),
                    max_deferred_messages: Some(max_deferred_messages),
                    ..LoopControlConfig::default()
                }),
                ..ServerConfig::default()
            }));
            let manager = SessionManager::with_persistence_dir(
                workspace,
                tmp.path().join(".mirri").join("v3_sessions"),
                Arc::new(mirri_os::LocalFs::new()),
            )
            .with_server_config(server_config);
            Self { manager, _tmp: tmp }
        }
    }

    fn capacity_metadata() -> SessionMetadata {
        SessionMetadata {
            title: "Capacity E2E".to_string(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            tags: std::collections::HashSet::new(),
            archived: false,
            is_custom_title: false,
            work_dir: None,
            last_prompt: None,
            forked_from: None,
            cache_root_id: None,
            custom: None,
        }
    }

    #[tokio::test]
    async fn should_replay_with_tightened_limits_when_session_resumes_end_to_end() {
        // Given a session run under tightened caps (history 4 / deferred 2 —
        // the caps `TurnFlow` applies via `set_limits` from its strategy
        // every turn): one bulky user message stores, a tool exchange
        // defers two arriving messages and hard-rejects the third at the
        // deferred cap, the flush lands truncated at the remaining history
        // capacity, and one more bulky message is hard-rejected at the
        // history cap — the wire carries every op regardless (construction
        // mirrors the capacity fixtures above, previous batch's Task 5)
        let max_history_messages = 4;
        let max_deferred_messages = 2;
        let mut mem = ContextMemory::new();
        mem.set_limits(max_history_messages, max_deferred_messages);
        let mut ops: Vec<WireOp> = Vec::new();
        record_message(
            &mut mem,
            &mut ops,
            user_msg(&format!("question 0 {}", "x".repeat(6_000))),
        );
        for event in [
            LoopEvent::StepBegin {
                step_id: "s1".to_string(),
                entry_seq: None,
            },
            LoopEvent::ToolCall {
                step_id: "s1".to_string(),
                tool_call_id: "call-1".to_string(),
                tool_name: "read".to_string(),
                args: serde_json::json!({"path": "/tmp/f"}),
            },
        ] {
            record_loop_event(&mut mem, &mut ops, event);
        }
        // Three messages arrive while the exchange is open: the first two
        // buffer, the third is hard-rejected at the deferred cap.
        for i in 0..=(max_deferred_messages + 1) {
            record_message(&mut mem, &mut ops, user_msg(&format!("deferred {i}")));
        }
        for event in [
            LoopEvent::ToolResult {
                tool_call_id: "call-1".to_string(),
                output: ToolOutput::Text("result 1".to_string()),
                is_error: false,
                note: None,
                entry_seq: None,
            },
            LoopEvent::ContentPart {
                step_id: "s1".to_string(),
                text: "answer 1 ".to_string().repeat(300),
            },
            LoopEvent::StepEnd {
                step_id: "s1".to_string(),
                input_tokens: 2_000,
                output_tokens: 100,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
        ] {
            record_loop_event(&mut mem, &mut ops, event);
        }
        // One more message beyond the history cap: hard-rejected, never
        // stored, while the wire still carries it.
        record_message(
            &mut mem,
            &mut ops,
            user_msg(&format!("question 1 {}", "x".repeat(6_000))),
        );

        // Sanity: the live capped memory is exactly four slots and neither
        // overflow message is in it.
        assert_eq!(mem.history().len(), max_history_messages);
        assert!(
            !mem.history()
                .iter()
                .any(|m| m.message.text_content().starts_with("question 1"))
        );
        assert!(
            !mem.history()
                .iter()
                .any(|m| m.message.text_content() == "deferred 2")
        );

        // And a compaction applied directly, keeping the last live slot
        // verbatim as the retention tail (the ~6 000-token head against
        // the 2 000-token head budget elides the middle, so the rewritten
        // history genuinely shrinks)
        let tail_start = mem.history().len() - 1;
        let tokens_before = mem.token_count_with_pending();
        let input = CompactionInput {
            summary: "s".into(),
            context_summary: "SUMMARY".into(),
            compacted_count: tail_start as u32,
            tokens_before,
            dropped_count: 0,
            retention_tail_start: Some(tail_start),
        };
        let (first_replaced, last_replaced) = direct_apply_replaced_range(&mem, tail_start);
        let outcome = mem.apply_compaction(input.clone());
        assert!(outcome.applied, "compaction must apply for the round-trip");
        assert_eq!(outcome.retained_tail_count, 1);
        record_direct_apply(
            &mut ops,
            &input,
            &outcome,
            mem.history().len(),
            mem.token_count(),
            first_replaced,
            last_replaced,
        );

        // And the ops persisted as a real session's wire log under a
        // manager wired to the same tightened `[loop_control]` snapshot
        // the live turns ran with
        let env =
            CapacityTestEnv::new(max_history_messages as u32, max_deferred_messages as u32).await;
        let session_id = SessionId::new("capacity-e2e");
        env.manager
            .create_session_with_id(
                session_id.clone(),
                capacity_metadata(),
                SessionScopeConfig::default(),
            )
            .await
            .unwrap();
        let wire_log_path = env.manager.persistence().wire_log_path(&session_id);
        let mut log = JsonlAppendLog::create(&wire_log_path).await.unwrap();
        // The model record a real embedded create writes (spec 11 R4): the
        // resume fallback chain resolves the model from the wire log's
        // record, else the base config — this fixture passes a bare default
        // base config, so the log must carry the record.
        log.append(&WireOp::ConfigUpdate {
            model_alias: Some("capacity-e2e-model".to_string()),
            profile_name: None,
            thinking_effort: None,
            system_prompt: None,
            disallowed_tools: None,
        })
        .await
        .unwrap();
        for op in &ops {
            log.append(op).await.unwrap();
        }
        drop(log);

        // When the session closes (the close-time snapshot folds under the
        // same manager-carried caps, not the default constants) and
        // resumes through the manager-level path with the limits the
        // mirri-server restore handler derives from the same snapshot
        env.manager
            .close_session(&session_id, CloseReason::Exit)
            .await
            .unwrap();
        assert!(
            env.manager
                .persistence()
                .snapshot_path(&session_id)
                .exists(),
            "the close-time snapshot must be written"
        );
        let replay_limits = env
            .manager
            .replay_limits()
            .expect("a wired config snapshot derives limits");
        assert_eq!(replay_limits.max_history_messages, max_history_messages);
        assert_eq!(replay_limits.max_deferred_messages, max_deferred_messages);
        let resumed = env
            .manager
            .resume_session_with_limits(
                &session_id,
                AgentScopeConfig::default(),
                Some(replay_limits),
            )
            .await
            .unwrap();

        // Then the rebuilt memory is the live post-compaction memory,
        // message for message — the tightened caps reject the same appends
        // the live side rejected (the history-cap overflow message and the
        // deferred-cap overflow message never leak into the rebuilt
        // history), and the close-time snapshot's fold agrees with the
        // resume's.
        let agent = resumed
            .get_agent(&AgentId::new("main"))
            .await
            .expect("resumed session must recreate the main agent");
        let memory = agent.context_memory();
        let rebuilt = memory.lock().unwrap_or_else(|e| e.into_inner());
        assert_histories_match(&mem, &rebuilt);
        assert!(
            !rebuilt
                .history()
                .iter()
                .any(|m| m.message.text_content().starts_with("question 1"))
        );
        assert!(
            !rebuilt
                .history()
                .iter()
                .any(|m| m.message.text_content() == "deferred 2")
        );
    }
}
