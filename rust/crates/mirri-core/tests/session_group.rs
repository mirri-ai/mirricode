// layer: T1
// Domain group binary for the session domain — consolidated from per-file
// test binaries per 16-test-execution-strategy.md §4.4 (pure file moves).

mod compaction_ {
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
        todo_list: None,
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
        todo_list: None,
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
        todo_list: None,
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
        todo_list: None,
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
        todo_list: None,
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
        todo_list: None,
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
        todo_list: None,
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
        todo_list: None,
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
        todo_list: None,
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
        todo_list: None,
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
        todo_list: None,
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
        todo_list: None,
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
        todo_list: None,
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
        todo_list: None,
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
        todo_list: None,
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
        todo_list: None,
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
        "  [pending] Task A".to_string(),
        "  [in_progress] Task B".to_string(),
        "  [done] Task C".to_string(),
    ];
    let result = mirri_core::compaction::round::post_process_summary("My summary.", Some(&todos));
    assert!(result.contains("My summary."));
    assert!(result.contains("## TODO List"));
    assert!(result.contains("  [pending] Task A"));
    assert!(result.contains("  [in_progress] Task B"));
    assert!(result.contains("  [done] Task C"));
}

// ---------------------------------------------------------------------------
// compaction summary todo appendix tests (US3.1)
// ---------------------------------------------------------------------------

/// The exact TS shape `fullCompactionService.postProcessSummary` appends:
/// `renderTodoList(todos, '## TODO List')` — the trimmed summary, a blank
/// line, the heading, then one `  [status] title` line per item.
#[test]
fn should_render_the_todo_list_section_in_the_ts_shape() {
    let todos = vec![
        "  [pending] First task".to_string(),
        "  [in_progress] Second task".to_string(),
        "  [done] Third task".to_string(),
    ];
    let result = mirri_core::compaction::round::post_process_summary("My summary.", Some(&todos));
    assert_eq!(
        result,
        "My summary.\n\n## TODO List\n  [pending] First task\n  [in_progress] Second task\n  [done] Third task"
    );
    // The TS output has no bullet prefix — the pre-rendered lines land raw.
    assert!(!result.contains("- ["));
}

#[tokio::test]
async fn should_append_the_todo_list_to_the_compaction_summary_when_present() {
    // Given a mock-LLM round over a compactable memory and a session todo
    // list with items at every status
    let llm = MockLlm::new(vec![MockResponse::end_turn("This is the summary.")]);
    let mut memory = make_memory_with_many_messages();
    let cancel = CancelToken::new();
    let tools = ToolManager::new();
    let todos = mirri_core::TodoList::new();
    todos.set(vec![
        mirri_core::TodoItem {
            title: "Write the landing page".to_string(),
            status: mirri_core::TodoStatus::Pending,
        },
        mirri_core::TodoItem {
            title: "Add tests".to_string(),
            status: mirri_core::TodoStatus::InProgress,
        },
        mirri_core::TodoItem {
            title: "Ship it".to_string(),
            status: mirri_core::TodoStatus::Done,
        },
    ]);
    todos.push_checkpoint();

    // When a compaction round runs with the list wired in
    let result = compaction_round(CompactionRoundInput {
        llm: &llm,
        memory: &mut memory,
        cancel: &cancel,
        tools: &tools,
        custom_instruction: None,
        max_output_size: None,
        todo_list: Some(&todos),
    })
    .await
    .expect("compaction should succeed");

    // Then the summary carries the `## TODO List` section with every item
    // in the TS renderTodoList line shape (`  [status] title`).
    assert!(result.summary.contains("This is the summary."));
    assert!(result.summary.contains("## TODO List"));
    assert!(
        result
            .summary
            .contains("  [pending] Write the landing page")
    );
    assert!(result.summary.contains("  [in_progress] Add tests"));
    assert!(result.summary.contains("  [done] Ship it"));
    assert!(result.context_summary.contains("## TODO List"));
    // And the applied round cleared the undo checkpoint stack.
    assert_eq!(
        todos.pop_checkpoint(1),
        0,
        "the applied round must clear the todo checkpoint stack"
    );
}

#[tokio::test]
async fn should_not_append_an_empty_todo_section_when_the_list_is_empty() {
    // Given a mock-LLM round and an EMPTY todo list
    let llm = MockLlm::new(vec![MockResponse::end_turn("This is the summary.")]);
    let mut memory = make_memory_with_many_messages();
    let cancel = CancelToken::new();
    let tools = ToolManager::new();
    let todos = mirri_core::TodoList::new();

    // When a compaction round runs with the empty list wired in
    let result = compaction_round(CompactionRoundInput {
        llm: &llm,
        memory: &mut memory,
        cancel: &cancel,
        tools: &tools,
        custom_instruction: None,
        max_output_size: None,
        todo_list: Some(&todos),
    })
    .await
    .expect("compaction should succeed");

    // Then no `## TODO List` section is appended (TS parity: an empty
    // list returns the summary unchanged).
    assert_eq!(result.summary, "This is the summary.");
    assert!(!result.summary.contains("## TODO List"));
}

#[tokio::test]
async fn should_not_append_an_empty_todo_section_when_no_list_is_provided() {
    // Given a mock-LLM round with NO todo list (a standalone/test round)
    let llm = MockLlm::new(vec![MockResponse::end_turn("This is the summary.")]);
    let mut memory = make_memory_with_many_messages();
    let cancel = CancelToken::new();
    let tools = ToolManager::new();

    // When a compaction round runs without a list
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

    // Then the summary is unchanged — no `## TODO List` section.
    assert_eq!(result.summary, "This is the summary.");
    assert!(!result.summary.contains("## TODO List"));
}

/// US3.1 through the LIVE orchestrator path: the FullCompaction round
/// (`begin` → `block`) must append the todo list to its summary too — the
/// turn flow's auto/manual compaction goes through this seam, not
/// `compaction_round`.
#[tokio::test]
async fn should_append_the_todo_list_on_the_live_compaction_path_when_present() {
    // Given a full-compaction orchestrator with a non-empty todo list
    let strategy = CompactionStrategy::default();
    let mut fc = FullCompaction::new(strategy);
    let todos = mirri_core::TodoList::new();
    todos.set(vec![mirri_core::TodoItem {
        title: "Review the spec".to_string(),
        status: mirri_core::TodoStatus::InProgress,
    }]);
    fc.set_todo_list(Some(Arc::new(todos)));

    let mut mem = make_memory_with_many_messages();
    let llm: Arc<dyn Llm> = Arc::new(MockLlm::new(vec![MockResponse::end_turn(
        "Orchestrator summary.",
    )]));
    let cancel = CancelToken::new();

    // When the begin → block cycle completes
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

    // Then the applied summary carries the todo section.
    assert!(result.applied, "the live round must apply");
    assert!(result.summary.contains("## TODO List"));
    assert!(result.summary.contains("[in_progress] Review the spec"));
}

/// Task 4 follow-up: an applied LIVE compaction clears the todo undo
/// checkpoint stack (the memory-side mirror of the reducer's
/// `context.apply_compaction` rule) — pre-compaction anchors are no longer
/// reachable by undo. A refused round (non-shrinking summary) must NOT
/// clear: memory stays byte-identical, so its anchors stay valid.
#[tokio::test]
async fn should_clear_the_live_todo_checkpoints_when_a_compaction_applies() {
    // Given an orchestrator with a todo list holding one undo checkpoint
    let strategy = CompactionStrategy::default();
    let mut fc = FullCompaction::new(strategy);
    let todos = Arc::new(mirri_core::TodoList::new());
    todos.set(vec![mirri_core::TodoItem {
        title: "A task".to_string(),
        status: mirri_core::TodoStatus::Pending,
    }]);
    todos.push_checkpoint();
    fc.set_todo_list(Some(Arc::clone(&todos)));

    let mut mem = make_memory_with_many_messages();
    let llm: Arc<dyn Llm> = Arc::new(MockLlm::new(vec![MockResponse::end_turn(
        "Summary that shrinks.",
    )]));
    let cancel = CancelToken::new();

    // When the begin → block cycle applies
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

    // Then the round applied and the checkpoint stack is empty — a pop
    // restores nothing while the live list itself is untouched.
    assert!(result.applied, "the live round must apply");
    assert_eq!(
        todos.pop_checkpoint(1),
        0,
        "the checkpoint stack must be cleared by the applied compaction"
    );
    assert_eq!(
        todos.get(),
        vec![mirri_core::TodoItem {
            title: "A task".to_string(),
            status: mirri_core::TodoStatus::Pending,
        }],
        "the live list itself must survive the compaction"
    );
}

#[tokio::test]
async fn should_keep_the_live_todo_checkpoints_when_a_compaction_is_refused() {
    // Given an orchestrator with a todo list holding one undo checkpoint
    // and a summary so large it fails the shrink check (变小校验)
    let strategy = CompactionStrategy::default();
    let mut fc = FullCompaction::new(strategy);
    let todos = Arc::new(mirri_core::TodoList::new());
    todos.set(vec![mirri_core::TodoItem {
        title: "A task".to_string(),
        status: mirri_core::TodoStatus::Pending,
    }]);
    todos.push_checkpoint();
    fc.set_todo_list(Some(Arc::clone(&todos)));

    let mut mem = make_memory_with_many_messages();
    // The history is ~160k tokens; a summary of ~1M chars (~250k tokens)
    // cannot pass the shrink check, so the round is refused.
    let llm: Arc<dyn Llm> = Arc::new(MockLlm::new(vec![MockResponse::end_turn(
        &"y".repeat(1_000_000),
    )]));
    let cancel = CancelToken::new();

    // When the begin → block cycle is refused
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

    // Then nothing was applied and the checkpoint stack is intact.
    assert!(!result.applied, "the oversized summary must be refused");
    assert_eq!(
        todos.pop_checkpoint(1),
        1,
        "the checkpoint stack must survive a refused compaction"
    );
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
        todo_list: None,
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
            None,
            None,
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
            None,
            None,
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
            None,
            None,
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
            None,
            None,
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
            last_used_at: None,
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

// ---------------------------------------------------------------------------
// Request-overhead baseline (system prompt + tool table) across
// compaction / undo
// ---------------------------------------------------------------------------

/// Build a memory whose history holds 3 user/assistant pairs, then have the
/// provider report one StepEnd usage total of exactly `usage_total` covering
/// the whole history — the way a live session's first settled step does.
///
/// The returned memory has `token_count == usage_total` (four-bucket sum) and
/// `request_overhead_tokens == usage_total − estimate(history)`, i.e. the
/// usage's fixed, non-history part (system prompt + tool table).
fn memory_with_reported_usage(usage_total: u64) -> ContextMemory {
    let mut mem = memory_with(3);
    mem.append_loop_event(LoopEvent::StepBegin {
        step_id: "step-usage".to_string(),
        entry_seq: None,
    });
    mem.append_loop_event(LoopEvent::ContentPart {
        step_id: "step-usage".to_string(),
        text: "answer".to_string(),
    });
    mem.append_loop_event(LoopEvent::StepEnd {
        step_id: "step-usage".to_string(),
        input_tokens: usage_total,
        output_tokens: 0,
        cache_read_tokens: 0,
        cache_creation_tokens: 0,
    });
    mem
}

#[test]
fn should_preserve_request_overhead_baseline_when_compaction_applied() {
    // Given a session whose provider-reported usage total carries a large
    // fixed overhead (system prompt + tool table): the usage total exceeds
    // the history estimate by a wide margin, so the compaction trigger keeps
    // seeing request-true pressure instead of a history-only undercount.
    let mem = memory_with_reported_usage(100_000);

    // The observed baseline is the usage total minus the estimated tokens
    // of the history that request covered (the settled step is the newest
    // message, so coverage spans the whole history).
    let history_estimate = mirri_core::context_memory::token_estimator::estimate_messages_tokens(
        mem.history(),
    );
    assert_eq!(mem.request_overhead_tokens(), 100_000 - history_estimate);

    // When compaction rewrites the history (shrink check passes: the
    // request-true `tokens_before` far exceeds the history-only rewrite)
    let mut mem = mem;
    let outcome = mem.apply_compaction(CompactionInput {
        summary: "s".into(),
        context_summary: "SUMMARY".into(),
        compacted_count: 4,
        tokens_before: mem.token_count_with_pending(),
        dropped_count: 0,
        retention_tail_start: Some(4), // keep the last pair verbatim
    });

    // The exact post-compaction measure, derived the same way the apply
    // does: estimate(new history) + the observed baseline — pinned here so
    // the assertion below catches BOTH a dropped and an inflated baseline.
    let tokens_after = mirri_core::context_memory::token_estimator::estimate_messages_tokens(
        mem.history(),
    );

    // Then the apply succeeds and the post-compaction token measure keeps
    // the fixed-overhead baseline: the rewrite shrinks the history part,
    // but the system prompt + tool table still ride every request the
    // engine is about to make, so the trigger line is not silently pushed
    // out (bug: a ~270K-token overhead used to vanish, leaving the engine
    // believing it sat at 1/3 while the real request neared the window).
    assert!(outcome.applied);
    assert!(mem.request_overhead_tokens() > 0);
    assert_eq!(
        mem.token_count_with_pending(),
        tokens_after + mem.request_overhead_tokens(),
        "the post-compaction measure must be exactly estimate(new history) + baseline, got {} (estimate {}, baseline {})",
        mem.token_count_with_pending(),
        tokens_after,
        mem.request_overhead_tokens()
    );
}

#[test]
fn should_preserve_request_overhead_baseline_when_messages_undone() {
    // Given a session whose provider-reported usage total carries a fixed
    // overhead, then a user turn is appended beyond the usage-covered
    // prefix, then the memory truncates back past the coverage point via
    // undo_count — forcing the estimate-reset path in undo.
    let mut mem = memory_with_reported_usage(100_000);
    let baseline = mem.request_overhead_tokens();
    assert!(baseline > 0);
    let reported_total = mem.token_count();

    // A user turn beyond the covered prefix. undo_count(1) would cut at
    // exactly the coverage boundary (covered == 7, cut == 7) and SKIP the
    // reset branch — the second anchor below reaches back into the covered
    // prefix, so the cut lands at 5 and
    // `token_count_covered_message_count > history.len()` actually fires.
    mem.append_message(user_msg("question after usage"));
    mem.undo_count(2).unwrap();

    // Then the re-derived count is EXACTLY the remaining history's
    // estimate plus the observed fixed-overhead baseline: system prompt
    // + tool table are still part of every request, so the undo must not
    // silently drop them from the pressure measure — nor leave the stale
    // provider-reported total in place (the equality pins both: a stale
    // total would still read `reported_total`, a dropped baseline would
    // read estimate-only).
    let remaining_estimate = mirri_core::context_memory::token_estimator::estimate_messages_tokens(
        mem.history(),
    );
    assert!(
        remaining_estimate > 0,
        "the post-undo history must carry non-trivial estimated tokens"
    );
    assert_eq!(
        mem.token_count(),
        remaining_estimate + baseline,
        "token_count after the undo reset must be exactly estimate(remaining history) + baseline"
    );
    assert_ne!(
        mem.token_count(),
        reported_total,
        "the undo must have re-derived the count, not kept the stale reported total"
    );
    assert_eq!(mem.request_overhead_tokens(), baseline);
}

}

mod session_events_ {
//! Integration tests for session events + gates (Task 33).
//!
//! Covers: [`EventBus`] fan-out / lag / close semantics, [`SessionEvent`]
//! serde (type tag + unknown-variant fallback), [`ApprovalGate`] and
//! [`QuestionGate`] request/response/drain flows, [`SessionShutdown`], and
//! [`SessionManager`] integration (fresh gates per session, events published
//! for resume/fork/close, drain-on-close).
//!
//! Tests use the Given-When-Then naming convention.

use std::sync::Arc;
use std::time::Duration;

use mirri_core::{
    AgentLifecycle, AgentScopeConfig, AppScope, AppScopeConfig, ApprovalDecision, ApprovalGate,
    BackgroundTaskInfo, CloseReason, CompactionResult, CompactionSource, EventBus, QuestionGate,
    SessionActivityState, SessionArtifacts, SessionEvent, SessionId, SessionManager,
    SessionMetadata, SessionPendingInteraction, SessionScopeConfig, SessionShutdown,
    SessionTurnOutcome, SuspendCause, TaskKind, TurnStopReason, WorkspaceId, WorkspaceScopeConfig,
    event_projection, uuid_from_id,
};
use tokio::sync::broadcast::{self, error::RecvError, error::TryRecvError};
use uuid::Uuid;

// ===========================================================================
// Helpers
// ===========================================================================

struct TestEnv {
    manager: SessionManager,
    _tmp: tempfile::TempDir,
}

impl TestEnv {
    async fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let app = AppScope::new(AppScopeConfig::default());
        let workspace = app
            .create_workspace_with_config(
                WorkspaceId::new("events-test-ws"),
                tmp.path().to_path_buf(),
                WorkspaceScopeConfig::default(),
            )
            .await
            .unwrap();
        let manager = SessionManager::with_persistence_dir(
            workspace,
            tmp.path().join(".mirri").join("sessions"),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        Self { manager, _tmp: tmp }
    }
}

fn make_metadata(title: &str) -> SessionMetadata {
    SessionMetadata {
        title: title.to_string(),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        tags: Default::default(),
        archived: false,
        is_custom_title: false,
        work_dir: None,
        last_prompt: None,
        forked_from: None,
        cache_root_id: None,
        custom: None,
        last_used_at: None,
    }
}

/// Poll a condition until it becomes true or a deadline passes.
async fn wait_until(label: &str, mut condition: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("condition never became true: {label}");
}

/// Receive events until one matches the predicate; panics on timeout/close.
async fn recv_matching(
    receiver: &mut broadcast::Receiver<SessionEvent>,
    label: &str,
    mut predicate: impl FnMut(&SessionEvent) -> bool,
) -> SessionEvent {
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match receiver.recv().await {
                Ok(event) => {
                    if predicate(&event) {
                        return event;
                    }
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => {
                    panic!("event channel closed while waiting for {label}")
                }
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for event: {label}"))
}

// ===========================================================================
// EventBus — fan-out
// ===========================================================================

#[tokio::test]
async fn should_all_subscribers_receive_event_when_published_on_event_bus() {
    // Given an event bus with two subscribers
    let bus = Arc::new(EventBus::new(16));
    let mut receiver_a = bus.subscribe();
    let mut receiver_b = bus.subscribe();
    let session_id = Uuid::new_v4();

    // When publishing a SessionClosed event
    bus.publish(SessionEvent::SessionClosed { session_id });

    // Then both subscribers receive it
    for receiver in [&mut receiver_a, &mut receiver_b] {
        match receiver.recv().await.unwrap() {
            SessionEvent::SessionClosed {
                session_id: received_id,
            } => assert_eq!(received_id, session_id),
            other => panic!("expected SessionClosed, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn should_only_receive_future_events_when_subscribing_after_publish() {
    // Given an event bus that already published an event
    let bus = Arc::new(EventBus::new(16));
    bus.publish(SessionEvent::SessionResumed {
        session_id: Uuid::new_v4(),
    });

    // When subscribing afterwards
    let mut receiver = bus.subscribe();

    // Then the old event is not replayed
    assert!(
        matches!(receiver.try_recv(), Err(TryRecvError::Empty)),
        "new subscribers must not see events published before subscribing"
    );

    // And future events are received
    let session_id = Uuid::new_v4();
    bus.publish(SessionEvent::SessionClosed { session_id });
    match receiver.recv().await.unwrap() {
        SessionEvent::SessionClosed {
            session_id: received_id,
        } => assert_eq!(received_id, session_id),
        other => panic!("expected SessionClosed, got {other:?}"),
    }
}

#[tokio::test]
async fn should_return_lagged_error_when_events_exceed_buffer_capacity() {
    // Given a small event bus and a subscriber that does not keep up
    let bus = Arc::new(EventBus::new(2));
    let mut receiver = bus.subscribe();

    // When publishing more events than the buffer can hold
    for _ in 0..5 {
        bus.publish(SessionEvent::SessionClosed {
            session_id: Uuid::new_v4(),
        });
    }

    // Then the subscriber observes a Lagged error (missed events are dropped)
    match receiver.recv().await {
        Err(RecvError::Lagged(skipped)) => assert!(
            skipped >= 1,
            "expected at least one skipped event, got {skipped}"
        ),
        other => panic!("expected Lagged error, got {other:?}"),
    }
}

#[tokio::test]
async fn should_return_closed_error_when_all_event_bus_arcs_dropped() {
    // Given a receiver holding the only subscriber
    let bus = Arc::new(EventBus::new(8));
    let mut receiver = bus.subscribe();

    // When the last Arc<EventBus> is released
    drop(bus);

    // Then the receiver observes the channel closing
    assert!(matches!(receiver.recv().await, Err(RecvError::Closed)));
}

// ===========================================================================
// SessionEvent — serialization
// ===========================================================================

#[tokio::test]
async fn should_have_type_tag_identifying_variant_when_session_event_serialized() {
    // Given a tool approval request event
    let agent_id = Uuid::new_v4();
    let request_id = Uuid::new_v4();
    let event = SessionEvent::ToolApprovalRequested {
        agent_id,
        tool_name: "bash".to_string(),
        request_id,
    };

    // When serializing
    let json = serde_json::to_value(event).unwrap();

    // Then the type tag and fields are present
    assert_eq!(json["type"], "tool_approval_requested");
    assert_eq!(json["tool_name"], "bash");
    assert_eq!(json["agent_id"], agent_id.to_string());
    assert_eq!(json["request_id"], request_id.to_string());
}

#[tokio::test]
async fn should_deserialize_to_unknown_variant_when_wire_type_unrecognized() {
    // When deserializing JSON with an unrecognised type tag
    let event: SessionEvent =
        serde_json::from_str(r#"{"type":"some_future_event","payload":42}"#).unwrap();

    // Then the Unknown fallback variant is produced
    assert!(matches!(event, SessionEvent::Unknown));
}

#[tokio::test]
async fn should_preserve_status_when_agent_status_updated_event_roundtripped() {
    // Given an AgentStatusUpdated event carrying the structured status
    // snapshot (phase, model, context, usage)
    use mirri_core::session::agent_status::AgentPhase;
    use mirri_core::usage::{TokenUsage, UsageStatus};
    use std::collections::HashMap;

    let tokens = TokenUsage {
        input_tokens: 120,
        output_tokens: 30,
        cache_read_tokens: 5,
        cache_creation_tokens: 1,
    };
    let usage = UsageStatus {
        by_model: HashMap::from([("kimi-k2".to_string(), tokens.clone())]),
        current_turn: tokens.clone(),
        total: tokens,
        first_call: None,
    };
    let event = SessionEvent::AgentStatusUpdated {
        agent_id: "main".to_string(),
        phase: AgentPhase::Running,
        model: Some("kimi-k2".to_string()),
        thinking_effort: Some(mirri_core::ThinkingEffort::High),
        context_tokens: Some(120_000),
        max_context_tokens: Some(200_000),
        plan_mode: Some(false),
        swarm_mode: None,
        usage: Some(usage.clone()),
    };

    // When serializing and deserializing
    let json = serde_json::to_string(&event).unwrap();
    let roundtrip: SessionEvent = serde_json::from_str(&json).unwrap();

    // Then the tagged variant and its structured status survive the
    // roundtrip, and the absent optional field is entirely absent from
    // the serialised form (never a null key)
    match roundtrip {
        SessionEvent::AgentStatusUpdated {
            agent_id,
            phase,
            model,
            thinking_effort,
            context_tokens,
            max_context_tokens,
            plan_mode,
            swarm_mode,
            usage: received_usage,
        } => {
            assert_eq!(agent_id, "main");
            assert_eq!(phase, AgentPhase::Running);
            assert_eq!(model.as_deref(), Some("kimi-k2"));
            assert_eq!(thinking_effort, Some(mirri_core::ThinkingEffort::High));
            assert_eq!(context_tokens, Some(120_000));
            assert_eq!(max_context_tokens, Some(200_000));
            assert_eq!(plan_mode, Some(false));
            assert_eq!(swarm_mode, None);
            assert_eq!(received_usage, Some(usage));
        }
        other => panic!("expected AgentStatusUpdated, got {other:?}"),
    }
    assert!(
        !json.contains("swarm_mode") && !json.contains("swarmMode"),
        "an absent optional field must not be serialized at all: {json}"
    );
}

#[test]
fn should_roundtrip_a_session_work_changed_event_through_its_serde_form() {
    // Given a SessionWorkChanged event with a completed-turn state
    let event = SessionEvent::SessionWorkChanged {
        state: SessionActivityState {
            busy: true,
            main_turn_active: false,
            pending_interaction: SessionPendingInteraction::Question,
            last_turn_reason: Some(SessionTurnOutcome::Cancelled),
        },
    };

    // When serializing and deserializing the event itself (the ws relay
    // serialises SessionEvent directly, without going through the projection)
    let json = serde_json::to_value(&event).expect("event serialises");
    let roundtrip: SessionEvent = serde_json::from_value(json.clone()).unwrap();

    // Then the tagged variant and its snake_case activity state survive
    // (the enum's own serde shape uses snake_case, distinct from the
    // projection payload which mirrors it key-for-key)
    match roundtrip {
        SessionEvent::SessionWorkChanged { state } => {
            assert!(state.busy);
            assert!(!state.main_turn_active);
            assert_eq!(
                state.pending_interaction,
                SessionPendingInteraction::Question
            );
            assert_eq!(state.last_turn_reason, Some(SessionTurnOutcome::Cancelled));
        }
        other => panic!("expected SessionWorkChanged, got {other:?}"),
    }
    assert_eq!(json["type"], "session_work_changed");
    assert_eq!(json["state"]["busy"], serde_json::json!(true));
    assert_eq!(
        json["state"]["pending_interaction"],
        serde_json::json!("question")
    );
    assert_eq!(
        json["state"]["last_turn_reason"],
        serde_json::json!("cancelled")
    );
}

/// Given a SessionWorkChanged event whose state predates any completed
/// turn, When it is projected, Then the `last_turn_reason` key stays absent
/// from the payload (the projection strips it — the protocol says the key
/// is missing before any turn, so it never appears as a null placeholder).
#[test]
fn should_omit_last_turn_reason_from_the_payload_when_projecting_a_pre_turn_state() {
    // Given a SessionWorkChanged event with no completed turn yet
    let event = SessionEvent::SessionWorkChanged {
        state: SessionActivityState::default(),
    };

    // When projecting it onto the wire event
    let projected = projected(&event);

    // Then the idle flags are carried and the reason key stays absent
    assert_eq!(projected.method, "event.session.work_changed");
    assert_eq!(projected.payload["busy"], serde_json::json!(false));
    assert_eq!(
        projected.payload["main_turn_active"],
        serde_json::json!(false)
    );
    assert_eq!(
        projected.payload["pending_interaction"],
        serde_json::json!("none")
    );
    assert!(projected.payload.get("last_turn_reason").is_none());
}

// ===========================================================================
// ApprovalGate — auto-approve
// ===========================================================================

#[tokio::test]
async fn should_approve_immediately_when_gate_in_auto_approve_mode() {
    // Given a gate in auto-approve (headless) mode
    let gate = ApprovalGate::new(true);

    // When requesting approval
    let decision = gate.request("bash", "rm -rf /tmp/x").await;

    // Then the request is approved immediately without registering anything
    assert_eq!(decision, ApprovalDecision::Approved);
    assert_eq!(gate.pending_count(), 0);
}

// ===========================================================================
// ApprovalGate — request / resolve
// ===========================================================================

#[tokio::test]
async fn should_block_until_resolved_with_approved_when_request_made() {
    // Given a gate wired to an event bus (the bus carries the request_id)
    let bus = Arc::new(EventBus::new(16));
    let gate = Arc::new(ApprovalGate::with_event_bus(false, Arc::clone(&bus)));
    let mut receiver = bus.subscribe();

    let request_gate = Arc::clone(&gate);
    let handle = tokio::spawn(async move { request_gate.request("bash", "ls -la").await });

    // When the request is published and then resolved as approved
    let event = recv_matching(&mut receiver, "ToolApprovalRequested", |e| {
        matches!(e, SessionEvent::ToolApprovalRequested { .. })
    })
    .await;
    let request_id = match event {
        SessionEvent::ToolApprovalRequested {
            request_id,
            tool_name,
            ..
        } => {
            assert_eq!(tool_name, "bash");
            request_id
        }
        other => panic!("expected ToolApprovalRequested, got {other:?}"),
    };

    // Then the request is pending until resolved
    assert_eq!(gate.pending_count(), 1);
    assert!(gate.resolve(request_id, ApprovalDecision::Approved));
    assert_eq!(gate.pending_count(), 0);

    // And the requester receives the approved decision
    assert_eq!(handle.await.unwrap(), ApprovalDecision::Approved);
}

#[tokio::test]
async fn should_receive_denial_with_reason_when_resolved_as_denied() {
    // Given a pending request
    let bus = Arc::new(EventBus::new(16));
    let gate = Arc::new(ApprovalGate::with_event_bus(false, Arc::clone(&bus)));
    let mut receiver = bus.subscribe();

    let request_gate = Arc::clone(&gate);
    let handle = tokio::spawn(async move { request_gate.request("bash", "ls").await });

    let request_id = match recv_matching(&mut receiver, "ToolApprovalRequested", |e| {
        matches!(e, SessionEvent::ToolApprovalRequested { .. })
    })
    .await
    {
        SessionEvent::ToolApprovalRequested { request_id, .. } => request_id,
        other => panic!("expected ToolApprovalRequested, got {other:?}"),
    };

    // When resolving as denied with a reason
    assert!(gate.resolve(
        request_id,
        ApprovalDecision::Denied("user said no".to_string())
    ));

    // Then the requester receives the denial with its reason
    assert_eq!(
        handle.await.unwrap(),
        ApprovalDecision::Denied("user said no".to_string())
    );
}

#[tokio::test]
async fn should_return_false_when_resolving_unknown_request_id() {
    // Given a gate with no pending requests
    let gate = ApprovalGate::new(false);

    // When resolving a request id that was never registered
    let resolved = gate.resolve(Uuid::new_v4(), ApprovalDecision::Approved);

    // Then resolution reports failure
    assert!(!resolved);
}

#[tokio::test]
async fn should_return_false_on_second_resolve_when_same_request_resolved_twice() {
    // Given a pending request
    let bus = Arc::new(EventBus::new(16));
    let gate = Arc::new(ApprovalGate::with_event_bus(false, Arc::clone(&bus)));
    let mut receiver = bus.subscribe();

    let request_gate = Arc::clone(&gate);
    let handle = tokio::spawn(async move { request_gate.request("bash", "ls").await });

    let request_id = match recv_matching(&mut receiver, "ToolApprovalRequested", |e| {
        matches!(e, SessionEvent::ToolApprovalRequested { .. })
    })
    .await
    {
        SessionEvent::ToolApprovalRequested { request_id, .. } => request_id,
        other => panic!("expected ToolApprovalRequested, got {other:?}"),
    };

    // When resolving the same request twice
    let first = gate.resolve(request_id, ApprovalDecision::Approved);
    let second = gate.resolve(request_id, ApprovalDecision::Approved);

    // Then only the first resolution succeeds
    assert!(first);
    assert!(!second);
    assert_eq!(handle.await.unwrap(), ApprovalDecision::Approved);
}

#[tokio::test]
async fn should_resolve_pending_request_as_timeout_when_gate_drained() {
    // Given a pending request (the request_id is not needed to drain)
    let gate = Arc::new(ApprovalGate::new(false));
    let request_gate = Arc::clone(&gate);
    let handle = tokio::spawn(async move { request_gate.request("bash", "ls").await });
    wait_until("request registered", || gate.pending_count() == 1).await;

    // When draining the gate (session close)
    gate.drain().await;

    // Then the pending request resolves as Timeout and the gate is empty
    assert_eq!(handle.await.unwrap(), ApprovalDecision::Timeout);
    assert_eq!(gate.pending_count(), 0);
}

// ===========================================================================
// ApprovalGate — event bus integration
// ===========================================================================

#[tokio::test]
async fn should_publish_events_with_matching_ids_when_request_and_resolve_through_event_bus() {
    // Given a gate wired to an event bus
    let bus = Arc::new(EventBus::new(16));
    let gate = Arc::new(ApprovalGate::with_event_bus(false, Arc::clone(&bus)));
    let mut receiver = bus.subscribe();
    let agent_id = Uuid::new_v4();

    let request_gate = Arc::clone(&gate);
    let handle =
        tokio::spawn(async move { request_gate.request_for_agent(agent_id, "bash", "ls").await });

    // When the request is published and resolved as approved
    let request_id = match recv_matching(&mut receiver, "ToolApprovalRequested", |e| {
        matches!(e, SessionEvent::ToolApprovalRequested { .. })
    })
    .await
    {
        SessionEvent::ToolApprovalRequested {
            agent_id: event_agent_id,
            tool_name,
            request_id,
        } => {
            assert_eq!(event_agent_id, agent_id);
            assert_eq!(tool_name, "bash");
            request_id
        }
        other => panic!("expected ToolApprovalRequested, got {other:?}"),
    };
    assert!(gate.resolve(request_id, ApprovalDecision::Approved));
    assert_eq!(handle.await.unwrap(), ApprovalDecision::Approved);

    // Then a ToolApprovalResolved event is published for the same request
    match recv_matching(&mut receiver, "ToolApprovalResolved", |e| {
        matches!(e, SessionEvent::ToolApprovalResolved { .. })
    })
    .await
    {
        SessionEvent::ToolApprovalResolved {
            request_id: resolved_id,
            approved,
        } => {
            assert_eq!(resolved_id, request_id);
            assert!(approved);
        }
        other => panic!("expected ToolApprovalResolved, got {other:?}"),
    }
}

#[tokio::test]
async fn should_publish_resolved_event_with_approved_false_when_resolved_as_denied() {
    // Given a pending request
    let bus = Arc::new(EventBus::new(16));
    let gate = Arc::new(ApprovalGate::with_event_bus(false, Arc::clone(&bus)));
    let mut receiver = bus.subscribe();

    let request_gate = Arc::clone(&gate);
    let handle = tokio::spawn(async move { request_gate.request("bash", "ls").await });

    let request_id = match recv_matching(&mut receiver, "ToolApprovalRequested", |e| {
        matches!(e, SessionEvent::ToolApprovalRequested { .. })
    })
    .await
    {
        SessionEvent::ToolApprovalRequested { request_id, .. } => request_id,
        other => panic!("expected ToolApprovalRequested, got {other:?}"),
    };

    // When resolving as denied
    assert!(gate.resolve(request_id, ApprovalDecision::Denied("no".to_string())));
    assert_eq!(
        handle.await.unwrap(),
        ApprovalDecision::Denied("no".to_string())
    );

    // Then the resolved event reports approved: false with the same id
    match recv_matching(&mut receiver, "ToolApprovalResolved", |e| {
        matches!(e, SessionEvent::ToolApprovalResolved { .. })
    })
    .await
    {
        SessionEvent::ToolApprovalResolved {
            request_id: resolved_id,
            approved,
        } => {
            assert_eq!(resolved_id, request_id);
            assert!(!approved);
        }
        other => panic!("expected ToolApprovalResolved, got {other:?}"),
    }
}

// ===========================================================================
// QuestionGate — ask / answer / drain
// ===========================================================================

#[tokio::test]
async fn should_deliver_answer_when_question_asked_and_answered() {
    // Given a question gate wired to an event bus (the bus carries the id)
    let bus = Arc::new(EventBus::new(16));
    let gate = Arc::new(QuestionGate::with_event_bus(Arc::clone(&bus)));
    let mut receiver = bus.subscribe();

    let ask_gate = Arc::clone(&gate);
    let handle = tokio::spawn(async move { ask_gate.ask("what is 2+2?").await });

    // When the question is published and then answered
    let request_id = match recv_matching(&mut receiver, "QuestionAsked", |e| {
        matches!(e, SessionEvent::QuestionAsked { .. })
    })
    .await
    {
        SessionEvent::QuestionAsked {
            questions,
            request_id,
            ..
        } => {
            assert_eq!(questions[0]["question"], "what is 2+2?");
            request_id
        }
        other => panic!("expected QuestionAsked, got {other:?}"),
    };

    assert!(gate.answer(request_id, "4".to_string()));

    // Then the asker receives the answer
    assert_eq!(handle.await.unwrap(), Some("4".to_string()));
    assert_eq!(gate.pending_count(), 0);
}

#[tokio::test]
async fn should_return_false_when_answering_unknown_question_request_id() {
    // Given a question gate with no pending questions
    let gate = QuestionGate::new();

    // When answering a request id that was never registered
    let answered = gate.answer(Uuid::new_v4(), "42".to_string());

    // Then answering reports failure
    assert!(!answered);
}

#[tokio::test]
async fn should_return_none_when_question_gate_drained() {
    // Given a pending question
    let gate = Arc::new(QuestionGate::new());
    let ask_gate = Arc::clone(&gate);
    let handle = tokio::spawn(async move { ask_gate.ask("continue?").await });
    wait_until("question registered", || gate.pending_count() == 1).await;

    // When draining the gate (session close)
    gate.drain().await;

    // Then the asker gets None and the gate is empty
    assert_eq!(handle.await.unwrap(), None);
    assert_eq!(gate.pending_count(), 0);
}

#[tokio::test]
async fn should_publish_events_with_matching_ids_when_question_asked_and_answered_through_event_bus()
 {
    // Given a question gate wired to an event bus
    let bus = Arc::new(EventBus::new(16));
    let gate = Arc::new(QuestionGate::with_event_bus(Arc::clone(&bus)));
    let mut receiver = bus.subscribe();
    let agent_id = Uuid::new_v4();

    let ask_gate = Arc::clone(&gate);
    let handle = tokio::spawn(async move { ask_gate.ask_for_agent(agent_id, "proceed?").await });

    // When the question is published and then answered
    let request_id = match recv_matching(&mut receiver, "QuestionAsked", |e| {
        matches!(e, SessionEvent::QuestionAsked { .. })
    })
    .await
    {
        SessionEvent::QuestionAsked {
            agent_id: event_agent_id,
            questions,
            request_id,
        } => {
            assert_eq!(event_agent_id, agent_id);
            assert_eq!(questions[0]["question"], "proceed?");
            request_id
        }
        other => panic!("expected QuestionAsked, got {other:?}"),
    };
    assert!(gate.answer(request_id, "yes".to_string()));
    assert_eq!(handle.await.unwrap(), Some("yes".to_string()));

    // Then a QuestionAnswered event is published for the same request
    match recv_matching(&mut receiver, "QuestionAnswered", |e| {
        matches!(e, SessionEvent::QuestionAnswered { .. })
    })
    .await
    {
        SessionEvent::QuestionAnswered {
            request_id: answered_id,
            answer,
        } => {
            assert_eq!(answered_id, request_id);
            assert_eq!(answer, "yes");
        }
        other => panic!("expected QuestionAnswered, got {other:?}"),
    }
}

// ===========================================================================
// SessionShutdown — drain both gates
// ===========================================================================

#[tokio::test]
async fn should_timeout_approval_and_return_none_for_question_when_session_shuts_down() {
    // Given a session artifact set with one pending approval and one pending
    // question (the gates share the session's event bus)
    let artifacts = SessionArtifacts::new();

    let approval_request = Arc::clone(&artifacts.approval_gate);
    let approval_handle = tokio::spawn(async move { approval_request.request("bash", "ls").await });
    let question_request = Arc::clone(&artifacts.question_gate);
    let question_handle = tokio::spawn(async move { question_request.ask("continue?").await });
    wait_until("approval registered", || {
        artifacts.approval_gate.pending_count() == 1
    })
    .await;
    wait_until("question registered", || {
        artifacts.question_gate.pending_count() == 1
    })
    .await;

    // When the session shuts down
    let shutdown = SessionShutdown::new(&artifacts);
    shutdown.shutdown().await;

    // Then the approval times out and the question returns None
    assert_eq!(approval_handle.await.unwrap(), ApprovalDecision::Timeout);
    assert_eq!(question_handle.await.unwrap(), None);
    assert_eq!(artifacts.approval_gate.pending_count(), 0);
    assert_eq!(artifacts.question_gate.pending_count(), 0);
}

// ===========================================================================
// SessionManager — per-session artifacts
// ===========================================================================

#[tokio::test]
async fn should_get_distinct_artifacts_when_two_sessions_created() {
    // Given a manager with no sessions
    let env = TestEnv::new().await;

    // When creating two sessions
    let first = env
        .manager
        .create_session(make_metadata("First"), SessionScopeConfig::default())
        .await
        .unwrap();
    let second = env
        .manager
        .create_session(make_metadata("Second"), SessionScopeConfig::default())
        .await
        .unwrap();

    // Then each session exposes its own distinct event bus and gates
    let first_artifacts = env
        .manager
        .session_artifacts(first.id())
        .expect("first session artifacts");
    let second_artifacts = env
        .manager
        .session_artifacts(second.id())
        .expect("second session artifacts");
    assert!(!Arc::ptr_eq(
        &first_artifacts.event_bus,
        &second_artifacts.event_bus
    ));
    assert!(!Arc::ptr_eq(
        &first_artifacts.approval_gate,
        &second_artifacts.approval_gate
    ));
    assert!(!Arc::ptr_eq(
        &first_artifacts.question_gate,
        &second_artifacts.question_gate
    ));

    // And both sessions start with empty (fresh) gates
    assert_eq!(first_artifacts.approval_gate.pending_count(), 0);
    assert_eq!(first_artifacts.question_gate.pending_count(), 0);
    assert_eq!(second_artifacts.approval_gate.pending_count(), 0);
    assert_eq!(second_artifacts.question_gate.pending_count(), 0);
}

#[tokio::test]
async fn should_keep_first_session_request_pending_when_second_session_created() {
    // Given a session with a pending approval request
    let env = TestEnv::new().await;
    let first = env
        .manager
        .create_session(make_metadata("First"), SessionScopeConfig::default())
        .await
        .unwrap();
    let first_artifacts = env
        .manager
        .session_artifacts(first.id())
        .expect("first session artifacts");

    let request_gate = Arc::clone(&first_artifacts.approval_gate);
    let handle = tokio::spawn(async move { request_gate.request("bash", "ls").await });
    wait_until("approval registered", || {
        first_artifacts.approval_gate.pending_count() == 1
    })
    .await;

    // When a second session is created
    let second = env
        .manager
        .create_session(make_metadata("Second"), SessionScopeConfig::default())
        .await
        .unwrap();
    let second_artifacts = env
        .manager
        .session_artifacts(second.id())
        .expect("second session artifacts");

    // Then the first session's request is NOT drained — it stays pending
    // (per-session gates isolate sessions from each other)
    assert_eq!(first_artifacts.approval_gate.pending_count(), 1);
    assert_eq!(second_artifacts.approval_gate.pending_count(), 0);

    // And the requester only resolves once the first session closes, which
    // drains exactly that session's gates
    env.manager
        .close_session(first.id(), CloseReason::Exit)
        .await
        .unwrap();
    assert_eq!(handle.await.unwrap(), ApprovalDecision::Timeout);
    assert_eq!(first_artifacts.approval_gate.pending_count(), 0);
}

#[tokio::test]
async fn should_drain_gates_and_publish_event_when_session_closed() {
    // Given a session with a pending approval request
    let env = TestEnv::new().await;
    let session = env
        .manager
        .create_session(make_metadata("To Close"), SessionScopeConfig::default())
        .await
        .unwrap();
    let session_id = session.id().clone();
    let expected_uuid = uuid_from_id(session_id.as_str());
    let artifacts = env
        .manager
        .session_artifacts(&session_id)
        .expect("session artifacts");

    let request_gate = Arc::clone(&artifacts.approval_gate);
    let handle = tokio::spawn(async move { request_gate.request("bash", "ls").await });
    wait_until("approval registered", || {
        artifacts.approval_gate.pending_count() == 1
    })
    .await;

    let mut receiver = artifacts.event_bus.subscribe();

    // When closing the session
    env.manager
        .close_session(&session_id, CloseReason::Exit)
        .await
        .unwrap();

    // Then the pending request is drained (resolves as Timeout)
    assert_eq!(handle.await.unwrap(), ApprovalDecision::Timeout);
    assert_eq!(artifacts.approval_gate.pending_count(), 0);

    // And a resolution event is published before the SessionClosed event
    match recv_matching(&mut receiver, "ToolApprovalResolved", |e| {
        matches!(e, SessionEvent::ToolApprovalResolved { .. })
    })
    .await
    {
        SessionEvent::ToolApprovalResolved { approved, .. } => assert!(!approved),
        other => panic!("expected ToolApprovalResolved, got {other:?}"),
    }
    match recv_matching(&mut receiver, "SessionClosed", |e| {
        matches!(e, SessionEvent::SessionClosed { .. })
    })
    .await
    {
        SessionEvent::SessionClosed {
            session_id: closed_id,
        } => assert_eq!(closed_id, expected_uuid),
        other => panic!("expected SessionClosed, got {other:?}"),
    }

    // And the closed session's artifacts are no longer registered
    assert!(
        env.manager.session_artifacts(&session_id).is_none(),
        "closed session must not keep artifacts registered"
    );
}

#[tokio::test]
async fn should_install_fresh_artifacts_when_session_resumed() {
    // Given a persisted session (state.json + wire.jsonl on disk)
    let env = TestEnv::new().await;
    let session = env
        .manager
        .create_session(make_metadata("To Resume"), SessionScopeConfig::default())
        .await
        .unwrap();
    let session_id = session.id().clone();
    let expected_uuid = uuid_from_id(session_id.as_str());
    let old_artifacts = env
        .manager
        .session_artifacts(&session_id)
        .expect("pre-close artifacts");

    let wire_log_path = env.manager.persistence().wire_log_path(&session_id);
    tokio::fs::create_dir_all(wire_log_path.parent().unwrap())
        .await
        .unwrap();
    // A minimal resumable log: the metadata envelope plus the model record
    // a real embedded create writes (spec 11 R4 — the resume chain needs a
    // resolvable model: the wire log's record, else the base config).
    tokio::fs::write(
        &wire_log_path,
        concat!(
            "{\"type\":\"metadata\"}\n",
            "{\"type\":\"config.update\",\"modelAlias\":\"resume-events-model\"}\n",
        ),
    )
    .await
    .unwrap();
    env.manager
        .close_session(&session_id, CloseReason::Exit)
        .await
        .unwrap();
    assert!(
        env.manager.session_artifacts(&session_id).is_none(),
        "closed session must not keep artifacts registered"
    );

    // When resuming (the manager installs fresh artifacts; the resumed
    // announcement is the caller's to publish, after wiring its relay)
    env.manager.resume_session(&session_id).await.unwrap();

    // Then fresh artifacts are installed: a new event bus and empty gates
    let resumed_artifacts = env
        .manager
        .session_artifacts(&session_id)
        .expect("resumed session artifacts");
    assert!(
        !Arc::ptr_eq(&old_artifacts.event_bus, &resumed_artifacts.event_bus),
        "resumed session must get a fresh event bus"
    );
    assert_eq!(resumed_artifacts.approval_gate.pending_count(), 0);
    assert_eq!(resumed_artifacts.question_gate.pending_count(), 0);

    // And the fresh bus is functional for the resumed session
    let mut receiver = resumed_artifacts.event_bus.subscribe();
    resumed_artifacts
        .event_bus
        .publish(SessionEvent::SessionClosed {
            session_id: uuid_from_id(session_id.as_str()),
        });
    match recv_matching(&mut receiver, "SessionClosed", |e| {
        matches!(e, SessionEvent::SessionClosed { .. })
    })
    .await
    {
        SessionEvent::SessionClosed {
            session_id: closed_id,
        } => assert_eq!(closed_id, expected_uuid),
        other => panic!("expected SessionClosed, got {other:?}"),
    }
}

#[tokio::test]
async fn should_publish_session_forked_event_on_source_bus_when_session_forked() {
    // Given a source session with an agent
    let env = TestEnv::new().await;
    let source_session = env
        .manager
        .create_session(make_metadata("Source"), SessionScopeConfig::default())
        .await
        .unwrap();
    let source_uuid = uuid_from_id(source_session.id().as_str());
    let source_artifacts = env
        .manager
        .session_artifacts(source_session.id())
        .expect("source session artifacts");

    let lifecycle = AgentLifecycle::new(Arc::clone(&source_session));
    let source_agent = lifecycle
        .create_agent(AgentScopeConfig::default())
        .await
        .unwrap();

    // When forking the session (subscribing to the source session's bus
    // first, since the forked session's bus has no subscribers yet)
    let mut receiver = source_artifacts.event_bus.subscribe();
    let forked_session = env
        .manager
        .fork_session(&source_session, source_agent.id(), make_metadata("Forked"))
        .await
        .unwrap();
    let forked_uuid = uuid_from_id(forked_session.id().as_str());

    // Then a SessionForked event is published on the source session's bus
    // with both session UUIDs
    match recv_matching(&mut receiver, "SessionForked", |e| {
        matches!(e, SessionEvent::SessionForked { .. })
    })
    .await
    {
        SessionEvent::SessionForked {
            source_session_id,
            new_session_id,
        } => {
            assert_eq!(source_session_id, source_uuid);
            assert_eq!(new_session_id, forked_uuid);
        }
        other => panic!("expected SessionForked, got {other:?}"),
    }

    // And the forked session owns its own distinct artifacts
    let forked_artifacts = env
        .manager
        .session_artifacts(forked_session.id())
        .expect("forked session artifacts");
    assert!(
        !Arc::ptr_eq(&source_artifacts.event_bus, &forked_artifacts.event_bus),
        "forked session must get its own event bus"
    );
}

#[tokio::test]
async fn should_announce_goal_updated_on_fork_bus_when_fork_main_agent_creates_goal() {
    // Given a source session with an agent (whose config carries the SOURCE
    // session's event bus — the fork pipeline's inheritance seed)
    let env = TestEnv::new().await;
    let source_session = env
        .manager
        .create_session(make_metadata("Source"), SessionScopeConfig::default())
        .await
        .unwrap();
    let source_artifacts = env
        .manager
        .session_artifacts(source_session.id())
        .expect("source session artifacts");
    let lifecycle = AgentLifecycle::new(Arc::clone(&source_session));
    let source_agent = lifecycle
        .create_agent(AgentScopeConfig {
            event_bus: Some(Arc::clone(&source_artifacts.event_bus)),
            ..AgentScopeConfig::default()
        })
        .await
        .unwrap();

    // When forking the session
    let forked_session = env
        .manager
        .fork_session(&source_session, source_agent.id(), make_metadata("Forked"))
        .await
        .unwrap();
    let forked_artifacts = env
        .manager
        .session_artifacts(forked_session.id())
        .expect("forked session artifacts");

    // Then the fork's main agent carries the FORK session's event bus — a
    // goal created through its tools must announce on the fork's own stream,
    // never on the source's (the live goal card would otherwise never appear
    // in the fork's web client).
    let fork_main = forked_session
        .get_agent(&mirri_core::AgentId::new(
            mirri_core::session::manager::MAIN_AGENT_ID,
        ))
        .await
        .expect("fork main agent");
    let fork_bus = forked_artifacts.event_bus.clone();
    let agent_bus = fork_main
        .config()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .event_bus
        .clone();
    let agent_bus = agent_bus.expect("fork main agent must have an event bus");
    assert!(
        Arc::ptr_eq(&agent_bus, &fork_bus),
        "fork main agent's event bus must be the fork session's own bus"
    );
}

#[tokio::test]
async fn should_bind_fork_main_agent_to_fork_gates_when_fork_inherits_interaction_kernel() {
    // Given a source session whose main agent carries the source session's
    // interaction kernel (the embedded create path always wires one)
    let env = TestEnv::new().await;
    let source_session = env
        .manager
        .create_session(make_metadata("Source"), SessionScopeConfig::default())
        .await
        .unwrap();
    let source_artifacts = env
        .manager
        .session_artifacts(source_session.id())
        .expect("source session artifacts");
    let source_kernel = Arc::new(mirri_core::InteractionKernel::new(
        Arc::clone(&source_artifacts.approval_gate),
        Arc::clone(&source_artifacts.question_gate),
    ));
    let lifecycle = AgentLifecycle::new(Arc::clone(&source_session));
    let source_agent = lifecycle
        .create_agent(AgentScopeConfig {
            interaction_kernel: Some(source_kernel),
            ..AgentScopeConfig::default()
        })
        .await
        .unwrap();

    // When forking the session
    let forked_session = env
        .manager
        .fork_session(&source_session, source_agent.id(), make_metadata("Forked"))
        .await
        .unwrap();
    let forked_artifacts = env
        .manager
        .session_artifacts(forked_session.id())
        .expect("forked session artifacts");

    // Then the fork's main agent carries an interaction kernel bound to the
    // FORK session's gates — an approval asked through it registers with the
    // fork's pending lists, never with the source's.
    let fork_main = forked_session
        .get_agent(&mirri_core::AgentId::new(
            mirri_core::session::manager::MAIN_AGENT_ID,
        ))
        .await
        .expect("fork main agent");
    let kernel = fork_main
        .config()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .interaction_kernel
        .clone()
        .expect("fork main agent must have an interaction kernel");

    // Behavioral proof: register a pending approval on the FORK's gate and
    // observe it through the fork main agent's kernel.
    use mirri_core::ApprovalSnapshot;
    let fork_gate = forked_artifacts.approval_gate.clone();
    let (_receiver_id, _receiver) = fork_gate.register(ApprovalSnapshot {
        tool_name: "Bash".to_string(),
        action: "execute Bash".to_string(),
        display: serde_json::json!({}),
        agent_id: uuid::Uuid::nil(),
        turn_id: None,
        tool_call_id: None,
        created_at: 0,
        approval_rule: None,
        grant_dir: None,
    });
    let pending = kernel.approval_list_pending();
    assert_eq!(
        pending.len(),
        1,
        "the fork main agent's kernel must observe the fork's gate, not the source's"
    );
    assert!(
        source_artifacts.approval_gate.list_pending().is_empty(),
        "the source's gate must not observe the fork's registration"
    );
}

#[tokio::test]
async fn should_return_error_and_leave_live_session_intact_when_closing_nonexistent_session() {
    // Given a manager with an active session
    let env = TestEnv::new().await;
    let session = env
        .manager
        .create_session(make_metadata("Active"), SessionScopeConfig::default())
        .await
        .unwrap();
    let artifacts = env
        .manager
        .session_artifacts(session.id())
        .expect("session artifacts");
    let mut receiver = artifacts.event_bus.subscribe();

    // When closing a session that does not exist
    let result = env
        .manager
        .close_session(&SessionId::new("nonexistent-session"), CloseReason::Exit)
        .await;

    // Then the close fails, no event is published, and the live session's
    // gates are intact
    assert!(result.is_err());
    assert!(
        matches!(receiver.try_recv(), Err(TryRecvError::Empty)),
        "no event should be published for a failed close"
    );

    // And the live session's gates still accept and drain requests normally
    let request_gate = Arc::clone(&artifacts.approval_gate);
    let handle = tokio::spawn(async move { request_gate.request("bash", "ls").await });
    wait_until("approval registered", || {
        artifacts.approval_gate.pending_count() == 1
    })
    .await;
    artifacts.approval_gate.drain().await;
    assert_eq!(handle.await.unwrap(), ApprovalDecision::Timeout);
    assert_eq!(artifacts.approval_gate.pending_count(), 0);
}

// ===========================================================================
// Per-session isolation — events and gates never cross sessions
// ===========================================================================

#[tokio::test]
async fn should_keep_other_bus_silent_when_event_published_on_one_session_bus() {
    // Given two sessions with separate event buses
    let env = TestEnv::new().await;
    let first = env
        .manager
        .create_session(make_metadata("First"), SessionScopeConfig::default())
        .await
        .unwrap();
    let second = env
        .manager
        .create_session(make_metadata("Second"), SessionScopeConfig::default())
        .await
        .unwrap();
    let first_artifacts = env
        .manager
        .session_artifacts(first.id())
        .expect("first session artifacts");
    let second_artifacts = env
        .manager
        .session_artifacts(second.id())
        .expect("second session artifacts");
    let mut first_rx = first_artifacts.event_bus.subscribe();
    let mut second_rx = second_artifacts.event_bus.subscribe();

    // When an event is published on the first session's bus
    first_artifacts
        .event_bus
        .publish(SessionEvent::MessageReceived {
            agent_id: "main".to_string(),
            role: "user".to_string(),
            content_summary: "hello from session one".to_string(),
        });

    // Then the first session's subscriber receives it
    match recv_matching(&mut first_rx, "MessageReceived", |e| {
        matches!(e, SessionEvent::MessageReceived { .. })
    })
    .await
    {
        SessionEvent::MessageReceived {
            content_summary, ..
        } => assert_eq!(content_summary, "hello from session one"),
        other => panic!("expected MessageReceived, got {other:?}"),
    }

    // And the second session's bus never saw it
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        matches!(second_rx.try_recv(), Err(TryRecvError::Empty)),
        "events must not leak across session buses"
    );
}

#[tokio::test]
async fn should_not_affect_other_gate_when_approval_resolved_for_one_session() {
    // Given two sessions with separate approval gates
    let env = TestEnv::new().await;
    let first = env
        .manager
        .create_session(make_metadata("First"), SessionScopeConfig::default())
        .await
        .unwrap();
    let second = env
        .manager
        .create_session(make_metadata("Second"), SessionScopeConfig::default())
        .await
        .unwrap();
    let first_artifacts = env
        .manager
        .session_artifacts(first.id())
        .expect("first session artifacts");
    let second_artifacts = env
        .manager
        .session_artifacts(second.id())
        .expect("second session artifacts");

    // When a request is registered on the first session's gate
    let request_gate = Arc::clone(&first_artifacts.approval_gate);
    let handle = tokio::spawn(async move { request_gate.request("bash", "ls").await });
    let mut first_rx = first_artifacts.event_bus.subscribe();
    let request_id = match recv_matching(&mut first_rx, "ToolApprovalRequested", |e| {
        matches!(e, SessionEvent::ToolApprovalRequested { .. })
    })
    .await
    {
        SessionEvent::ToolApprovalRequested { request_id, .. } => request_id,
        other => panic!("expected ToolApprovalRequested, got {other:?}"),
    };
    assert_eq!(first_artifacts.approval_gate.pending_count(), 1);

    // And it is resolved through the first session's gate
    assert!(
        first_artifacts
            .approval_gate
            .resolve(request_id, ApprovalDecision::Approved)
    );
    assert_eq!(handle.await.unwrap(), ApprovalDecision::Approved);

    // Then the second session's gate never saw the request: it stayed empty,
    // and resolving the same request id against it reports failure
    assert_eq!(second_artifacts.approval_gate.pending_count(), 0);
    assert!(
        !second_artifacts
            .approval_gate
            .resolve(request_id, ApprovalDecision::Approved),
        "a request id from another session must not resolve on this gate"
    );
}

// ===========================================================================
// Event projection — the shared event.* wire vocabulary
// ===========================================================================

/// Envelope keys the transport owns; payload objects must never use them
/// (payload keys are merged flat into the notification's `params`).
const RESERVED_ENVELOPE_KEYS: [&str; 5] = ["sessionId", "volatile", "timestamp", "seq", "epoch"];

/// One instance of every [`SessionEvent`] variant (49 total), for
/// exhaustive enumeration in the projection invariants below.
fn all_variants() -> Vec<SessionEvent> {
    vec![
        SessionEvent::AgentCreated {
            agent_id: Uuid::nil(),
            session_id: Uuid::nil(),
        },
        SessionEvent::AgentStatusUpdated {
            agent_id: "main".to_string(),
            phase: mirri_core::session::agent_status::AgentPhase::Running,
            model: None,
            thinking_effort: None,
            context_tokens: None,
            max_context_tokens: None,
            plan_mode: None,
            swarm_mode: None,
            usage: None,
        },
        SessionEvent::AgentRemoved {
            agent_id: Uuid::nil(),
        },
        SessionEvent::SessionResumed {
            session_id: Uuid::nil(),
        },
        SessionEvent::SessionForked {
            source_session_id: Uuid::nil(),
            new_session_id: Uuid::nil(),
        },
        SessionEvent::SessionClosed {
            session_id: Uuid::nil(),
        },
        SessionEvent::ToolApprovalRequested {
            agent_id: Uuid::nil(),
            tool_name: "bash".to_string(),
            request_id: Uuid::nil(),
        },
        SessionEvent::ToolApprovalResolved {
            request_id: Uuid::nil(),
            approved: true,
        },
        SessionEvent::QuestionAsked {
            agent_id: Uuid::nil(),
            questions: serde_json::json!([{ "question": "q?" }]),
            request_id: Uuid::nil(),
        },
        SessionEvent::QuestionAnswered {
            request_id: Uuid::nil(),
            answer: "a".to_string(),
        },
        SessionEvent::QuestionDismissed {
            request_id: Uuid::nil(),
        },
        SessionEvent::MessageReceived {
            agent_id: "main".to_string(),
            role: "user".to_string(),
            content_summary: "hello".to_string(),
        },
        SessionEvent::AgentMessageChunk {
            agent_id: "main".to_string(),
            text: "chunk".to_string(),
        },
        SessionEvent::ThinkingDelta {
            agent_id: "main".to_string(),
            text: "thinking".to_string(),
        },
        SessionEvent::ToolCallDelta {
            agent_id: "main".to_string(),
            tool_call_id: "call_1".to_string(),
            tool_name: Some("bash".to_string()),
            arguments_part: "{\"a\":".to_string(),
        },
        SessionEvent::TurnFailed {
            agent_id: "main".to_string(),
            turn_id: "turn-1".to_string(),
            error: "boom".to_string(),
            error_code: "internal".to_string(),
            wire_turn_id: 1,
            ended_at_ms: None,
        },
        SessionEvent::TurnStarted {
            agent_id: "main".to_string(),
            session_id: "session-1".to_string(),
            turn_id: "turn-1".to_string(),
        },
        SessionEvent::TurnCompleted {
            agent_id: "main".to_string(),
            session_id: "session-1".to_string(),
            turn_id: "turn-1".to_string(),
            stop_reason: "completed".to_string(),
            wire_turn_id: 1,
            duration_ms: Some(500),
            ended_at_ms: None,
        },
        SessionEvent::ToolCallStarted {
            agent_id: "main".to_string(),
            session_id: "session-1".to_string(),
            tool_name: "bash".to_string(),
            tool_call_id: "call-1".to_string(),
            args: serde_json::json!({ "command": "cargo test" }),
            started_at_ms: 1_730_000_000_000,
        },
        SessionEvent::ToolCallCompleted {
            agent_id: "main".to_string(),
            session_id: "session-1".to_string(),
            tool_call_id: "call-1".to_string(),
            tool_name: "bash".to_string(),
            is_error: false,
            output: mirri_core::ToolOutput::Text("42 passed".to_string()),
            duration_ms: 500,
        },
        SessionEvent::ToolProgress {
            agent_id: "main".to_string(),
            tool_call_id: "call-1".to_string(),
            update: mirri_core::tool::ToolUpdate {
                kind: mirri_core::tool::ToolUpdateKind::Progress,
                text: Some("halfway".to_string()),
                percent: Some(50.0),
                custom_kind: None,
                custom_data: None,
            },
        },
        SessionEvent::CompactionStarted {
            agent_id: "main".to_string(),
            trigger: CompactionSource::Manual,
            instruction: Some("keep the test facts".to_string()),
        },
        SessionEvent::CompactionCancelled {
            agent_id: "main".to_string(),
        },
        SessionEvent::CompactionBlocked {
            agent_id: "main".to_string(),
            turn_id: Some("turn-1".to_string()),
        },
        SessionEvent::CompactionCompleted {
            agent_id: "main".to_string(),
            result: CompactionResult {
                applied: true,
                summary: "summary".to_string(),
                context_summary: "context summary".to_string(),
                compacted_count: 4,
                tokens_before: 100,
                tokens_after: 20,
                dropped_count: 1,
                dropped_entry_positions: Vec::new(),
                dropped_entry_seqs: Vec::new(),
                retained_tail_count: 0,
                kept_head_user_message_count: 0,
                elision_message: None,
                first_replaced_entry_seq: None,
                last_replaced_entry_seq: None,
            },
        },
        SessionEvent::HookResult {
            hook_event: "UserPromptSubmit".to_string(),
            content: "hook says hi".to_string(),
            blocked: None,
        },
        SessionEvent::BackgroundTaskStarted {
            info: BackgroundTaskInfo {
                kind: TaskKind::Process {
                    command: Some("cargo build".to_string()),
                    dispatcher: None,
                },
                task_id: "task-1".to_string(),
                description: "release build".to_string(),
                status: "running".to_string(),
                started_at: 1_700_000_000_000_i64,
                ended_at: None,
                stop_reason: None,
                agent_id: None,
                exit_code: None,
            },
        },
        SessionEvent::BackgroundTaskTerminated {
            info: BackgroundTaskInfo {
                kind: TaskKind::Agent {
                    agent_id: Some("agent-1".to_string()),
                    subagent_type: None,
                    parent_tool_call_id: None,
                    swarm_index: None,
                    model: None,
                },
                task_id: "task-2".to_string(),
                description: "subagent".to_string(),
                status: "failed".to_string(),
                started_at: 1_700_000_000_000_i64,
                ended_at: Some(1_700_000_001_500_i64),
                stop_reason: Some("boom".to_string()),
                agent_id: None,
                exit_code: None,
            },
        },
        SessionEvent::ShellStarted {
            command_id: "shell-1".to_string(),
            task_id: "task-3".to_string(),
        },
        SessionEvent::ShellOutput {
            command_id: "shell-1".to_string(),
            update: mirri_core::tool::ToolUpdate {
                kind: mirri_core::tool::ToolUpdateKind::Stdout,
                text: Some("chunk".to_string()),
                percent: None,
                custom_kind: None,
                custom_data: None,
            },
            task_id: Some("task-3".to_string()),
        },
        SessionEvent::ShellCompleted {
            command_id: "shell-1".to_string(),
            is_error: false,
            task_id: Some("task-3".to_string()),
        },
        SessionEvent::SubagentSpawned {
            subagent_id: "agent-1".to_string(),
            subagent_name: "coder".to_string(),
            description: Some("test spawn".to_string()),
            run_in_background: false,
            model: Some("test-model".to_string()),
            parent_agent_id: Some("main".to_string()),
            parent_tool_call_id: Some("call-1".to_string()),
            swarm_index: Some(0),
        },
        SessionEvent::SubagentStarted {
            subagent_id: "agent-1".to_string(),
        },
        // The legacy pre-ADR-011 free-form reason stays part of the
        // vocabulary (the compat-window `Legacy` form).
        SessionEvent::SubagentSuspended {
            subagent_id: "agent-1".to_string(),
            reason: SuspendCause::Legacy("rate_limited".to_string()),
        },
        SessionEvent::SubagentCompleted {
            subagent_id: "agent-1".to_string(),
            result_summary: "done".to_string(),
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 2,
            cache_creation_tokens: 1,
        },
        SessionEvent::SubagentFailed {
            subagent_id: "agent-1".to_string(),
            error: "turn failed".to_string(),
        },
        SessionEvent::TurnStepStarted {
            agent_id: "main".to_string(),
            turn_id: "turn-1".to_string(),
            step: 1,
        },
        SessionEvent::TurnStepCompleted {
            agent_id: "main".to_string(),
            turn_id: "turn-1".to_string(),
            step: 1,
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 2,
            cache_creation_tokens: 1,
            model: None,
        },
        SessionEvent::TurnStepInterrupted {
            agent_id: "main".to_string(),
            turn_id: Some("turn-1".to_string()),
            step: 2,
            reason: "aborted".to_string(),
            message: Some("cancelled by user".to_string()),
        },
        SessionEvent::TurnStepRetrying {
            agent_id: "main".to_string(),
            turn_id: Some("turn-1".to_string()),
            step: 2,
            failed_attempt: 1,
            next_attempt: 2,
            max_attempts: 3,
            delay_ms: 0,
        },
        SessionEvent::GoalUpdated {
            snapshot: serde_json::json!({ "goalId": "goal-1", "status": "active" }),
            change: Some(serde_json::json!({ "kind": "lifecycle", "status": "active" })),
        },
        SessionEvent::SkillActivated {
            activation_id: "act-1".to_string(),
            skill_name: "commit".to_string(),
            args: None,
            trigger: "user-slash".to_string(),
            skill_path: Some("/skills/commit".to_string()),
            skill_source: Some("project".to_string()),
        },
        SessionEvent::McpServerStatus {
            name: "server-1".to_string(),
            transport: "stdio".to_string(),
            status: "connected".to_string(),
            tool_count: 3,
            error: None,
            pool: "workspace".to_string(),
            change_kind: Some("added".to_string()),
        },
        SessionEvent::SessionMetaUpdated {
            title: Some("renamed".to_string()),
            patch: Some(serde_json::json!({ "title": "renamed" })),
        },
        SessionEvent::ModelSwitched {
            from: "kimi-k2".to_string(),
            to: "claude-sonnet-4".to_string(),
        },
        SessionEvent::PluginCommandActivated {
            activation_id: "act-2".to_string(),
            plugin_id: "plugin-1".to_string(),
            command_name: "build".to_string(),
            command_args: Some("--release".to_string()),
            trigger: "user-slash".to_string(),
        },
        SessionEvent::CronFired {
            task_id: "cron-1".to_string(),
            cron: "* * * * *".to_string(),
            prompt: "check the build".to_string(),
            recurring: true,
            coalesced_count: 1,
            stale: false,
            maxed_out: false,
            readonly: false,
        },
        SessionEvent::SessionWorkChanged {
            state: SessionActivityState::default(),
        },
        SessionEvent::Unknown,
    ]
}

fn projected(event: &SessionEvent) -> event_projection::ProjectedEvent {
    event_projection::project(event).expect("event has a wire projection")
}

#[test]
fn should_carry_real_coalesce_and_stale_when_projecting_a_cron_fired_event() {
    // Given a CronFired event with a coalesced, stale fire
    let event = SessionEvent::CronFired {
        task_id: "cron-1".to_string(),
        cron: "*/5 * * * *".to_string(),
        prompt: "tick".to_string(),
        recurring: true,
        coalesced_count: 3,
        stale: true,
        maxed_out: false,
        readonly: false,
    };

    // When projected onto the wire event
    let projected = projected(&event);

    // Then the payload mirrors the TS CronFiredEvent origin
    assert_eq!(projected.method, "event.cron.fired");
    assert_eq!(projected.payload["origin"]["coalescedCount"], 3);
    assert_eq!(projected.payload["origin"]["stale"], true);
    assert_eq!(projected.payload["origin"]["jobId"], "cron-1");
    assert_eq!(projected.payload["prompt"], "tick");
}

#[test]
fn should_project_session_work_changed_onto_the_durable_event_with_snake_case_payload() {
    // Given a SessionWorkChanged event carrying a non-idle activity state
    let event = SessionEvent::SessionWorkChanged {
        state: SessionActivityState {
            busy: true,
            main_turn_active: true,
            pending_interaction: SessionPendingInteraction::None,
            last_turn_reason: Some(SessionTurnOutcome::Completed),
        },
    };

    // When projected onto the wire event
    let projected = projected(&event);

    // Then the durable event carries the snake_case activity state payload
    assert_eq!(projected.method, "event.session.work_changed");
    assert!(!projected.volatile);
    assert_eq!(projected.payload["busy"], serde_json::json!(true));
    assert_eq!(
        projected.payload["main_turn_active"],
        serde_json::json!(true)
    );
    assert_eq!(
        projected.payload["pending_interaction"],
        serde_json::json!("none")
    );
    assert_eq!(
        projected.payload["last_turn_reason"],
        serde_json::json!("completed")
    );
}

#[test]
fn should_project_message_received_to_event_message_received_when_event_is_message_received() {
    // Given a message_received event
    let event = SessionEvent::MessageReceived {
        agent_id: "main".to_string(),
        role: "user".to_string(),
        content_summary: "hello".to_string(),
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.message.received with the camelCase payload
    assert_eq!(projected.method, "event.message.received");
    assert!(!projected.volatile);
    assert_eq!(projected.payload["agentId"], "main");
    assert_eq!(projected.payload["role"], "user");
    assert_eq!(projected.payload["contentSummary"], "hello");
}

#[test]
fn should_project_agent_message_chunk_to_event_assistant_delta_when_event_is_agent_message_chunk() {
    // Given an agent_message_chunk streaming event
    let event = SessionEvent::AgentMessageChunk {
        agent_id: "main".to_string(),
        text: "streamed".to_string(),
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.assistant.delta with the chunk payload
    // (the `delta` key, no legacy `done` marker)
    assert_eq!(projected.method, "event.assistant.delta");
    assert!(projected.volatile);
    assert_eq!(projected.payload["agentId"], "main");
    assert_eq!(projected.payload["delta"], "streamed");
    assert!(
        projected.payload.get("done").is_none(),
        "the delta payload no longer carries the done marker"
    );
}

#[test]
fn should_project_a_thinking_delta_to_event_thinking_delta_when_event_is_thinking_delta() {
    // Given a thinking_delta streaming event (the live reasoning channel)
    let event = SessionEvent::ThinkingDelta {
        agent_id: "main".to_string(),
        text: "reasoning".to_string(),
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.thinking.delta with the same shape the
    // assistant delta carries — agent id plus the chunk payload, volatile
    assert_eq!(projected.method, "event.thinking.delta");
    assert!(projected.volatile);
    assert_eq!(projected.payload["agentId"], "main");
    assert_eq!(projected.payload["delta"], "reasoning");
}

#[test]
fn should_project_a_tool_call_delta_to_event_tool_call_delta_when_event_is_tool_call_delta() {
    // Given a tool_call_delta streaming event with a known identity
    let event = SessionEvent::ToolCallDelta {
        agent_id: "main".to_string(),
        tool_call_id: "call_1".to_string(),
        tool_name: Some("bash".to_string()),
        arguments_part: "{\"a\":".to_string(),
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.tool.call.delta — volatile, tool call
    // id plus the argument fragment, mirroring the thinking delta shape
    assert_eq!(projected.method, "event.tool.call.delta");
    assert!(projected.volatile);
    assert_eq!(projected.payload["agentId"], "main");
    assert_eq!(projected.payload["toolCallId"], "call_1");
    assert_eq!(projected.payload["name"], "bash");
    assert_eq!(projected.payload["argumentsPart"], "{\"a\":");
}

#[test]
fn should_omit_the_name_when_a_tool_call_delta_has_no_tool_name() {
    // Given a tool_call_delta whose provider part carried no identity
    let event = SessionEvent::ToolCallDelta {
        agent_id: "main".to_string(),
        tool_call_id: "call_1".to_string(),
        tool_name: None,
        arguments_part: "}".to_string(),
    };

    // When projecting it
    let projected = projected(&event);

    // Then the optional name stays absent — never a null placeholder, the
    // protocol schema (z.string().optional()) rejects null
    assert_eq!(projected.method, "event.tool.call.delta");
    assert!(projected.payload.get("name").is_none());
    assert_eq!(projected.payload["argumentsPart"], "}");
}

#[test]
fn should_project_a_compaction_started_to_event_compaction_started_when_event_is_compaction_started()
 {
    // Given a manual compaction started event
    let event = SessionEvent::CompactionStarted {
        agent_id: "main".to_string(),
        trigger: CompactionSource::Manual,
        instruction: Some("keep the test facts".to_string()),
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.compaction.started with the trigger and
    // the optional instruction, durable (compaction is a lifecycle change)
    assert_eq!(projected.method, "event.compaction.started");
    assert!(!projected.volatile);
    assert_eq!(projected.payload["trigger"], "manual");
    assert_eq!(projected.payload["instruction"], "keep the test facts");
}

#[test]
fn should_omit_the_instruction_when_a_compaction_started_has_no_instruction() {
    // Given an auto-triggered compaction started event without one
    let event = SessionEvent::CompactionStarted {
        agent_id: "main".to_string(),
        trigger: CompactionSource::Auto,
        instruction: None,
    };

    // When projecting it
    let projected = projected(&event);

    // Then the trigger maps to the protocol vocabulary and the absent
    // instruction stays absent (no null placeholder)
    assert_eq!(projected.payload["trigger"], "auto");
    assert!(projected.payload.get("instruction").is_none());
}

#[test]
fn should_project_a_compaction_cancelled_to_event_compaction_cancelled_when_event_is_compaction_cancelled()
 {
    // Given a compaction cancelled event
    let event = SessionEvent::CompactionCancelled {
        agent_id: "main".to_string(),
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.compaction.cancelled carrying only the
    // agent's attribution — the protocol type carries no other fields — and
    // durable
    assert_eq!(projected.method, "event.compaction.cancelled");
    assert!(!projected.volatile);
    assert_eq!(projected.payload, serde_json::json!({ "agentId": "main" }));
}

#[test]
fn should_project_a_compaction_completed_to_event_compaction_completed_when_event_is_compaction_completed()
 {
    // Given a compaction completed event carrying the engine's result
    let event = SessionEvent::CompactionCompleted {
        agent_id: "main".to_string(),
        result: CompactionResult {
            applied: true,
            summary: "the summary".to_string(),
            context_summary: "context: the summary".to_string(),
            compacted_count: 7,
            tokens_before: 1000,
            tokens_after: 120,
            dropped_count: 2,
            dropped_entry_positions: Vec::new(),
            dropped_entry_seqs: Vec::new(),
            retained_tail_count: 0,
            kept_head_user_message_count: 0,
            elision_message: None,
            first_replaced_entry_seq: None,
            last_replaced_entry_seq: None,
        },
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.compaction.completed with the protocol's
    // CompactionResult fields (camelCase); the rust-only context_summary
    // stays off the wire and the untracked kept* fields stay absent
    assert_eq!(projected.method, "event.compaction.completed");
    assert!(!projected.volatile);
    assert_eq!(projected.payload["result"]["summary"], "the summary");
    assert_eq!(projected.payload["result"]["compactedCount"], 7);
    assert_eq!(projected.payload["result"]["tokensBefore"], 1000);
    assert_eq!(projected.payload["result"]["tokensAfter"], 120);
    assert_eq!(projected.payload["result"]["droppedCount"], 2);
    assert!(projected.payload["result"].get("contextSummary").is_none());
    assert!(
        projected.payload["result"]
            .get("keptUserMessageCount")
            .is_none()
    );
    assert!(
        projected.payload["result"]
            .get("keptHeadUserMessageCount")
            .is_none()
    );
}

#[test]
fn should_project_a_compaction_blocked_to_event_compaction_blocked_when_event_is_compaction_blocked()
 {
    // Given a compaction blocked event without a wire turn id (the engine
    // does not plumb one down to the turn loop)
    let event = SessionEvent::CompactionBlocked {
        agent_id: "main".to_string(),
        turn_id: None,
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.compaction.blocked carrying only the
    // agent's attribution — turnId is optional on the protocol type and stays
    // absent
    assert_eq!(projected.method, "event.compaction.blocked");
    assert!(!projected.volatile);
    assert_eq!(projected.payload, serde_json::json!({ "agentId": "main" }));
}

#[test]
fn should_carry_the_turn_id_when_a_compaction_blocked_event_has_one() {
    // Given a compaction blocked event carrying a wire turn id
    let event = SessionEvent::CompactionBlocked {
        agent_id: "main".to_string(),
        turn_id: Some("turn-abc".to_string()),
    };

    // When projecting it
    let projected = projected(&event);

    // Then the payload carries it under turnId (a future turn-loop that
    // plumbs the wire id would surface it without a projection change)
    assert_eq!(projected.payload["turnId"], "turn-abc");
}

#[test]
fn should_omit_turn_id_from_the_serialised_form_when_a_compaction_blocked_has_none() {
    // Given a compaction blocked event without a wire turn id
    let event = SessionEvent::CompactionBlocked {
        agent_id: "main".to_string(),
        turn_id: None,
    };

    // When serde-serialising the event itself (the ws relay serialises
    // SessionEvent directly, without going through the projection)
    let json = serde_json::to_value(&event).expect("event serialises");

    // Then the wire type tag is present and the absent turn id stays absent
    // (no null placeholder on the relayed frame)
    assert_eq!(json["type"], "compaction_blocked");
    assert!(json.get("turn_id").is_none());
}

#[test]
fn should_project_a_hook_result_to_event_hook_result_when_event_is_hook_result() {
    // Given a passing hook's rendered result (no block)
    let event = SessionEvent::HookResult {
        hook_event: "UserPromptSubmit".to_string(),
        content: "hook says hi".to_string(),
        blocked: None,
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.hook.result with the camelCase payload —
    // durable (the result is rendered into the conversation context) and
    // the absent blocked flag stays absent (no null placeholder)
    assert_eq!(projected.method, "event.hook.result");
    assert!(!projected.volatile);
    assert_eq!(projected.payload["hookEvent"], "UserPromptSubmit");
    assert_eq!(projected.payload["content"], "hook says hi");
    assert!(projected.payload.get("blocked").is_none());
}

#[test]
fn should_carry_blocked_when_a_hook_result_event_has_one() {
    // Given a blocking hook's rendered result
    let event = SessionEvent::HookResult {
        hook_event: "UserPromptSubmit".to_string(),
        content: "you shall not pass".to_string(),
        blocked: Some(true),
    };

    // When projecting it
    let projected = projected(&event);

    // Then the payload carries the blocked flag
    assert_eq!(projected.method, "event.hook.result");
    assert_eq!(projected.payload["blocked"], serde_json::json!(true));
}

#[test]
fn should_omit_blocked_from_the_serialised_form_when_a_hook_result_has_none() {
    // Given a passing hook result event
    let event = SessionEvent::HookResult {
        hook_event: "UserPromptSubmit".to_string(),
        content: "hook says hi".to_string(),
        blocked: None,
    };

    // When serde-serialising the event itself (the ws relay serialises
    // SessionEvent directly, without going through the projection)
    let json = serde_json::to_value(&event).expect("event serialises");

    // Then the wire type tag is present and the absent blocked flag stays
    // absent (no null placeholder on the relayed frame)
    assert_eq!(json["type"], "hook_result");
    assert!(json.get("blocked").is_none());
}

#[test]
fn should_project_turn_started_with_agent_attribution() {
    // Given a turn_started event carrying the agent's wire identity
    let event = SessionEvent::TurnStarted {
        agent_id: "main".to_string(),
        session_id: "s-1".to_string(),
        turn_id: "turn-1".to_string(),
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.turn.started carrying the agentId the
    // web routes member rows by
    assert_eq!(projected.method, "event.turn.started");
    assert_eq!(projected.payload["agentId"], "main");
    assert_eq!(projected.payload["turnId"], "turn-1");
}

#[test]
fn should_project_turn_completed_with_agent_attribution() {
    // Given a turn_completed event carrying a subagent's wire identity
    let event = SessionEvent::TurnCompleted {
        agent_id: "agent-0192-aaaa".to_string(),
        session_id: "s-1".to_string(),
        turn_id: "turn-1".to_string(),
        stop_reason: "completed".to_string(),
        wire_turn_id: 1,
        duration_ms: None,
        ended_at_ms: None,
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.turn.ended carrying the agentId the
    // web routes member rows by plus the stop reason under `reason`
    assert_eq!(projected.method, "event.turn.ended");
    assert_eq!(projected.payload["agentId"], "agent-0192-aaaa");
    assert_eq!(projected.payload["reason"], "completed");
}

#[test]
fn should_project_turn_completed_with_the_server_measured_duration_when_the_event_carries_it() {
    // Given a turn_completed event whose engine measured the turn's duration
    // (the same wall-clock the wire `turn.ended` record carries — a client
    // that resynced mid-turn has no local anchor to estimate it from)
    let event = SessionEvent::TurnCompleted {
        agent_id: "main".to_string(),
        session_id: "s-1".to_string(),
        turn_id: "turn-1".to_string(),
        stop_reason: "completed".to_string(),
        wire_turn_id: 1,
        duration_ms: Some(90_324),
        ended_at_ms: None,
    };

    // When projecting it
    let projected = projected(&event);

    // Then the event.turn.ended payload carries the server's authoritative
    // durationMs, so a live completion renders "Worked for X" instead of a
    // naked "Worked" after a resync dropped the client's start anchor
    assert_eq!(projected.method, "event.turn.ended");
    assert_eq!(projected.payload["durationMs"], 90_324);
}

#[test]
fn should_omit_duration_ms_from_the_projection_when_the_completed_turn_carries_none() {
    // Given a turn_completed event with no measured duration (engines that
    // predate the field, or a turn that ended without a TurnEndResult)
    let event = SessionEvent::TurnCompleted {
        agent_id: "main".to_string(),
        session_id: "s-1".to_string(),
        turn_id: "turn-1".to_string(),
        stop_reason: "completed".to_string(),
        wire_turn_id: 1,
        duration_ms: None,
        ended_at_ms: None,
    };

    // When projecting it
    let projected = projected(&event);

    // Then the event.turn.ended payload omits the key entirely (absent, not
    // null) so legacy consumers keep their previous behavior
    assert_eq!(projected.method, "event.turn.ended");
    assert!(
        projected.payload.get("durationMs").is_none(),
        "payload: {}",
        projected.payload
    );
}

#[test]
fn should_carry_ended_at_ms_on_the_turn_ended_payload_when_the_completed_turn_measures_one() {
    // Given: a TurnCompleted carrying an ended_at measurement
    let event = SessionEvent::TurnCompleted {
        agent_id: "main".to_string(),
        session_id: "s-1".to_string(),
        turn_id: "turn-1".to_string(),
        stop_reason: "completed".to_string(),
        wire_turn_id: 7,
        duration_ms: Some(1200),
        ended_at_ms: Some(1_730_000_000_000),
    };

    // When projected
    let projected = projected(&event);

    // Then the payload carries the same instant, untransformed, so the
    // footer's "completed at" stamp is engine-authoritative on the live path
    assert_eq!(projected.method, "event.turn.ended");
    assert_eq!(projected.payload["endedAtMs"], 1_730_000_000_000_u64);
}

#[test]
fn should_carry_ended_at_ms_on_the_turn_ended_payload_when_the_failed_turn_measures_one() {
    // Given: a TurnFailed carrying an ended_at measurement
    let event = SessionEvent::TurnFailed {
        agent_id: "main".to_string(),
        turn_id: "turn-1".to_string(),
        error: "boom".to_string(),
        error_code: "E_BOOM".to_string(),
        wire_turn_id: 7,
        ended_at_ms: Some(1_730_000_000_001),
    };

    // When projected
    let projected = projected(&event);

    // Then the failure frame carries the instant too — both terminal frames
    // are engine-authoritative about when the turn ended
    assert_eq!(projected.method, "event.turn.ended");
    assert_eq!(projected.payload["endedAtMs"], 1_730_000_000_001_u64);
}

#[test]
fn should_omit_ended_at_ms_from_the_turn_ended_payload_when_the_event_carries_none() {
    // Given: terminal events with no measurement (an older producer)
    for event in [
        SessionEvent::TurnCompleted {
            agent_id: "main".to_string(),
            session_id: "s-1".to_string(),
            turn_id: "turn-1".to_string(),
            stop_reason: "completed".to_string(),
            wire_turn_id: 7,
            duration_ms: Some(1200),
            ended_at_ms: None,
        },
        SessionEvent::TurnFailed {
            agent_id: "main".to_string(),
            turn_id: "turn-1".to_string(),
            error: "boom".to_string(),
            error_code: "E_BOOM".to_string(),
            wire_turn_id: 7,
            ended_at_ms: None,
        },
    ] {
        // When projected
        let projected = projected(&event);

        // Then the key is absent (never null), mirroring durationMs
        assert!(
            projected.payload.get("endedAtMs").is_none(),
            "payload: {}",
            projected.payload
        );
    }
}

#[test]
fn should_project_compaction_events_with_agent_attribution() {
    // Given the compaction lifecycle events carrying the agent's wire
    // identity
    for event in [
        SessionEvent::CompactionStarted {
            agent_id: "main".to_string(),
            trigger: CompactionSource::Manual,
            instruction: None,
        },
        SessionEvent::CompactionCancelled {
            agent_id: "main".to_string(),
        },
        SessionEvent::CompactionBlocked {
            agent_id: "main".to_string(),
            turn_id: None,
        },
        SessionEvent::CompactionCompleted {
            agent_id: "main".to_string(),
            result: CompactionResult {
                applied: true,
                summary: "the summary".to_string(),
                context_summary: "context: the summary".to_string(),
                compacted_count: 7,
                tokens_before: 1000,
                tokens_after: 120,
                dropped_count: 2,
                dropped_entry_positions: Vec::new(),
                dropped_entry_seqs: Vec::new(),
                retained_tail_count: 0,
                kept_head_user_message_count: 0,
                elision_message: None,
                first_replaced_entry_seq: None,
                last_replaced_entry_seq: None,
            },
        },
    ] {
        // When projecting it
        let projected = projected(&event);

        // Then the payload carries the agentId the web routes member rows by
        assert_eq!(
            projected.payload["agentId"], "main",
            "payload: {}",
            projected.payload
        );
    }
}

#[test]
fn should_project_turn_failed_to_event_turn_ended_with_failed_reason_when_event_is_turn_failed() {
    // Given a turn_failed event whose engine error carries its domain code
    // and whose agent is identified by its wire id (the same string identity
    // every other session event carries — "main" for the main agent)
    let event = SessionEvent::TurnFailed {
        agent_id: "main".to_string(),
        turn_id: "turn-1".to_string(),
        error: "provider rejected the request".to_string(),
        error_code: "provider.api_error".to_string(),
        wire_turn_id: 0,
        ended_at_ms: None,
    };

    // When projecting it
    let projected = projected(&event);

    // Then it folds into the terminal event.turn.ended method with
    // reason "failed" and a structured error object carrying the real
    // domain code — not a hardcoded internal — plus its retryability
    // verdict. The payload's agentId must stay the agent's wire identity:
    // clients (the web projector) route turn.ended frames by it — an id
    // that is not the main agent's makes them treat the frame as a
    // subagent's and leaves the main turn's UI stuck "thinking"
    assert_eq!(projected.method, "event.turn.ended");
    assert!(!projected.volatile);
    assert_eq!(projected.payload["agentId"], "main");
    assert_eq!(projected.payload["turnId"], "turn-1");
    assert_eq!(projected.payload["reason"], "failed");
    assert_eq!(projected.payload["error"]["code"], "provider.api_error");
    assert_eq!(
        projected.payload["error"]["message"],
        "provider rejected the request"
    );
    assert_eq!(projected.payload["error"]["retryable"], false);
}

#[test]
fn should_project_the_user_cancelled_abort_as_cancelled_reason_when_the_failed_turn_reports_it() {
    // Given a turn_failed event carrying the user-cancellation domain code —
    // the hard-abort path's bracket close (message.abort / goal.pause /
    // goal.cancel publish it with this exact code)
    let event = SessionEvent::TurnFailed {
        agent_id: "main".to_string(),
        turn_id: "turn-3".to_string(),
        error: "turn aborted by the user via message.abort".to_string(),
        error_code: "abort.user_cancelled".to_string(),
        wire_turn_id: 0,
        ended_at_ms: None,
    };

    // When projecting it
    let projected = projected(&event);

    // Then the terminal event.turn.ended reports reason "cancelled" — the
    // vocabulary IS the attribution (ADR-007): a user-initiated hard abort
    // must not render as an engine failure — while the structured error
    // payload is retained for diagnostics
    assert_eq!(projected.method, "event.turn.ended");
    assert_eq!(projected.payload["reason"], "cancelled");
    assert_eq!(projected.payload["error"]["code"], "abort.user_cancelled");
    assert_eq!(
        projected.payload["error"]["message"],
        "turn aborted by the user via message.abort"
    );
    assert_eq!(projected.payload["error"]["retryable"], false);
}

#[test]
fn should_project_a_system_abort_as_failed_reason_when_the_code_is_not_user_cancelled() {
    // Given a turn_failed event carrying the SYSTEM-abort domain code —
    // the exact-match guard's negative case: abort.cancelled must NOT be
    // swept into the user-cancellation vocabulary
    let event = SessionEvent::TurnFailed {
        agent_id: "main".to_string(),
        turn_id: "turn-4".to_string(),
        error: "operation cancelled".to_string(),
        error_code: "abort.cancelled".to_string(),
        wire_turn_id: 0,
        ended_at_ms: None,
    };

    // When projecting it
    let projected = projected(&event);

    // Then it stays in the failed vocabulary — only the user-cancellation
    // code flips (ADR-007's rejected-alternative b: prefix matching would
    // misattribute system aborts)
    assert_eq!(projected.payload["reason"], "failed");
    assert_eq!(projected.payload["error"]["code"], "abort.cancelled");
}

#[test]
fn should_project_the_retryable_verdict_when_a_failed_turn_reports_context_overflow() {
    // Given a turn_failed event whose engine error is context overflow —
    // the one domain code the metadata table marks retryable
    let event = SessionEvent::TurnFailed {
        agent_id: "main".to_string(),
        turn_id: "turn-2".to_string(),
        error: "context overflow after compaction".to_string(),
        error_code: "context.overflow".to_string(),
        wire_turn_id: 0,
        ended_at_ms: None,
    };

    // When projecting it
    let projected = projected(&event);

    // Then the structured error object reports retryable true, so clients
    // can offer a compact-and-resend action instead of a dead end
    assert_eq!(projected.payload["error"]["code"], "context.overflow");
    assert_eq!(projected.payload["error"]["retryable"], true);
}

#[test]
fn should_project_tool_call_started_to_event_tool_call_started_when_event_is_tool_call_started() {
    // Given a tool_call_started event
    let event = SessionEvent::ToolCallStarted {
        agent_id: "main".to_string(),
        session_id: "session-1".to_string(),
        tool_name: "bash".to_string(),
        tool_call_id: "call-1".to_string(),
        args: serde_json::json!({ "command": "cargo test" }),
        started_at_ms: 1_730_000_000_000,
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.tool.call.started — durable (its result
    // counterpart must replay so reconnecting clients see finished tool
    // calls), with the tool name under `name`, the full argument object
    // under `args` (the TS protocol's field), and no payload sessionId
    assert_eq!(projected.method, "event.tool.call.started");
    assert!(!projected.volatile);
    assert_eq!(projected.payload["toolCallId"], "call-1");
    assert_eq!(projected.payload["name"], "bash");
    assert_eq!(projected.payload["args"]["command"], "cargo test");
    assert!(projected.payload.get("sessionId").is_none());
}

#[test]
fn should_carry_started_at_and_duration_when_live_relay_translates_tool_events() {
    // Given a live tool-call started / completed pair carrying timing
    let started = mirri_core::session::live_relay::live_event_to_session_event(
        mirri_core::loop_engine::events::LiveEvent::ToolCallStarted {
            tool_name: "bash".to_string(),
            tool_call_id: "call-1".to_string(),
            args: serde_json::json!({}),
            started_at: 1_730_000_000_000_u64,
        },
        "main",
        "session-1",
        None,
    )
    .expect("started translates");

    // When translating, the session event carries the epoch-ms start
    match started {
        SessionEvent::ToolCallStarted { started_at_ms, .. } => {
            assert_eq!(started_at_ms, 1_730_000_000_000);
        }
        other => panic!("unexpected variant: {other:?}"),
    }

    let completed = mirri_core::session::live_relay::live_event_to_session_event(
        mirri_core::loop_engine::events::LiveEvent::ToolCallCompleted {
            tool_call_id: "call-1".to_string(),
            tool_name: "bash".to_string(),
            is_error: false,
            output: mirri_core::tool::ToolOutput::Text("ok".to_string()),
            duration_ms: 1_234,
        },
        "main",
        "session-1",
        None,
    )
    .expect("completed translates");
    match completed {
        SessionEvent::ToolCallCompleted { duration_ms, .. } => {
            assert_eq!(duration_ms, 1_234);
        }
        other => panic!("unexpected variant: {other:?}"),
    }
}

#[test]
fn should_project_timing_fields_when_tool_events_carry_them() {
    // Given a started event with an epoch-ms start
    let event = SessionEvent::ToolCallStarted {
        agent_id: "main".to_string(),
        session_id: "session-1".to_string(),
        tool_name: "bash".to_string(),
        tool_call_id: "call-1".to_string(),
        args: serde_json::json!({}),
        started_at_ms: 1_730_000_000_000,
    };
    // When projecting, the wire payload carries startedAtMs
    let started_frame = projected(&event);
    assert_eq!(started_frame.payload["startedAtMs"], 1_730_000_000_000_u64);

    // Given a completed event with a duration
    let event = SessionEvent::ToolCallCompleted {
        agent_id: "main".to_string(),
        session_id: "session-1".to_string(),
        tool_call_id: "call-1".to_string(),
        tool_name: "bash".to_string(),
        is_error: false,
        output: mirri_core::tool::ToolOutput::Text("ok".to_string()),
        duration_ms: 1_234,
    };
    // When projecting, the wire payload carries durationMs
    let completed_frame = projected(&event);
    assert_eq!(completed_frame.payload["durationMs"], 1_234_u64);
}

#[test]
fn should_omit_timing_fields_when_the_engine_recorded_zero() {
    // Given a replayed legacy record whose timing fields deserialized to 0
    // (serde(default) fills them), the zero is "absent", not a real instant:
    // a startedAtMs of 0 would render a 56-year timer and a durationMs of 0 a
    // meaningless 0ms label.
    let event = SessionEvent::ToolCallStarted {
        agent_id: "main".to_string(),
        session_id: "session-1".to_string(),
        tool_name: "bash".to_string(),
        tool_call_id: "call-1".to_string(),
        args: serde_json::json!({}),
        started_at_ms: 0,
    };
    // When projecting, the payload omits startedAtMs so clients treat it as
    // missing rather than ticking from epoch 0
    let started_frame = projected(&event);
    assert!(started_frame.payload.get("startedAtMs").is_none());

    let event = SessionEvent::ToolCallCompleted {
        agent_id: "main".to_string(),
        session_id: "session-1".to_string(),
        tool_call_id: "call-1".to_string(),
        tool_name: "bash".to_string(),
        is_error: false,
        output: mirri_core::tool::ToolOutput::Text("ok".to_string()),
        duration_ms: 0,
    };
    // When projecting, the payload omits durationMs. A genuine sub-millisecond
    // tool also lands on 0 and then shows no duration — an acceptable trade
    // for never rendering a fabricated 0ms on replayed old logs.
    let completed_frame = projected(&event);
    assert!(completed_frame.payload.get("durationMs").is_none());
}

/// Given a tool event emitted under a member agent's relay, When projected to
/// the wire, Then the frame carries the member's agentId — the field the web
/// routes member tool frames by (supervision P2+1; the pre-fix frames were
/// indistinguishable from the main agent's and leaked into its transcript).
#[test]
fn should_project_the_agent_id_on_tool_frames() {
    for event in [
        SessionEvent::ToolCallStarted {
            agent_id: "agent-34d6376b-c9ed-4555-9f99-2256e024348d".to_string(),
            session_id: "session-1".to_string(),
            tool_name: "Read".to_string(),
            tool_call_id: "call-1".to_string(),
            args: serde_json::json!({ "file_path": "note.txt" }),
            started_at_ms: 1_730_000_000_000,
        },
        SessionEvent::ToolCallCompleted {
            agent_id: "agent-34d6376b-c9ed-4555-9f99-2256e024348d".to_string(),
            session_id: "session-1".to_string(),
            tool_call_id: "call-1".to_string(),
            tool_name: "Read".to_string(),
            is_error: false,
            output: mirri_core::tool::ToolOutput::text("ok"),
            duration_ms: 500,
        },
        SessionEvent::ToolProgress {
            agent_id: "agent-34d6376b-c9ed-4555-9f99-2256e024348d".to_string(),
            tool_call_id: "call-1".to_string(),
            update: mirri_core::tool::ToolUpdate {
                kind: mirri_core::tool::ToolUpdateKind::Status,
                text: Some("working".to_string()),
                percent: None,
                custom_kind: None,
                custom_data: None,
            },
        },
    ] {
        let projected =
            mirri_core::session::event_projection::project(&event).expect("tool events project");
        let payload = projected.payload;
        assert_eq!(
            payload.get("agentId").and_then(serde_json::Value::as_str),
            Some("agent-34d6376b-c9ed-4555-9f99-2256e024348d"),
            "agentId missing on {}: {:?}",
            projected.method,
            payload
        );
    }
}

#[test]
fn should_project_tool_call_completed_to_event_tool_result_when_event_is_tool_call_completed() {
    // Given a tool_call_completed event
    let event = SessionEvent::ToolCallCompleted {
        agent_id: "main".to_string(),
        session_id: "session-1".to_string(),
        tool_call_id: "call-1".to_string(),
        tool_name: "bash".to_string(),
        is_error: true,
        output: mirri_core::ToolOutput::Text("boom: exit 1".to_string()),
        duration_ms: 500,
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.tool.result — durable so replay shows
    // finished tool calls — with the tool's name (a consumer that missed
    // the started event still attributes the result to its tool), the
    // result's output text, and no payload sessionId
    assert_eq!(projected.method, "event.tool.result");
    assert!(!projected.volatile);
    assert_eq!(projected.payload["toolCallId"], "call-1");
    assert_eq!(projected.payload["name"], "bash");
    assert_eq!(projected.payload["isError"], true);
    assert_eq!(projected.payload["output"], "boom: exit 1");
    assert!(projected.payload.get("sessionId").is_none());
}

#[test]
fn should_project_turn_steps_with_the_wire_turn_id_the_started_event_carries() {
    // Given a turn whose steps publish under the wire turn id (`turn-<uuid>`)
    // — the same key `turn.started` / `turn.ended` carry
    let started = SessionEvent::TurnStepStarted {
        agent_id: "main".to_string(),
        turn_id: "turn-1".to_string(),
        step: 1,
    };
    let completed = SessionEvent::TurnStepCompleted {
        agent_id: "main".to_string(),
        turn_id: "turn-1".to_string(),
        step: 1,
        input_tokens: 10,
        output_tokens: 5,
        cache_read_tokens: 2,
        cache_creation_tokens: 1,
        model: None,
    };

    // When projecting both
    let started_projected = projected(&started);
    let completed_projected = projected(&completed);

    // Then each payload carries the wire turn id under `turnId` — the
    // turn.step family routes by the same key the turn boundary events use
    assert_eq!(started_projected.method, "event.turn.step.started");
    assert_eq!(started_projected.payload["turnId"], "turn-1");
    assert_eq!(started_projected.payload["step"], 1);
    assert_eq!(completed_projected.method, "event.turn.step.completed");
    assert_eq!(completed_projected.payload["turnId"], "turn-1");
    assert_eq!(completed_projected.payload["step"], 1);
}

/// Given a turn step recorded under a model alias, when the step-completed
/// event is projected, then the wire payload carries the model alongside
/// the four usage buckets (turn-level usage attribution, ADR-002).
#[test]
fn should_carry_the_model_on_turn_step_completed_when_projected() {
    let event = SessionEvent::TurnStepCompleted {
        agent_id: "main".to_string(),
        turn_id: "turn-1".to_string(),
        step: 1,
        input_tokens: 10,
        output_tokens: 5,
        cache_read_tokens: 200,
        cache_creation_tokens: 0,
        model: Some("expert".to_string()),
    };
    let projected = projected(&event);
    assert_eq!(projected.payload["model"], serde_json::json!("expert"));
}

/// Given a turn step with no model attribution, when the step-completed
/// event is projected, then the wire payload omits the `model` key —
/// no invented fallback value rides the event.
#[test]
fn should_omit_the_model_key_on_turn_step_completed_without_attribution() {
    let event = SessionEvent::TurnStepCompleted {
        agent_id: "main".to_string(),
        turn_id: "turn-1".to_string(),
        step: 1,
        input_tokens: 10,
        output_tokens: 5,
        cache_read_tokens: 200,
        cache_creation_tokens: 0,
        model: None,
    };
    let projected = projected(&event);
    assert!(projected.payload.get("model").is_none());
}

#[test]
fn should_emit_usage_in_the_ts_client_shape_when_projecting_usage_events() {
    // Given a completed turn step and a completed subagent, each carrying
    // the engine's four raw token buckets (the counters the on-disk
    // `usage.record` keeps under the Rust spellings — ADR-012 pins those
    // on disk and assigns the TS client spellings to this projection layer)
    let step = SessionEvent::TurnStepCompleted {
        agent_id: "main".to_string(),
        turn_id: "turn-1".to_string(),
        step: 1,
        input_tokens: 10,
        output_tokens: 5,
        cache_read_tokens: 2,
        cache_creation_tokens: 1,
        model: None,
    };
    let subagent = SessionEvent::SubagentCompleted {
        subagent_id: "agent-1".to_string(),
        result_summary: "done".to_string(),
        input_tokens: 10,
        output_tokens: 5,
        cache_read_tokens: 2,
        cache_creation_tokens: 1,
    };

    // When wrapped in the client notification envelope
    let step_notification = event_projection::event_notification(
        &step,
        "session-1",
        Some(7),
        Some("epoch-1"),
        chrono::Utc::now(),
    )
    .expect("a completed step projects a notification");
    let subagent_notification = event_projection::event_notification(
        &subagent,
        "session-1",
        Some(8),
        Some("epoch-1"),
        chrono::Utc::now(),
    )
    .expect("a completed subagent projects a notification");

    // Then each `usage` object carries exactly the TS client TokenUsage
    // spellings — the keys the TS consumers read (kap-server's
    // `tokenUsageSchema` for `turn.step.completed`/`subagent.completed`
    // and the web agentEventProjector's `normalizeUsage`, which has no
    // `inputTokens` fallback)
    let expected = serde_json::json!({
        "inputOther": 10,
        "output": 5,
        "inputCacheRead": 2,
        "inputCacheCreation": 1,
    });
    assert_eq!(step_notification.method, "event.turn.step.completed");
    assert_eq!(step_notification.params["usage"], expected);
    assert_eq!(subagent_notification.method, "event.subagent.completed");
    assert_eq!(subagent_notification.params["usage"], expected);

    // And no on-disk Rust spelling leaks into the client shape
    for params in [&step_notification.params, &subagent_notification.params] {
        for rust_spelling in [
            "inputTokens",
            "outputTokens",
            "cacheReadTokens",
            "cacheCreationTokens",
        ] {
            assert!(
                params["usage"].get(rust_spelling).is_none(),
                "the client usage shape must not carry the on-disk key `{rust_spelling}`"
            );
        }
    }

    // And the envelope carries the client-visible timestamp (the epoch-ms
    // `time` of the on-disk usage.record row stays on disk — ADR-011;
    // the client event contract carries no `model`/`usageScope` keys)
    assert!(
        step_notification.params["timestamp"]
            .as_str()
            .is_some_and(|stamp| !stamp.is_empty()),
        "the notification envelope carries an RFC 3339 timestamp"
    );
}

#[test]
fn should_project_step_interruption_and_retry_with_the_wire_turn_id_when_carried() {
    // Given an interrupted and a retrying step, both carrying the wire turn
    // id the relay callback captured at dispatch
    let interrupted = SessionEvent::TurnStepInterrupted {
        agent_id: "main".to_string(),
        turn_id: Some("turn-1".to_string()),
        step: 2,
        reason: "aborted".to_string(),
        message: Some("cancelled by user".to_string()),
    };
    let retrying = SessionEvent::TurnStepRetrying {
        agent_id: "main".to_string(),
        turn_id: Some("turn-1".to_string()),
        step: 2,
        failed_attempt: 1,
        next_attempt: 2,
        max_attempts: 3,
        delay_ms: 0,
    };

    // When projecting both
    let interrupted_projected = projected(&interrupted);
    let retrying_projected = projected(&retrying);

    // Then the protocol-required `turnId` is present on the wire (the
    // pre-alignment spelling omitted it)
    assert_eq!(interrupted_projected.method, "event.turn.step.interrupted");
    assert_eq!(interrupted_projected.payload["turnId"], "turn-1");
    assert_eq!(interrupted_projected.payload["reason"], "aborted");
    assert_eq!(retrying_projected.method, "event.turn.step.retrying");
    assert_eq!(retrying_projected.payload["turnId"], "turn-1");
    assert_eq!(retrying_projected.payload["failedAttempt"], 1);

    // And a callback built without a turn id still projects — the field
    // stays absent rather than null (the optional spelling)
    let bare = SessionEvent::TurnStepInterrupted {
        agent_id: "main".to_string(),
        turn_id: None,
        step: 2,
        reason: "aborted".to_string(),
        message: None,
    };
    let bare_projected = projected(&bare);
    assert!(bare_projected.payload.get("turnId").is_none());
}

/// Given the four step-boundary events a subagent's flow publishes, when
/// each is projected, then every wire payload carries the agent's string
/// identity under `agentId` — the same routing key `event.turn.started`
/// carries, so the web segment router can attribute step boundaries to
/// the owning agent's timeline (ADR-015's "boundary events carry the agent
/// identity" made to hold on the step family too).
#[test]
fn should_carry_the_agent_id_on_every_step_boundary_payload_when_projected() {
    // Given a subagent's step boundaries (the agent-scoped id shape)
    let events = [
        SessionEvent::TurnStepStarted {
            agent_id: "agent-42".to_string(),
            turn_id: "turn-1".to_string(),
            step: 1,
        },
        SessionEvent::TurnStepCompleted {
            agent_id: "agent-42".to_string(),
            turn_id: "turn-1".to_string(),
            step: 1,
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 2,
            cache_creation_tokens: 1,
            model: None,
        },
        SessionEvent::TurnStepInterrupted {
            agent_id: "agent-42".to_string(),
            turn_id: Some("turn-1".to_string()),
            step: 2,
            reason: "aborted".to_string(),
            message: None,
        },
        SessionEvent::TurnStepRetrying {
            agent_id: "agent-42".to_string(),
            turn_id: Some("turn-1".to_string()),
            step: 2,
            failed_attempt: 1,
            next_attempt: 2,
            max_attempts: 3,
            delay_ms: 0,
        },
    ];

    // When projecting each
    for event in &events {
        let projected = projected(event);
        // Then the payload carries the agent's identity — not null, not
        // absent (the routing key is required on the step family)
        assert_eq!(
            projected.payload["agentId"], "agent-42",
            "the `{}` payload must carry the agent id",
            projected.method
        );
    }

    // And the main agent's shape carries the reserved `"main"` spelling
    let main_started = SessionEvent::TurnStepStarted {
        agent_id: "main".to_string(),
        turn_id: "turn-1".to_string(),
        step: 1,
    };
    let main_projected = projected(&main_started);
    assert_eq!(main_projected.payload["agentId"], "main");
}

#[test]
fn should_project_tool_progress_to_event_tool_progress_when_event_is_tool_progress() {
    // Given a tool_progress event carrying a percent update
    let event = SessionEvent::ToolProgress {
        agent_id: "main".to_string(),
        tool_call_id: "call-1".to_string(),
        update: mirri_core::tool::ToolUpdate {
            kind: mirri_core::tool::ToolUpdateKind::Progress,
            text: Some("halfway".to_string()),
            percent: Some(50.0),
            custom_kind: None,
            custom_data: None,
        },
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.tool.progress — volatile per the
    // protocol's VOLATILE_EVENT_TYPES (no seq stamp, never replayed) — with
    // the update under `update` and no payload sessionId or turnId
    assert_eq!(projected.method, "event.tool.progress");
    assert!(projected.volatile);
    assert_eq!(projected.payload["toolCallId"], "call-1");
    assert_eq!(projected.payload["update"]["kind"], "progress");
    assert_eq!(projected.payload["update"]["text"], "halfway");
    assert_eq!(projected.payload["update"]["percent"], 50.0);
    assert!(projected.payload.get("sessionId").is_none());
    assert!(projected.payload.get("turnId").is_none());
    assert!(projected.payload["update"].get("customKind").is_none());
}

/// A [`mirri_core::UsageStatus`] fixture with one model bucket, shared by
/// the agent status projection tests.
fn test_usage_status() -> mirri_core::UsageStatus {
    use mirri_core::usage::TokenUsage;
    use std::collections::HashMap;

    let tokens = TokenUsage {
        input_tokens: 100,
        output_tokens: 20,
        cache_read_tokens: 5,
        cache_creation_tokens: 1,
    };
    mirri_core::UsageStatus {
        by_model: HashMap::from([("kimi-k2".to_string(), tokens.clone())]),
        current_turn: tokens.clone(),
        total: tokens,
        first_call: None,
    }
}

#[test]
fn should_project_agent_status_updated_with_structured_payload() {
    // Given an agent_status_updated event carrying the full structured
    // snapshot (phase, model, context, plan/swarm flags, usage)
    use mirri_core::session::agent_status::AgentPhase;

    let event = SessionEvent::AgentStatusUpdated {
        agent_id: "main".to_string(),
        phase: AgentPhase::Running,
        model: Some("kimi-k2".to_string()),
        thinking_effort: None,
        context_tokens: Some(120_000),
        max_context_tokens: Some(200_000),
        plan_mode: Some(false),
        swarm_mode: Some(false),
        usage: Some(test_usage_status()),
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.agent.status.updated with the
    // structured camelCase payload — key-by-key the protocol
    // AgentStatusUpdatedEvent shape — and it is volatile, aligned with
    // the protocol's VOLATILE_EVENT_TYPES
    assert_eq!(projected.method, "event.agent.status.updated");
    assert!(projected.volatile);
    assert_eq!(projected.payload["agentId"], serde_json::json!("main"));
    assert_eq!(projected.payload["phase"], serde_json::json!("running"));
    assert_eq!(projected.payload["model"], serde_json::json!("kimi-k2"));
    assert_eq!(
        projected.payload["contextTokens"],
        serde_json::json!(120_000)
    );
    assert_eq!(
        projected.payload["maxContextTokens"],
        serde_json::json!(200_000)
    );
    assert_eq!(projected.payload["contextUsage"], serde_json::json!(0.6));
    assert_eq!(projected.payload["planMode"], serde_json::json!(false));
    assert_eq!(projected.payload["swarmMode"], serde_json::json!(false));
    assert!(
        projected.payload.get("thinkingEffort").is_none(),
        "an absent optional key must not appear as null"
    );
    assert!(
        projected.payload.get("previousStatus").is_none(),
        "the TS v2 slice shape carries no previousStatus key"
    );

    // And the usage object uses the TS TokenUsage bucket spelling
    // (inputOther/output/inputCacheRead/inputCacheCreation — the same
    // dialect event.turn.step.completed's usage object speaks)
    assert_eq!(
        projected.payload["usage"]["total"]["inputOther"],
        serde_json::json!(100)
    );
    assert_eq!(
        projected.payload["usage"]["byModel"]["kimi-k2"]["output"],
        serde_json::json!(20)
    );
    assert_eq!(
        projected.payload["usage"]["currentTurn"]["inputCacheRead"],
        serde_json::json!(5)
    );
}

#[test]
fn should_project_a_custom_thinking_effort_verbatim_when_agent_status_updated() {
    // Given an agent_status_updated event carrying a model-declared custom
    // effort (the open vocabulary's escape hatch — not one of the four
    // canonical levels)
    use mirri_core::session::agent_status::AgentPhase;

    let event = SessionEvent::AgentStatusUpdated {
        agent_id: "main".to_string(),
        phase: AgentPhase::Running,
        model: Some("kimi-k2".to_string()),
        thinking_effort: Some(mirri_core::ThinkingEffort::Custom("max".to_string())),
        context_tokens: None,
        max_context_tokens: None,
        plan_mode: None,
        swarm_mode: None,
        usage: None,
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event carries the custom spelling verbatim as
    // thinkingEffort — never folded to a canonical level nor dropped
    assert_eq!(projected.method, "event.agent.status.updated");
    assert_eq!(
        projected.payload["thinkingEffort"],
        serde_json::json!("max")
    );
}

#[test]
fn should_omit_optional_status_fields_entirely_when_absent() {
    // Given an agent_status_updated event with only the identity and the
    // phase (every optional field absent)
    use mirri_core::session::agent_status::AgentPhase;

    let event = SessionEvent::AgentStatusUpdated {
        agent_id: "main".to_string(),
        phase: AgentPhase::Idle,
        model: None,
        thinking_effort: None,
        context_tokens: None,
        max_context_tokens: None,
        plan_mode: None,
        swarm_mode: None,
        usage: None,
    };

    // When projecting it
    let projected = projected(&event);

    // Then the payload carries only agentId and phase — no null keys, no
    // status/previousStatus debug-placeholder residue
    assert_eq!(projected.method, "event.agent.status.updated");
    let object = projected
        .payload
        .as_object()
        .expect("the payload is a JSON object");
    assert_eq!(
        object.len(),
        2,
        "only agentId and phase may be present, got {object:?}"
    );
    assert_eq!(projected.payload["agentId"], serde_json::json!("main"));
    assert_eq!(projected.payload["phase"], serde_json::json!("idle"));
}

#[test]
fn should_project_tool_approval_requested_to_event_approval_requested_when_event_is_tool_approval_requested()
 {
    // Given a tool_approval_requested event
    let request_id = Uuid::new_v4();
    let event = SessionEvent::ToolApprovalRequested {
        agent_id: Uuid::nil(),
        tool_name: "bash".to_string(),
        request_id,
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.approval.requested with the request payload
    assert_eq!(projected.method, "event.approval.requested");
    assert!(!projected.volatile);
    assert_eq!(projected.payload["agentId"], serde_json::json!(Uuid::nil()));
    assert_eq!(projected.payload["toolName"], "bash");
    assert_eq!(
        projected.payload["requestId"],
        serde_json::json!(request_id)
    );
}

#[test]
fn should_project_tool_approval_resolved_to_event_approval_resolved_when_event_is_tool_approval_resolved()
 {
    // Given a tool_approval_resolved event
    let event = SessionEvent::ToolApprovalResolved {
        request_id: Uuid::new_v4(),
        approved: false,
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.approval.resolved with the outcome
    assert_eq!(projected.method, "event.approval.resolved");
    assert!(!projected.volatile);
    assert!(projected.payload["requestId"].is_string());
    assert_eq!(projected.payload["approved"], false);
}

#[test]
fn should_project_question_asked_to_event_question_requested_when_event_is_question_asked() {
    // Given a question_asked event
    let request_id = Uuid::new_v4();
    let event = SessionEvent::QuestionAsked {
        agent_id: Uuid::nil(),
        questions: serde_json::json!([{ "question": "Deploy now?" }]),
        request_id,
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.question.requested with the question
    assert_eq!(projected.method, "event.question.requested");
    assert!(!projected.volatile);
    assert_eq!(projected.payload["agentId"], serde_json::json!(Uuid::nil()));
    assert_eq!(
        projected.payload["requestId"],
        serde_json::json!(request_id)
    );
    assert_eq!(projected.payload["question"], "Deploy now?");
}

#[test]
fn should_carry_every_option_when_projecting_a_question_asked_event() {
    // Given a question_asked event carrying the tool's full item payload
    let request_id = Uuid::new_v4();
    let event = SessionEvent::QuestionAsked {
        agent_id: Uuid::nil(),
        questions: serde_json::json!([{
            "question": "这批本地变更要开成一个 PR，还是按主题拆分？",
            "header": "PR 结构",
            "options": [
                { "label": "一个 PR，分组描述", "description": "改动高度交叉" },
                { "label": "按主题拆 4-5 个 PR" }
            ]
        }]),
        request_id,
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire payload keeps the summary string AND the full items,
    // so a client that never saw the reverse-RPC ask can still render the
    // options instead of degrading to an options-less card
    assert_eq!(
        projected.payload["question"],
        "这批本地变更要开成一个 PR，还是按主题拆分？"
    );
    let items = projected.payload["questions"]
        .as_array()
        .expect("questions is an array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["header"], "PR 结构");
    let options = items[0]["options"].as_array().expect("options is an array");
    assert_eq!(options.len(), 2);
    assert_eq!(options[0]["label"], "一个 PR，分组描述");
    assert_eq!(options[0]["description"], "改动高度交叉");
    // The option id is the label: the engine feeds the answer string back to
    // the model verbatim, so the label must survive as the id
    assert_eq!(options[1]["id"], "按主题拆 4-5 个 PR");
}

#[test]
fn should_project_question_answered_to_event_question_answered_when_event_is_question_answered() {
    // Given a question_answered event
    let event = SessionEvent::QuestionAnswered {
        request_id: Uuid::new_v4(),
        answer: "yes".to_string(),
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.question.answered with the answer
    assert_eq!(projected.method, "event.question.answered");
    assert!(!projected.volatile);
    assert!(projected.payload["requestId"].is_string());
    assert_eq!(projected.payload["answer"], "yes");
}

#[test]
fn should_project_question_dismissed_to_event_question_dismissed_when_event_is_question_dismissed()
{
    // Given a question_dismissed event
    let request_id = Uuid::new_v4();
    let event = SessionEvent::QuestionDismissed { request_id };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.question.dismissed (durable, so clients
    // that reconnect learn the question was cancelled) carrying only the
    // requestId — no fabricated dismissal metadata
    assert_eq!(projected.method, "event.question.dismissed");
    assert!(!projected.volatile);
    assert_eq!(
        projected.payload["requestId"],
        serde_json::json!(request_id)
    );
    let keys: Vec<&str> = projected
        .payload
        .as_object()
        .expect("payload is an object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["requestId"]);
}

#[test]
fn should_project_session_resumed_and_forked_to_event_session_updated_when_session_changes() {
    // Given resume and fork events
    let new_session_id = Uuid::new_v4();
    let resumed = SessionEvent::SessionResumed {
        session_id: Uuid::nil(),
    };
    let forked = SessionEvent::SessionForked {
        source_session_id: Uuid::nil(),
        new_session_id,
    };

    // When projecting them
    let resumed_projected = projected(&resumed);
    let forked_projected = projected(&forked);

    // Then both map to event.session.updated with their change markers;
    // the fork's new session rides under `newSessionId` so it never
    // collides with the envelope's `sessionId`
    assert_eq!(resumed_projected.method, "event.session.updated");
    assert_eq!(resumed_projected.payload["change"], "resumed");
    assert_eq!(forked_projected.method, "event.session.updated");
    assert_eq!(forked_projected.payload["change"], "forked");
    assert_eq!(
        forked_projected.payload["sourceSessionId"],
        serde_json::json!(Uuid::nil())
    );
    assert_eq!(
        forked_projected.payload["newSessionId"],
        serde_json::json!(new_session_id)
    );
    assert!(
        forked_projected.payload.get("sessionId").is_none(),
        "the fork payload must not use the envelope's reserved sessionId key"
    );
}

#[test]
fn should_project_session_closed_to_event_session_deleted_when_event_is_session_closed() {
    // Given a session_closed event
    let event = SessionEvent::SessionClosed {
        session_id: Uuid::nil(),
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.session.deleted with the closure reason
    // (hardcoded `user_initiated` until the reason vocabulary widens, F3)
    assert_eq!(projected.method, "event.session.deleted");
    assert!(!projected.volatile);
    assert_eq!(projected.payload["reason"], "user_initiated");
}

#[test]
fn should_project_agent_created_to_event_agent_created_when_event_is_agent_created() {
    // Given an agent_created event
    let event = SessionEvent::AgentCreated {
        agent_id: Uuid::nil(),
        session_id: Uuid::nil(),
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.agent.created with only the agent id —
    // the session identity lives in the envelope
    assert_eq!(projected.method, "event.agent.created");
    assert!(!projected.volatile);
    assert_eq!(projected.payload["agentId"], serde_json::json!(Uuid::nil()));
    assert!(projected.payload.get("sessionId").is_none());
}

#[test]
fn should_project_agent_removed_to_event_agent_removed_when_event_is_agent_removed() {
    // Given an agent_removed event
    let event = SessionEvent::AgentRemoved {
        agent_id: Uuid::nil(),
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.agent.removed with the agent id
    assert_eq!(projected.method, "event.agent.removed");
    assert!(!projected.volatile);
    assert_eq!(projected.payload["agentId"], serde_json::json!(Uuid::nil()));
}

#[test]
fn should_return_none_when_event_is_unknown() {
    // Given an Unknown event
    let event = SessionEvent::Unknown;

    // When projecting it
    let projection = event_projection::project(&event);

    // Then there is no wire projection
    assert!(projection.is_none());
}

#[test]
fn should_cover_the_named_variants_with_forty_six_wire_methods_when_enumerated() {
    // Given every named SessionEvent variant
    let variants = all_variants();

    // When collecting the distinct methods they project onto
    let mut methods: Vec<&str> = variants
        .iter()
        .filter_map(|event| event_projection::project(event).map(|p| p.method))
        .collect();
    methods.sort_unstable();
    methods.dedup();

    // Then exactly the forty-five protocol methods are covered (the
    // variant count minus Unknown, with turn completed/failed and session
    // resumed/forked sharing their terminal methods)
    assert_eq!(
        methods,
        [
            "event.agent.created",
            "event.agent.removed",
            "event.agent.status.updated",
            "event.approval.requested",
            "event.approval.resolved",
            "event.assistant.delta",
            "event.background.task.started",
            "event.background.task.terminated",
            "event.compaction.blocked",
            "event.compaction.cancelled",
            "event.compaction.completed",
            "event.compaction.started",
            "event.cron.fired",
            "event.goal.updated",
            "event.hook.result",
            "event.mcp.server.status",
            "event.message.received",
            "event.plugin_command.activated",
            "event.question.answered",
            "event.question.dismissed",
            "event.question.requested",
            "event.session.deleted",
            "event.session.meta.updated",
            "event.session.model_switched",
            "event.session.updated",
            "event.session.work_changed",
            "event.shell.completed",
            "event.shell.output",
            "event.shell.started",
            "event.skill.activated",
            "event.subagent.completed",
            "event.subagent.failed",
            "event.subagent.spawned",
            "event.subagent.started",
            "event.subagent.suspended",
            "event.thinking.delta",
            "event.tool.call.delta",
            "event.tool.call.started",
            "event.tool.progress",
            "event.tool.result",
            "event.turn.ended",
            "event.turn.started",
            "event.turn.step.completed",
            "event.turn.step.interrupted",
            "event.turn.step.retrying",
            "event.turn.step.started",
        ]
    );
}

#[test]
fn should_project_the_variant_set_onto_exactly_the_projected_event_methods() {
    // Given one instance of every SessionEvent variant
    let variants = all_variants();

    // When projecting each (Unknown is the only variant without a
    // projection — asserted inline) and collecting the distinct methods
    let mut methods: Vec<&str> = Vec::new();
    for event in &variants {
        let projection = event_projection::project(event);
        assert_eq!(
            projection.is_none(),
            matches!(event, SessionEvent::Unknown),
            "only Unknown lacks a wire projection, got {event:?}"
        );
        if let Some(projected) = projection {
            methods.push(projected.method);
        }
    }
    methods.sort_unstable();
    methods.dedup();

    // Then the method set equals the exported closed vocabulary exactly — a
    // variant added without a PROJECTED_EVENT_METHODS entry (or a stale entry
    // without a projecting variant) fails here instead of drifting the
    // contract the TS suite reconciles against
    assert_eq!(methods, event_projection::PROJECTED_EVENT_METHODS.to_vec());
}

#[test]
fn should_keep_is_volatile_consistent_with_the_projection_for_every_variant() {
    // Given every SessionEvent variant
    let variants = all_variants();

    for event in &variants {
        // When asking whether it is volatile and projecting it
        // Then the two agree (Unknown is volatile and unprojected)
        assert_eq!(
            event_projection::is_volatile(event),
            event_projection::project(event).is_none_or(|p| p.volatile),
            "volatility disagrees with the projection for {event:?}"
        );
    }
}

#[test]
fn should_keep_payload_keys_off_the_reserved_envelope_keys_for_every_projected_event() {
    // Given every SessionEvent variant with a wire projection
    // (Unknown has none, so it is excluded from the enumeration)
    let variants: Vec<SessionEvent> = all_variants()
        .into_iter()
        .filter(|event| !matches!(event, SessionEvent::Unknown))
        .collect();

    for event in &variants {
        // When projecting it
        let projected = projected(event);

        // Then its payload keys never collide with the reserved envelope
        // keys (payloads are merged flat into the notification params)
        if let serde_json::Value::Object(payload) = &projected.payload {
            for key in payload.keys() {
                assert!(
                    !RESERVED_ENVELOPE_KEYS.contains(&key.as_str()),
                    "payload key {key} of {} collides with a reserved envelope key",
                    projected.method
                );
            }
        } else {
            panic!("payload of {} is not an object", projected.method);
        }
    }
}

#[test]
fn should_stamp_seq_and_epoch_on_durable_events_only_when_building_notifications() {
    // Given every SessionEvent variant with a wire projection, a seq, an
    // epoch, and a timestamp (Unknown has none, so it is excluded)
    let variants: Vec<SessionEvent> = all_variants()
        .into_iter()
        .filter(|event| !matches!(event, SessionEvent::Unknown))
        .collect();
    let timestamp = chrono::Utc::now();

    for event in &variants {
        // When building the notification envelope (the seq/epoch supplied
        // even for volatile events, as the server's forwarder does)
        let notification = event_projection::event_notification(
            event,
            "session-1",
            Some(42),
            Some("epoch-1"),
            timestamp,
        )
        .expect("projected event builds a notification");

        // Then the envelope always carries the session, volatility, and
        // timestamp...
        let params = &notification.params;
        assert_eq!(params["sessionId"], "session-1");
        assert_eq!(
            params["volatile"],
            event_projection::is_volatile(event),
            "envelope volatility matches the projection for {event:?}"
        );
        assert!(
            params["timestamp"]
                .as_str()
                .is_some_and(|stamp| !stamp.is_empty()),
            "the envelope carries an RFC 3339 timestamp"
        );

        // ...and seq/epoch appear on durable events only, even though the
        // caller supplied them for the volatile ones too
        if event_projection::is_volatile(event) {
            assert!(
                params.get("seq").is_none(),
                "volatile event {event:?} must not carry a seq"
            );
            assert!(
                params.get("epoch").is_none(),
                "volatile event {event:?} must not carry an epoch"
            );
        } else {
            assert_eq!(params["seq"], 42, "durable event {event:?} carries its seq");
            assert_eq!(params["epoch"], "epoch-1");
        }
    }
}

#[test]
fn should_return_none_when_building_a_notification_for_an_unknown_event() {
    // Given an Unknown event
    let event = SessionEvent::Unknown;

    // When building the notification envelope
    let notification = event_projection::event_notification(
        &event,
        "session-1",
        Some(42),
        Some("epoch-1"),
        chrono::Utc::now(),
    );

    // Then there is no notification to frame
    assert!(notification.is_none());
}

#[test]
fn should_map_each_turn_stop_reason_to_its_wire_string_when_asked() {
    // Given the seven turn stop reasons
    let cases = [
        (TurnStopReason::EndTurn, "completed"),
        (TurnStopReason::MaxTokens, "completed"),
        (TurnStopReason::Filtered, "filtered"),
        (TurnStopReason::Paused, "completed"),
        (TurnStopReason::Unknown, "completed"),
        (TurnStopReason::Aborted, "cancelled"),
        (TurnStopReason::BlockedByHook, "blocked"),
    ];

    for (reason, expected) in cases {
        // When mapping it to the wire string
        // Then it matches the TS kernel's turnStopReasonToWire vocabulary
        assert_eq!(
            event_projection::turn_stop_reason_wire(&reason),
            expected,
            "wire string for {reason:?}"
        );
    }
}

// --- background.task projection (design §7.8) -----------------------------

#[test]
fn should_project_background_task_started_to_event_background_task_started_with_info() {
    // Given a running background task's started event with a process-kind info
    let event = SessionEvent::BackgroundTaskStarted {
        info: BackgroundTaskInfo {
            kind: TaskKind::Process {
                command: Some("cargo build".to_string()),
                dispatcher: None,
            },
            task_id: "task-1".to_string(),
            description: "release build".to_string(),
            status: "running".to_string(),
            started_at: 1_700_000_000_000_i64,
            ended_at: None,
            stop_reason: None,
            agent_id: None,
            exit_code: None,
        },
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.background.task.started, durable, with
    // the info snapshot nested verbatim (the TS protocol's discriminated
    // union — kind discriminator in-band, optional fields absent)
    assert_eq!(projected.method, "event.background.task.started");
    assert!(!projected.volatile);
    assert_eq!(projected.payload["info"]["taskId"], "task-1");
    assert_eq!(projected.payload["info"]["kind"], "process");
    assert_eq!(projected.payload["info"]["command"], "cargo build");
    assert_eq!(projected.payload["info"]["status"], "running");
    assert_eq!(
        projected.payload["info"]["startedAt"],
        1_700_000_000_000_i64
    );
    assert_eq!(
        projected.payload["info"]["endedAt"],
        serde_json::Value::Null,
        "the required-nullable endedAt rides the started spelling as null"
    );
    assert!(projected.payload["info"].get("exitCode").is_none());
}

#[test]
fn should_project_background_task_terminated_with_terminal_info_when_event_is_terminated() {
    // Given a completed process task's terminated event
    let event = SessionEvent::BackgroundTaskTerminated {
        info: BackgroundTaskInfo {
            kind: TaskKind::Process {
                command: Some("cargo build".to_string()),
                dispatcher: None,
            },
            task_id: "task-1".to_string(),
            description: "release build".to_string(),
            status: "completed".to_string(),
            started_at: 1_700_000_000_000_i64,
            ended_at: Some(1_700_000_001_500_i64),
            stop_reason: None,
            agent_id: None,
            exit_code: Some(0),
        },
    };

    // When projecting it
    let projected = projected(&event);

    // Then the wire event is event.background.task.terminated, durable,
    // carrying the terminal snapshot (endedAt + exitCode present on the
    // completed process branch)
    assert_eq!(projected.method, "event.background.task.terminated");
    assert!(!projected.volatile);
    assert_eq!(projected.payload["info"]["status"], "completed");
    assert_eq!(projected.payload["info"]["endedAt"], 1_700_000_001_500_i64);
    assert_eq!(projected.payload["info"]["exitCode"], 0);
}

#[test]
fn should_project_the_shell_started_event_as_a_volatile_wire_event() {
    // Given a shell.started event carrying the command and task ids
    let event = SessionEvent::ShellStarted {
        command_id: "shell-1".to_string(),
        task_id: "task-3".to_string(),
    };

    // When projecting it
    let projected = event_projection::project(&event).expect("shell.started projects");

    // Then the wire method, volatility (the protocol's VOLATILE_EVENT_TYPES
    // lists shell.started), and camelCase payload match the TS shape
    assert_eq!(projected.method, "event.shell.started");
    assert!(projected.volatile);
    assert_eq!(projected.payload["commandId"], "shell-1");
    assert_eq!(projected.payload["taskId"], "task-3");
    assert!(event_projection::is_volatile(&event));
}

#[test]
fn should_project_the_shell_output_event_as_a_volatile_wire_event() {
    // Given a shell.output event streaming one stdout chunk
    let event = SessionEvent::ShellOutput {
        command_id: "shell-1".to_string(),
        update: mirri_core::tool::ToolUpdate {
            kind: mirri_core::tool::ToolUpdateKind::Stdout,
            text: Some("chunk".to_string()),
            percent: None,
            custom_kind: None,
            custom_data: None,
        },
        task_id: Some("task-3".to_string()),
    };

    // When projecting it
    let projected = event_projection::project(&event).expect("shell.output projects");

    // Then the chunk rides the camelCase ToolUpdate shape and the optional
    // task id stays present (never a null placeholder when absent)
    assert_eq!(projected.method, "event.shell.output");
    assert!(projected.volatile);
    assert_eq!(projected.payload["commandId"], "shell-1");
    assert_eq!(projected.payload["taskId"], "task-3");
    assert_eq!(projected.payload["update"]["kind"], "stdout");
    assert_eq!(projected.payload["update"]["text"], "chunk");
    assert!(event_projection::is_volatile(&event));

    // And the id-less spelling omits taskId instead of nulling it
    let idless = SessionEvent::ShellOutput {
        command_id: "shell-1".to_string(),
        update: mirri_core::tool::ToolUpdate {
            kind: mirri_core::tool::ToolUpdateKind::Stderr,
            text: Some("boom".to_string()),
            percent: None,
            custom_kind: None,
            custom_data: None,
        },
        task_id: None,
    };
    let projected = event_projection::project(&idless).expect("id-less shell.output projects");
    assert!(projected.payload.get("taskId").is_none());
}

#[test]
fn should_project_the_shell_completed_event_as_a_durable_wire_event() {
    // Given a shell.completed event settling a failed foreground run
    let event = SessionEvent::ShellCompleted {
        command_id: "shell-1".to_string(),
        is_error: true,
        task_id: None,
    };

    // When projecting it
    let projected = event_projection::project(&event).expect("shell.completed projects");

    // Then the settlement is durable (only the streaming pair is volatile)
    // and carries the TS camelCase payload
    assert_eq!(projected.method, "event.shell.completed");
    assert!(!projected.volatile);
    assert_eq!(projected.payload["commandId"], "shell-1");
    assert_eq!(projected.payload["isError"], true);
    assert!(projected.payload.get("taskId").is_none());
    assert!(!event_projection::is_volatile(&event));
}

#[test]
fn should_project_a_model_switched_event_with_from_and_to_payload() {
    // Given a ModelSwitched session event
    let event = SessionEvent::ModelSwitched {
        from: "kimi-k2".to_string(),
        to: "claude-sonnet-4".to_string(),
    };

    // When projecting it
    let projected = projected(&event);

    // Then the method carries the switch identity and the camelCase payload,
    // durable like the other session-scoped lifecycle events
    assert_eq!(projected.method, "event.session.model_switched");
    assert!(!projected.volatile);
    assert_eq!(projected.payload["from"], "kimi-k2");
    assert_eq!(projected.payload["to"], "claude-sonnet-4");
}

#[test]
fn should_preserve_info_when_background_task_events_roundtrip_through_the_session_event() {
    // Given the two background task lifecycle events carrying terminated
    // process- and agent-kind infos (the flatten + internally-tagged kind
    // combination the ws relay's `from_value::<SessionEvent>` consumers
    // deserialize)
    let events = [
        SessionEvent::BackgroundTaskStarted {
            info: BackgroundTaskInfo {
                kind: TaskKind::Process {
                    command: Some("cargo build".to_string()),
                    dispatcher: None,
                },
                task_id: "task-1".to_string(),
                description: "release build".to_string(),
                status: "running".to_string(),
                started_at: 1_700_000_000_000_i64,
                ended_at: None,
                stop_reason: None,
                agent_id: None,
                exit_code: None,
            },
        },
        SessionEvent::BackgroundTaskTerminated {
            info: BackgroundTaskInfo {
                kind: TaskKind::Agent {
                    agent_id: Some("agent-1".to_string()),
                    subagent_type: Some("coder".to_string()),
                    parent_tool_call_id: None,
                    swarm_index: None,
                    model: None,
                },
                task_id: "task-2".to_string(),
                description: "subagent".to_string(),
                status: "failed".to_string(),
                started_at: 1_700_000_000_000_i64,
                ended_at: Some(1_700_000_001_500_i64),
                stop_reason: Some("boom".to_string()),
                agent_id: None,
                exit_code: None,
            },
        },
    ];

    for event in events {
        // When serializing and deserializing
        let json = serde_json::to_string(&event).unwrap();
        let roundtrip: SessionEvent = serde_json::from_str(&json).unwrap();

        // Then the tagged variant and its info snapshot survive the
        // roundtrip field-for-field
        match (event, roundtrip) {
            (
                SessionEvent::BackgroundTaskStarted { info },
                SessionEvent::BackgroundTaskStarted { info: received },
            )
            | (
                SessionEvent::BackgroundTaskTerminated { info },
                SessionEvent::BackgroundTaskTerminated { info: received },
            ) => assert_eq!(received, info),
            (original, _) => panic!("variant changed across the roundtrip: {original:?}"),
        }
    }
}

#[test]
fn should_preserve_identity_when_a_tool_call_delta_roundtrips_through_the_session_event() {
    // Given a tool_call_delta carrying the optional tool name (the wire
    // relay's `from_value::<SessionEvent>` consumers deserialize this shape)
    let event = SessionEvent::ToolCallDelta {
        agent_id: "main".to_string(),
        tool_call_id: "call_1".to_string(),
        tool_name: Some("bash".to_string()),
        arguments_part: "{\"a\":".to_string(),
    };

    // When serializing and deserializing
    let json = serde_json::to_string(&event).unwrap();
    let roundtrip: SessionEvent = serde_json::from_str(&json).unwrap();

    // Then the tagged variant and its identity fields survive field-for-field
    match roundtrip {
        SessionEvent::ToolCallDelta {
            tool_call_id,
            tool_name,
            arguments_part,
            ..
        } => {
            assert_eq!(tool_call_id, "call_1");
            assert_eq!(tool_name.as_deref(), Some("bash"));
            assert_eq!(arguments_part, "{\"a\":");
        }
        other => panic!("variant changed across the roundtrip: {other:?}"),
    }
}

// ===========================================================================
// Subagent suspension — structured reason (ADR-011)
// ===========================================================================

/// Given a member suspension carrying the minted approval-gate ids, When it
/// is projected, Then the wire reason is the structured snake_case object
/// `{ kind: "approval", request_id, tool_call_id }` (ADR-011) — machine
/// readable for consumers that render the member's awaiting-user state.
#[test]
fn should_project_a_structured_approval_suspension_when_a_member_awaits_an_approval() {
    // Given a member suspension with a minted approval request
    let event = SessionEvent::SubagentSuspended {
        subagent_id: "agent-1".to_string(),
        reason: SuspendCause::Approval {
            request_id: "req-1".to_string(),
            tool_call_id: Some("call-bash".to_string()),
        },
    };

    // When projected onto the wire event
    let projected = projected(&event);

    // Then the payload carries the structured snake_case reason
    assert_eq!(projected.method, "event.subagent.suspended");
    assert!(!projected.volatile);
    assert_eq!(projected.payload["subagentId"], "agent-1");
    assert_eq!(
        projected.payload["reason"],
        serde_json::json!({
            "kind": "approval",
            "request_id": "req-1",
            "tool_call_id": "call-bash",
        })
    );
}

/// Given a rate-limit suspension, When it is projected, Then the wire
/// reason is the machine-readable `{ "kind": "rate_limit" }` object — the
/// legacy free-form string no longer appears on new suspensions.
#[test]
fn should_keep_rate_limit_suspension_reason_machine_readable() {
    // Given a rate-limit suspension
    let event = SessionEvent::SubagentSuspended {
        subagent_id: "agent-2".to_string(),
        reason: SuspendCause::RateLimit,
    };

    // When projected onto the wire event
    let projected = projected(&event);

    // Then the reason is the structured rate_limit object
    assert_eq!(projected.method, "event.subagent.suspended");
    assert_eq!(
        projected.payload["reason"],
        serde_json::json!({ "kind": "rate_limit" })
    );
}

/// Given a pre-ADR-011 journal row whose suspension reason is a free-form
/// string, When it is deserialized and projected, Then it stays receivable:
/// deserialization maps it to the `Legacy` form and the projection passes
/// the string through unchanged (old consumers discriminate with `typeof`;
/// nothing crashes).
#[test]
fn should_keep_legacy_string_suspension_reasons_receivable_when_replayed() {
    // Given an old journal row carrying the free-form reason
    let json = serde_json::json!({
        "type": "subagent_suspended",
        "subagent_id": "agent-3",
        "reason": "Provider rate limit; subagent requeued for retry.",
    });

    // When it is deserialized and projected
    let event: SessionEvent = serde_json::from_value(json).unwrap();
    let projected = projected(&event);

    // Then the reason survives as the original string — new consumers must
    // not crash on the legacy shape
    assert_eq!(
        projected.payload["reason"],
        "Provider rate limit; subagent requeued for retry."
    );
}

/// Given a suspension reason in an unknown shape, When the event is
/// deserialized, Then it does not crash — the shape is preserved as a
/// legacy string instead (the projection layer's unknown-shape pass-through
/// for the compatibility window).
#[test]
fn should_not_crash_on_unknown_suspension_reason_shapes() {
    // Given a reason that is neither a string nor a known structured kind
    let json = serde_json::json!({
        "type": "subagent_suspended",
        "subagent_id": "agent-4",
        "reason": { "kind": "future_kind", "detail": 7 },
    });

    // When it is deserialized
    let event: SessionEvent = serde_json::from_value(json).unwrap();

    // Then the event is preserved with a legacy fallback reason
    match event {
        SessionEvent::SubagentSuspended { reason, .. } => {
            assert!(matches!(reason, SuspendCause::Legacy(_)));
        }
        other => panic!("expected SubagentSuspended, got {other:?}"),
    }
}

/// Given a structured suspension reason, When the session event is
/// round-tripped through its serde, Then the shape survives verbatim
/// (journals written with the structured form replay it exactly).
#[test]
fn should_roundtrip_structured_suspension_reasons_through_the_session_event() {
    // Given a structured approval suspension event with an absent tool call
    // id (the optional facet omits — never a null placeholder)
    let event = SessionEvent::SubagentSuspended {
        subagent_id: "agent-5".to_string(),
        reason: SuspendCause::Approval {
            request_id: "req-2".to_string(),
            tool_call_id: None,
        },
    };

    // When serialized and deserialized
    let json = serde_json::to_string(&event).unwrap();
    let roundtrip: SessionEvent = serde_json::from_str(&json).unwrap();

    // Then the structured reason survives field-for-field
    match roundtrip {
        SessionEvent::SubagentSuspended {
            subagent_id,
            reason,
        } => {
            assert_eq!(subagent_id, "agent-5");
            assert_eq!(
                reason,
                SuspendCause::Approval {
                    request_id: "req-2".to_string(),
                    tool_call_id: None,
                }
            );
        }
        other => panic!("expected SubagentSuspended, got {other:?}"),
    }
}

// ===========================================================================
// AgentPhase derivation (Task 7, CR4)
// ===========================================================================

/// Port-fidelity tests for [`phase_from_activity`], the Rust port of the
/// kap-server TS function `toLegacyPhase`
/// (`packages/kap-server/src/services/legacyStatus/legacyStatus.ts:155-238`).
/// Each test builds an [`AgentActivityState`] by folding events onto a fresh
/// [`AgentActivityView`] — the same way the real fold produces snapshots.
mod agent_phase_derivation {
    use mirri_core::session::agent_status::{AgentPhase, phase_from_activity};
    use mirri_core::{
        ActivityRetryState, AgentActivityState, AgentActivityView, ApprovalRef, EndingReason,
        PromptOrigin, StreamKind, TurnEndReason,
    };

    /// Fold events onto a fresh view and return the resulting snapshot.
    fn snapshot(fold: impl FnOnce(&mut AgentActivityView)) -> AgentActivityState {
        let mut view = AgentActivityView::new();
        fold(&mut view);
        view.state().clone()
    }

    /// Given no active turn and no last-turn outcome, when deriving the
    /// phase, then it is `Idle`.
    #[test]
    fn should_derive_idle_when_no_turn_and_no_last_turn() {
        let state = snapshot(|_| {});

        assert_eq!(phase_from_activity(&state), AgentPhase::Idle);
    }

    /// Given no active turn but a recorded last-turn outcome, when deriving
    /// the phase, then it is `Ended`.
    #[test]
    fn should_derive_ended_when_no_turn_but_last_turn_present() {
        let state = snapshot(|view| {
            view.on_turn_started(1, PromptOrigin::User);
            view.on_turn_ended(1, TurnEndReason::Completed, Some(1500));
        });

        assert_eq!(phase_from_activity(&state), AgentPhase::Ended);
    }

    /// Given a turn that is ending AND has pending approvals, when deriving
    /// the phase, then it is `AwaitingApproval` — the TS source checks
    /// pending approvals before the ending flag (no premature `running` /
    /// `interrupted` while an approval is still outstanding).
    #[test]
    fn should_derive_awaiting_approval_when_approvals_pending() {
        let state = snapshot(|view| {
            view.on_turn_started(1, PromptOrigin::User);
            view.on_ending(Some(EndingReason::Aborted));
            view.on_approval_pending(ApprovalRef {
                approval_id: "ap-1".to_string(),
                tool_call_id: None,
                since: 0,
            });
        });

        assert_eq!(phase_from_activity(&state), AgentPhase::AwaitingApproval);
    }

    /// Given a turn in its ending phase with a known reason, when deriving
    /// the phase, then it is `Interrupted`.
    #[test]
    fn should_derive_interrupted_when_turn_is_ending() {
        let state = snapshot(|view| {
            view.on_turn_started(1, PromptOrigin::User);
            view.on_ending(Some(EndingReason::MaxSteps));
        });

        assert_eq!(phase_from_activity(&state), AgentPhase::Interrupted);
    }

    /// Given a turn whose ending flag is set but whose ending reason is
    /// unknown, when deriving the phase, then it falls through to the
    /// turn-phase mapping — the TS source requires `endingReason` to be
    /// defined before emitting `interrupted`.
    #[test]
    fn should_fall_through_to_turn_phase_when_ending_has_no_reason() {
        let state = snapshot(|view| {
            view.on_turn_started(1, PromptOrigin::User);
            view.on_streaming(StreamKind::Assistant);
            view.on_ending(None);
        });

        assert_eq!(phase_from_activity(&state), AgentPhase::Streaming);
    }

    /// Given a turn in the retrying phase, when deriving the phase, then it
    /// is `Retrying` — the TS source maps the `retrying` turn phase directly
    /// (retry detail defaults to zeroed fields when absent, it never falls
    /// back to `running`).
    #[test]
    fn should_derive_retrying_when_retry_state_present() {
        let state = snapshot(|view| {
            view.on_turn_started(1, PromptOrigin::User);
            view.on_retry(ActivityRetryState {
                failed_attempt: 1,
                next_attempt: 2,
                max_attempts: 3,
                delay_ms: 500,
                error_name: Some("rate_limited".to_string()),
                status_code: Some(429),
            });
        });

        assert_eq!(phase_from_activity(&state), AgentPhase::Retrying);
    }

    /// Given a turn in each of the plain phases (running, streaming,
    /// tool_call), when deriving the phase, then each maps 1:1 to the
    /// corresponding `AgentPhase` variant.
    #[test]
    fn should_derive_streaming_tool_call_and_running_from_turn_phase() {
        let running = snapshot(|view| {
            view.on_turn_started(1, PromptOrigin::User);
        });
        assert_eq!(phase_from_activity(&running), AgentPhase::Running);

        let streaming = snapshot(|view| {
            view.on_turn_started(2, PromptOrigin::User);
            view.on_streaming(StreamKind::Assistant);
        });
        assert_eq!(phase_from_activity(&streaming), AgentPhase::Streaming);

        let tool_call = snapshot(|view| {
            view.on_turn_started(3, PromptOrigin::User);
            view.on_tool_call("tc-1".to_string(), "Read".to_string());
        });
        assert_eq!(phase_from_activity(&tool_call), AgentPhase::ToolCall);
    }
}

// ===========================================================================
// SessionMetadata — last_used_at persistence roundtrip (SDR-014)
// ===========================================================================

/// Given a metadata with `last_used_at` set, When it is written through
/// the metadata ROW (ADR-006: the durable record) and read back, Then
/// `last_used_at` survives the round trip — and a legacy state file
/// predating the field deserializes to `None` (readers fall back to
/// `updated_at`).
#[tokio::test]
async fn should_roundtrip_last_used_at_through_state_persistence() {
    let env = TestEnv::new().await;
    let session = env
        .manager
        .create_session(make_metadata("Last Used"), SessionScopeConfig::default())
        .await
        .unwrap();
    let session_id = session.id().clone();

    // Given a metadata whose last_used_at is set (a send or a touch bumped it)
    let mut metadata = env
        .manager
        .metadata_store()
        .get(&session_id)
        .await
        .expect("the created session's row exists");
    let stamped = chrono::Utc::now();
    metadata.last_used_at = Some(stamped);
    env.manager
        .metadata_store()
        .apply(
            &session_id,
            &env.manager.metadata_workspace_id(),
            mirri_core::session::manager::full_metadata_patch(&SessionMetadata {
                last_used_at: Some(stamped),
                ..make_metadata("Last Used")
            }),
        )
        .await
        .unwrap();

    // When the row is read back
    let reloaded = env
        .manager
        .metadata_store()
        .get(&session_id)
        .await
        .expect("the row exists");

    // Then last_used_at survives the persisted round trip
    assert_eq!(reloaded.last_used_at, Some(stamped));
    // And the Default (a fresh session that never bumped it) is None
    assert_eq!(SessionMetadata::default().last_used_at, None);

    // And a legacy state.json predating the field deserializes to None
    let state_path = env.manager.persistence().state_path(&session_id);
    tokio::fs::write(
        &state_path,
        r#"{"title":"legacy","createdAt":"2024-01-01T00:00:00Z","updatedAt":"2024-01-02T00:00:00Z","tags":[]}"#,
    )
    .await
    .unwrap();
    let legacy = env
        .manager
        .persistence()
        .load_state(&session_id)
        .await
        .unwrap();
    assert_eq!(legacy.last_used_at, None);
}

}

mod session_resume_ {
//! Integration tests for session resume with wire-log replay (Task: session
//! resume).
//!
//! These tests verify that `SessionManager::resume_session` reconstructs a
//! closed session's agent state by replaying its `wire.jsonl` through the
//! wire-op reducer:
//!
//! - full replay from an empty base state when no snapshot exists;
//! - incremental replay from a `snapshot.json` checkpoint when one exists;
//! - empty wire logs resume to an empty (but usable) agent;
//! - closing a session writes a snapshot and resume recreates the agents.
//!
//! Tests use the Given-When-Then naming convention.

use std::collections::HashSet;

use mirri_core::shell_command::{ShellCommandRegistry, ShellRunTarget, run_shell_command};
use mirri_core::task::TaskStatus;
use mirri_core::todo::{TodoItem, TodoStatus};
use mirri_core::{
    AgentBlobService, AgentId, AgentScopeConfig, AppScope, AppScopeConfig, CloseReason, EventBus,
    PromptOrigin, Role, SessionError, SessionId, SessionManager, SessionMetadata,
    SessionScopeConfig, WorkspaceId, WorkspaceScopeConfig, WorkspaceTrustLevel,
};
use mirri_persistence::{AppendLogStore, JsonlAppendLog, SnapshotStore};
use mirri_proto::{AgentState, WireOp};
use serde_json::json;

// ===========================================================================
// Helper: create a workspace + session manager backed by a temp dir
// ===========================================================================

struct TestEnv {
    manager: SessionManager,
    _tmp: tempfile::TempDir,
}

impl TestEnv {
    async fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let app = AppScope::new(AppScopeConfig::default());
        let workspace = app
            .create_workspace_with_config(
                WorkspaceId::new("test-ws"),
                tmp.path().to_path_buf(),
                WorkspaceScopeConfig {
                    trust_level: WorkspaceTrustLevel::Trusted,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let manager = SessionManager::with_persistence_dir(
            workspace,
            tmp.path().join(".mirri").join("sessions"),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        Self { manager, _tmp: tmp }
    }
}

fn make_metadata(title: &str, tags: &[&str]) -> SessionMetadata {
    let mut tag_set = HashSet::new();
    for tag in tags {
        tag_set.insert(tag.to_string());
    }
    SessionMetadata {
        title: title.to_string(),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        tags: tag_set,
        archived: false,
        is_custom_title: false,
        work_dir: None,
        last_prompt: None,
        forked_from: None,
        cache_root_id: None,
        custom: None,
        last_used_at: None,
    }
}

/// A resume base config carrying an explicit model (spec 11 R4): the
/// hand-written wire logs in this file carry no model record, so the base
/// config seeds the model exactly the way the embedded host seeds it from
/// the config snapshot's `default_model` (the no-record fallback chain:
/// config default model → loud error).
fn resume_base_config() -> AgentScopeConfig {
    AgentScopeConfig {
        model_id: "test-resume-model".to_string(),
        ..AgentScopeConfig::default()
    }
}

/// Write a sequence of wire ops to the session's `wire.jsonl`
/// (truncating any existing file).
async fn write_wire_log(manager: &SessionManager, session_id: &SessionId, ops: &[WireOp]) {
    let wire_log_path = manager.persistence().wire_log_path(session_id);
    let mut log = JsonlAppendLog::create(&wire_log_path).await.unwrap();
    for op in ops {
        log.append(op).await.unwrap();
    }
}

/// Build a `context.append_message` op in the TS wire shape (camelCase
/// `toolCalls`, content parts tagged with `type`).
fn append_message_op(role: &str, text: &str) -> WireOp {
    WireOp::ContextAppendMessage {
        message: json!({
            "role": role,
            "content": [{"type": "text", "text": text}],
            "toolCalls": [],
        }),
    }
}

/// A full assistant step: step.begin → content.part → tool.call →
/// tool.result → step.end (shapes taken from the TS wire fixtures).
fn assistant_step_ops(step_id: &str, call_id: &str, text: &str) -> [WireOp; 5] {
    [
        WireOp::ContextAppendLoopEvent {
            event: json!({"type": "step.begin", "uuid": step_id, "turnId": "1", "step": 0}),
        },
        WireOp::ContextAppendLoopEvent {
            event: json!({"type": "content.part", "part": {"type": "text", "text": text}, "uuid": step_id}),
        },
        WireOp::ContextAppendLoopEvent {
            event: json!({
                "type": "tool.call",
                "callId": call_id,
                "name": "Read",
                "input": {"path": "/tmp/test.txt"},
                "uuid": step_id,
            }),
        },
        WireOp::ContextAppendLoopEvent {
            event: json!({
                "type": "tool.result",
                "callId": call_id,
                "result": {"output": [{"type": "text", "text": "file contents"}]},
                "uuid": step_id,
            }),
        },
        WireOp::ContextAppendLoopEvent {
            event: json!({
                "type": "step.end",
                "uuid": step_id,
                "turnId": "1",
                "step": 0,
                "finishReason": "end_turn",
                "usage": {"inputTokens": 10, "outputTokens": 5},
            }),
        },
    ]
}

/// The text content of each message in the agent's reconstructed history.
fn history_texts(agent: &mirri_core::AgentScope) -> Vec<String> {
    let memory = agent.context_memory();
    let guard = memory.lock().unwrap();
    guard
        .history()
        .iter()
        .map(|m| m.message.text_content())
        .collect()
}

// ===========================================================================
// SessionManager — resume replays the wire log into agent state
// ===========================================================================

#[tokio::test]
async fn should_reconstruct_state_when_session_with_wire_log_resumed() {
    // Given a closed session whose wire log records one user turn: two
    // plain messages plus a full assistant step (text + tool call + result)
    let env = TestEnv::new().await;
    let session = env
        .manager
        .create_session_with_id(
            SessionId::new("resume-wire-log"),
            make_metadata("Wire Log", &["resume"]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();
    let session_id = session.id().clone();

    let mut ops = vec![
        WireOp::Metadata {
            protocol_version: Some("1.5".to_string()),
            created_at: Some(1700000000000),
        },
        append_message_op("user", "hello"),
        append_message_op("assistant", "hi there"),
    ];
    ops.extend(assistant_step_ops("step-001", "call-001", "Let me check."));
    write_wire_log(&env.manager, &session_id, &ops).await;

    env.manager
        .close_session(&session_id, CloseReason::Exit)
        .await
        .unwrap();
    assert!(env.manager.get_session(&session_id).await.is_none());

    // When resuming the session
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then the main agent is recreated with the reconstructed context
    let agent = resumed
        .get_agent(&AgentId::new("main"))
        .await
        .expect("resumed session must recreate the main agent");
    let texts = history_texts(&agent);
    assert_eq!(
        texts,
        vec![
            "hello".to_string(),
            "hi there".to_string(),
            "Let me check.".to_string(),
            "file contents".to_string()
        ],
        "replayed state must yield the recorded messages in wire order"
    );

    let memory = agent.context_memory();
    {
        let guard = memory.lock().unwrap();
        let history = guard.history();
        assert_eq!(history.len(), 4);
        assert_eq!(history[0].message.role, Role::User);
        assert_eq!(history[1].message.role, Role::Assistant);

        // The step message carries the content part and the tool call
        assert_eq!(history[2].message.role, Role::Assistant);
        assert_eq!(history[2].message.tool_calls.len(), 1);
        assert_eq!(history[2].message.tool_calls[0].id, "call-001");
        assert_eq!(history[2].message.tool_calls[0].function.name, "Read");

        // The tool result message is attached via toolCallId (TS camelCase)
        assert_eq!(history[3].message.role, Role::Tool);
        assert_eq!(history[3].message.tool_call_id.as_deref(), Some("call-001"));
        assert!(!history[3].is_error);

        // Step usage is folded into the token count and the exchange is closed
        assert_eq!(guard.token_count(), 15);
        assert!(!guard.has_open_tool_exchange());
    } // guard dropped here, before the await below

    // And the session metadata is restored
    let metadata = resumed.metadata().await;
    assert_eq!(metadata.title, "Wire Log");
    assert!(metadata.tags.contains("resume"));
}

/// should reconstruct both inline skill activation wraps plus the body when a
/// session whose wire log records them is resumed (SDR-005 / US3.6 replay:
/// two activations in one turn replay as two independent activation facts and
/// no duplicate telemetry — the facts ride as `skill.activate` ops and the
/// wraps + body as ordered `context.append_message` records).
#[tokio::test]
async fn should_reconstruct_inline_skill_activations_when_session_resumed() {
    // Given a closed session whose wire log records one user turn with two
    // inline skill activation wraps threaded before the body message (the
    // ADR-004 "N wraps + 1 body" sequence) plus the assistant reply
    let env = TestEnv::new().await;
    let session = env
        .manager
        .create_session_with_id(
            SessionId::new("resume-inline-skills"),
            make_metadata("Inline", &[]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();
    let session_id = session.id().clone();
    let alpha_origin = json!({
        "kind": "skill_activation",
        "activationId": "msg-abc#0",
        "skillName": "alpha",
        "trigger": "user-inline",
        "skillPath": "/tmp/skills/alpha/SKILL.md",
        "skillSource": "project",
    });
    let beta_origin = json!({
        "kind": "skill_activation",
        "activationId": "msg-abc#1",
        "skillName": "beta",
        "trigger": "user-inline",
        "skillPath": "/tmp/skills/beta/SKILL.md",
        "skillSource": "project",
    });
    let ops: Vec<WireOp> = vec![
        WireOp::Metadata {
            protocol_version: Some("1.5".to_string()),
            created_at: Some(1700000000000),
        },
        WireOp::TurnPrompt {
            input: json!([{"type": "text", "text": "body"}]),
            origin: json!({"kind": "user"}),
        },
        WireOp::SkillActivate {
            origin: alpha_origin.clone(),
        },
        WireOp::ContextAppendMessage {
            message: json!({
                "role": "user",
                "content": [{"type": "text", "text": "Alpha wrap"}],
                "origin": alpha_origin,
            }),
        },
        WireOp::SkillActivate {
            origin: beta_origin.clone(),
        },
        WireOp::ContextAppendMessage {
            message: json!({
                "role": "user",
                "content": [{"type": "text", "text": "Beta wrap"}],
                "origin": beta_origin,
            }),
        },
        WireOp::ContextAppendMessage {
            message: json!({
                "role": "user",
                "content": [{"type": "text", "text": "body"}],
                "origin": json!({"kind": "user"}),
            }),
        },
        append_message_op("assistant", "ok."),
    ];
    write_wire_log(&env.manager, &session_id, &ops).await;

    env.manager
        .close_session(&session_id, CloseReason::Exit)
        .await
        .unwrap();
    assert!(env.manager.get_session(&session_id).await.is_none());

    // When resuming the session
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then the reconstructed context carries both wraps then the body then the
    // reply, and each activation message keeps its user-inline origin (the two
    // activation facts are recovered — no telemetry is re-run on replay)
    let agent = resumed
        .get_agent(&AgentId::new("main"))
        .await
        .expect("resumed session recreates the main agent");
    let texts = history_texts(&agent);
    // The two wraps plus the body plus the assistant reply. (The leading
    // TurnPrompt is a turn-op marker, not a message.)
    assert_eq!(
        texts,
        vec![
            "Alpha wrap".to_string(),
            "Beta wrap".to_string(),
            "body".to_string(),
            "ok.".to_string(),
        ],
        "replay yields wraps + body + reply in wire order"
    );
    let memory = agent.context_memory();
    let guard = memory.lock().unwrap();
    let history = guard.history();
    assert_eq!(history.len(), 4);
    assert_eq!(
        history[0].origin.as_ref(),
        Some(&PromptOrigin::SkillActivation {
            activation_id: Some("msg-abc#0".into()),
            skill_name: Some("alpha".into()),
            trigger: Some("user-inline".into()),
            skill_args: None,
            skill_path: Some("/tmp/skills/alpha/SKILL.md".into()),
            skill_source: Some("project".into()),
        })
    );
    assert_eq!(
        history[1].origin.as_ref(),
        Some(&PromptOrigin::SkillActivation {
            activation_id: Some("msg-abc#1".into()),
            skill_name: Some("beta".into()),
            trigger: Some("user-inline".into()),
            skill_args: None,
            skill_path: Some("/tmp/skills/beta/SKILL.md".into()),
            skill_source: Some("project".into()),
        })
    );
    // The body message is plain user input, the reply has no origin.
    assert_eq!(history[2].origin.as_ref(), Some(&PromptOrigin::User));
    assert_eq!(history[3].origin.as_ref(), None);
}

// ===========================================================================
// SessionManager — resume with a snapshot replays only from the checkpoint
// ===========================================================================

#[tokio::test]
async fn should_only_replay_from_checkpoint_when_session_with_snapshot_resumed() {
    // Given a persisted session whose wire log has three messages and a
    // snapshot checkpoint taken after the first op. The snapshot's state is
    // crafted to contain a message NOT in the log, so the test can observe
    // whether resume loads the snapshot as its base state (it must) and
    // whether it re-folds the ops before the checkpoint (it must not).
    let env = TestEnv::new().await;
    let session_id = SessionId::new("resume-snapshot");
    env.manager
        .persistence()
        .save_state(&session_id, &make_metadata("Checkpoint", &[]))
        .await
        .unwrap();
    write_wire_log(
        &env.manager,
        &session_id,
        &[
            append_message_op("user", "from-log-1"),
            append_message_op("user", "from-log-2"),
            append_message_op("user", "from-log-3"),
        ],
    )
    .await;

    let mut snapshot_state = AgentState::new();
    snapshot_state.conversation.messages.push(json!({
        "role": "user",
        "content": [{"type": "text", "text": "from-snapshot"}],
        "toolCalls": [],
    }));
    let snapshot_path = env.manager.persistence().snapshot_path(&session_id);
    SnapshotStore::write(&snapshot_path, &snapshot_state, 1)
        .await
        .unwrap();
    assert!(snapshot_path.exists());

    // When resuming
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then the agent's context is the snapshot base plus exactly the ops
    // recorded after the checkpoint — op 0 was not replayed
    let agent = resumed
        .get_agent(&AgentId::new("main"))
        .await
        .expect("main agent recreated");
    let texts = history_texts(&agent);
    assert_eq!(
        texts,
        vec![
            "from-snapshot".to_string(),
            "from-log-2".to_string(),
            "from-log-3".to_string(),
        ],
        "resume must load the snapshot state and replay only ops after the checkpoint"
    );
    assert!(
        !texts.contains(&"from-log-1".to_string()),
        "ops before the snapshot op_index must not be replayed"
    );
}

// ===========================================================================
// SessionManager — resume with an empty wire log yields an empty state
// ===========================================================================

#[tokio::test]
async fn should_yield_empty_state_when_session_with_no_wire_log_resumed() {
    // Given a persisted session whose wire log exists but is empty
    let env = TestEnv::new().await;
    let session_id = SessionId::new("resume-empty-log");
    env.manager
        .persistence()
        .save_state(&session_id, &make_metadata("Empty Log", &[]))
        .await
        .unwrap();
    let wire_log_path = env.manager.persistence().wire_log_path(&session_id);
    tokio::fs::create_dir_all(wire_log_path.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(&wire_log_path, "").await.unwrap();

    // When resuming
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then the session is live with the main agent holding an empty context
    let agent = resumed
        .get_agent(&AgentId::new("main"))
        .await
        .expect("main agent recreated");
    let memory = agent.context_memory();
    let guard = memory.lock().unwrap();
    assert!(
        guard.history().is_empty(),
        "empty wire log must yield an empty context"
    );
    assert_eq!(guard.token_count(), 0);
    assert!(!guard.has_open_tool_exchange());
}

// ===========================================================================
// SessionManager — close writes a snapshot; resume recreates the agents
// ===========================================================================

#[tokio::test]
async fn should_recreate_agents_when_closed_session_resumed() {
    // Given a session with a recorded conversation that is closed (close
    // must persist a snapshot checkpoint)
    let env = TestEnv::new().await;
    let session = env
        .manager
        .create_session_with_id(
            SessionId::new("resume-after-close"),
            make_metadata("Closed Session", &["restored"]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();
    let session_id = session.id().clone();
    write_wire_log(
        &env.manager,
        &session_id,
        &[
            append_message_op("user", "question one"),
            append_message_op("assistant", "answer one"),
        ],
    )
    .await;

    let snapshot_path = env.manager.persistence().snapshot_path(&session_id);
    assert!(!snapshot_path.exists(), "no snapshot before close");

    env.manager
        .close_session(&session_id, CloseReason::Exit)
        .await
        .unwrap();
    assert!(env.manager.get_session(&session_id).await.is_none());

    // Then the close wrote a snapshot checkpoint
    assert!(
        snapshot_path.exists(),
        "closing a session with a wire log must write a snapshot"
    );

    // When resuming the closed session
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then the agent is recreated with the reconstructed context
    let agents = resumed.list_agents().await;
    assert_eq!(
        agents.len(),
        1,
        "resumed session must contain its main agent"
    );
    assert_eq!(agents[0], AgentId::new("main"));

    let agent = resumed.get_agent(&AgentId::new("main")).await.unwrap();
    assert_eq!(
        history_texts(&agent),
        vec!["question one".to_string(), "answer one".to_string()]
    );

    // And the metadata survives the close/resume round trip
    let metadata = resumed.metadata().await;
    assert_eq!(metadata.title, "Closed Session");
    assert!(metadata.tags.contains("restored"));
}

// ===========================================================================
// SessionManager — a compacted wire log resumes with the summary restored
// ===========================================================================

#[tokio::test]
async fn should_restore_summary_message_when_compacted_wire_log_resumed() {
    // Given a session whose wire log contains a compaction: two messages
    // replaced by a summary, then one more message
    let env = TestEnv::new().await;
    let session_id = SessionId::new("resume-compacted");
    env.manager
        .persistence()
        .save_state(&session_id, &make_metadata("Compacted", &[]))
        .await
        .unwrap();
    write_wire_log(
        &env.manager,
        &session_id,
        &[
            append_message_op("user", "old message one"),
            append_message_op("user", "old message two"),
            WireOp::ContextApplyCompaction {
                summary: "context compacted here".to_string(),
                compacted_count: 2,
                tokens_before: Some(60_000),
                tokens_after: Some(800),
                context_summary: None,
                dropped_count: Some(2),
                retained_tail_count: None,
                kept_user_message_count: None,
                kept_head_user_message_count: None,
                elision_message: None,
                first_replaced_entry_seq: None,
                last_replaced_entry_seq: None,
                dropped_entry_seqs: None,
            },
            append_message_op("user", "message after compaction"),
        ],
    )
    .await;

    // When resuming
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then the compacted messages are gone and the summary is restored as a
    // user message tagged with the compaction origin, ahead of the newer
    // message
    let agent = resumed.get_agent(&AgentId::new("main")).await.unwrap();
    let memory = agent.context_memory();
    let guard = memory.lock().unwrap();
    let history = guard.history();
    assert_eq!(history.len(), 2);
    assert_eq!(
        history[0].origin,
        Some(PromptOrigin::CompactionSummary),
        "compaction summary must be restored with the compaction origin"
    );
    assert_eq!(history[0].message.text_content(), "context compacted here");
    assert_eq!(
        history[1].message.text_content(),
        "message after compaction"
    );
}

// ===========================================================================
// SessionManager — resume edge cases
// ===========================================================================

#[tokio::test]
async fn should_return_error_when_resume_session_with_nonexistent_id() {
    // Given a session id that has never been created or persisted
    let env = TestEnv::new().await;
    let session_id = SessionId::new("never-created");

    // When attempting to resume it
    let result = env.manager.resume_session(&session_id).await;

    // Then resume fails because the wire log does not exist
    assert!(
        matches!(result, Err(SessionError::ResumeFailed(_))),
        "resuming a nonexistent session must fail, got: {result:?}"
    );
}

/// Given a persisted session whose wire log carries no model record (no
/// `config.update` op) and a resume base config with no model, when the
/// session is resumed, then resume fails loudly with an actionable error
/// pointing at the configuration path — never a silent fallback to a
/// built-in model (spec 11 R9: the no-record fallback chain equals the
/// fresh-create chain: config default model → error).
#[tokio::test]
async fn should_fail_resume_with_actionable_error_when_no_model_record_and_no_default() {
    let env = TestEnv::new().await;
    let session_id = SessionId::new("resume-no-model");
    env.manager
        .create_session_with_id(
            session_id.clone(),
            make_metadata("No Model", &["resume"]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();
    // A wire log with conversation ops only — no `config.update` model
    // record anywhere.
    write_wire_log(
        &env.manager,
        &session_id,
        &[
            WireOp::Metadata {
                protocol_version: Some("1.5".to_string()),
                created_at: Some(1700000000000),
            },
            append_message_op("user", "hello"),
        ],
    )
    .await;
    env.manager
        .close_session(&session_id, CloseReason::Exit)
        .await
        .unwrap();

    // When the session is resumed on a base config carrying no model
    let result = env.manager.resume_session(&session_id).await;

    // Then resume fails with an actionable message naming the remediation
    // (a model record or a configured default model), not a built-in one
    let error = result.expect_err("resume must fail when no model is resolvable");
    assert!(
        matches!(&error, SessionError::ResumeFailed(message)
            if message.contains("default_model")),
        "expected an actionable ResumeFailed error, got: {error:?}"
    );
    assert!(
        !error.to_string().contains("claude-"),
        "the error must not name any built-in model: {error}"
    );
}

/// Given a multi-agent session whose wire log records two session-level
/// model switches (the second to "switch-two") and a registered subagent
/// whose registration pins "pinned-at-spawn", when the session is
/// resumed, then the main agent's model is the LAST switch record (A17 —
/// the switch records carry no agent attribution, so no fold
/// misattribution is possible) and the child is rehydrated on its
/// registration's pinned value (A14/R5 — the spawn-time pin survives the
/// restart, spec 11 §2.4).
#[tokio::test]
async fn should_restore_multi_agent_session_to_last_switched_model() {
    // Given a persisted multi-agent session: two session-level switch
    // records plus a pinned subagent registration, and the child's own
    // (empty) journal on disk so the restore rehydrates it
    let env = TestEnv::new().await;
    let session_id = SessionId::new("resume-multi-agent");
    env.manager
        .persistence()
        .save_state(&session_id, &make_metadata("Multi Agent", &["resume"]))
        .await
        .unwrap();
    let config_update = |alias: &str| WireOp::ConfigUpdate {
        model_alias: Some(alias.to_string()),
        profile_name: None,
        thinking_effort: None,
        system_prompt: None,
        disallowed_tools: None,
    };
    let child_id = "agent-pinned";
    write_wire_log(
        &env.manager,
        &session_id,
        &[
            WireOp::Metadata {
                protocol_version: Some("1.5".to_string()),
                created_at: Some(1700000000000),
            },
            config_update("switch-one"),
            config_update("switch-two"),
            WireOp::SubagentRegistered {
                agent_id: child_id.to_string(),
                profile_name: "coder".to_string(),
                model: Some("pinned-at-spawn".to_string()),
                thinking_effort: None,
                parent_agent_id: Some("main".to_string()),
                swarm_item: None,
                parent_tool_call_id: None,
                swarm_index: None,
            },
        ],
    )
    .await;
    let child_journal = env
        .manager
        .persistence()
        .agent_wire_log_path(&session_id, child_id);
    std::fs::create_dir_all(child_journal.parent().unwrap()).unwrap();
    std::fs::write(&child_journal, "").unwrap();

    // When the session is resumed
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .expect("resume succeeds");

    // Then the main agent carries the LAST switch record (A17) — neither
    // the base config's default nor the first switch
    let main = resumed
        .get_agent(&AgentId::new("main"))
        .await
        .expect("main agent recreated");
    assert_eq!(
        main.config()
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .model_id,
        "switch-two",
        "the session-level records fold onto the main agent in order — the last switch wins"
    );

    // And the subagent, once woken (lazy restore: it stays dormant until the
    // child is hydrated), is rehydrated on its registration's pinned model —
    // the dispatch-time pin survives the restart (A14/R5)
    assert!(
        resumed.hydrate_subagent(child_id).await,
        "the dormant child hydrates on wake"
    );
    let child = resumed
        .get_agent(&AgentId::new(child_id))
        .await
        .expect("the registered child is rehydrated");
    assert_eq!(
        child
            .config()
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .model_id,
        "pinned-at-spawn",
        "the child keeps its dispatch-time pinned model across the resume"
    );
}

#[tokio::test]
async fn should_resume_with_multiple_assistant_steps_when_wire_log_has_many() {
    // Given a session whose wire log records a user turn followed by three
    // full assistant steps (text + tool call + result each)
    let env = TestEnv::new().await;
    let session_id = SessionId::new("resume-many-steps");
    env.manager
        .persistence()
        .save_state(&session_id, &make_metadata("Many Steps", &["multi"]))
        .await
        .unwrap();
    let mut ops = vec![append_message_op("user", "start")];
    ops.extend(assistant_step_ops("step-001", "call-001", "first answer"));
    ops.extend(assistant_step_ops("step-002", "call-002", "second answer"));
    ops.extend(assistant_step_ops("step-003", "call-003", "third answer"));
    write_wire_log(&env.manager, &session_id, &ops).await;

    // When resuming
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then all three steps are replayed in wire order with no open exchange
    let agent = resumed
        .get_agent(&AgentId::new("main"))
        .await
        .expect("main agent recreated");
    assert_eq!(
        history_texts(&agent),
        vec![
            "start".to_string(),
            "first answer".to_string(),
            "file contents".to_string(),
            "second answer".to_string(),
            "file contents".to_string(),
            "third answer".to_string(),
            "file contents".to_string(),
        ],
        "every replayed step must yield its assistant text and tool result"
    );
    let memory = agent.context_memory();
    let guard = memory.lock().unwrap();
    assert_eq!(guard.history().len(), 7);
    // Token count holds the latest step's reported usage (10 + 5), not a sum.
    assert_eq!(guard.token_count(), 15);
    assert!(!guard.has_open_tool_exchange());
}

#[tokio::test]
async fn should_preserve_metadata_tags_when_session_resumed() {
    // Given a closed session carrying multiple metadata tags
    let env = TestEnv::new().await;
    let session = env
        .manager
        .create_session_with_id(
            SessionId::new("resume-tags"),
            make_metadata("Tagged Session", &["alpha", "beta", "gamma"]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();
    let session_id = session.id().clone();
    write_wire_log(
        &env.manager,
        &session_id,
        &[append_message_op("user", "tagged question")],
    )
    .await;
    env.manager
        .close_session(&session_id, CloseReason::Exit)
        .await
        .unwrap();

    // When resuming the closed session
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then every tag survives the close/resume round trip
    let metadata = resumed.metadata().await;
    assert_eq!(
        metadata.tags.len(),
        3,
        "all tags must be preserved, got: {:?}",
        metadata.tags
    );
    for tag in ["alpha", "beta", "gamma"] {
        assert!(
            metadata.tags.contains(tag),
            "tag {tag:?} must be preserved, got: {:?}",
            metadata.tags
        );
    }
}

#[tokio::test]
async fn should_resume_with_tool_error_result_when_wire_log_has_error() {
    // Given a session whose wire log records an assistant step whose
    // tool.result event is flagged with isError
    let env = TestEnv::new().await;
    let session_id = SessionId::new("resume-tool-error");
    env.manager
        .persistence()
        .save_state(&session_id, &make_metadata("Tool Error", &[]))
        .await
        .unwrap();
    write_wire_log(
        &env.manager,
        &session_id,
        &[
            WireOp::ContextAppendLoopEvent {
                event: json!({"type": "step.begin", "uuid": "step-err", "turnId": "1", "step": 0}),
            },
            WireOp::ContextAppendLoopEvent {
                event: json!({
                    "type": "tool.call",
                    "callId": "call-err",
                    "name": "Bash",
                    "input": {"command": "exit 1"},
                    "uuid": "step-err",
                }),
            },
            WireOp::ContextAppendLoopEvent {
                event: json!({
                    "type": "tool.result",
                    "callId": "call-err",
                    "result": {"output": [{"type": "text", "text": "command failed"}]},
                    "uuid": "step-err",
                    "isError": true,
                }),
            },
            WireOp::ContextAppendLoopEvent {
                event: json!({
                    "type": "step.end",
                    "uuid": "step-err",
                    "turnId": "1",
                    "step": 0,
                    "finishReason": "end_turn",
                    "usage": {"inputTokens": 5, "outputTokens": 2},
                }),
            },
        ],
    )
    .await;

    // When resuming
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then the replayed tool result keeps its error flag
    let agent = resumed
        .get_agent(&AgentId::new("main"))
        .await
        .expect("main agent recreated");
    let memory = agent.context_memory();
    let guard = memory.lock().unwrap();
    let history = guard.history();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].message.role, Role::Assistant);
    assert_eq!(history[1].message.role, Role::Tool);
    assert_eq!(history[1].message.tool_call_id.as_deref(), Some("call-err"));
    assert_eq!(history[1].message.text_content(), "command failed");
    assert!(
        history[1].is_error,
        "the isError flag must survive the wire-log replay"
    );
}

// ===========================================================================
// SessionManager — old-format (v1.0) wire logs are migrated on resume (12A)
// ===========================================================================

/// Write a raw wire-log file (bypassing `WireOp` serialisation) so a test
/// can craft old-format records the current kernel no longer writes.
async fn write_raw_wire_log(manager: &SessionManager, session_id: &SessionId, content: &str) {
    let wire_log_path = manager.persistence().wire_log_path(session_id);
    tokio::fs::create_dir_all(wire_log_path.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(&wire_log_path, content).await.unwrap();
}

/// The first line of the session's `wire.jsonl`, parsed as JSON.
async fn read_wire_log_header(
    manager: &SessionManager,
    session_id: &SessionId,
) -> serde_json::Value {
    let wire_log_path = manager.persistence().wire_log_path(session_id);
    let content = tokio::fs::read_to_string(&wire_log_path).await.unwrap();
    serde_json::from_str(content.lines().next().unwrap()).unwrap()
}

#[tokio::test]
async fn should_migrate_and_replay_old_wire_log_when_resuming_session_with_v1_0_log() {
    // Given a persisted session whose wire log is written in the v1.0
    // protocol: legacy `op`-tagged records carrying a v1.0 header. Without
    // migration these records do not deserialise as current WireOps and
    // would be lost (or fail the read).
    let env = TestEnv::new().await;
    let session_id = SessionId::new("resume-v1.0-log");
    env.manager
        .persistence()
        .save_state(&session_id, &make_metadata("Old Log", &["legacy"]))
        .await
        .unwrap();
    write_raw_wire_log(
        &env.manager,
        &session_id,
        concat!(
            r#"{"op":"metadata","protocolVersion":"1.0","createdAt":"2024-01-01T00:00:00Z"}"#,
            "\n",
            r#"{"op":"permission.set_mode","mode":"auto","time":"2024-01-01T00:00:01Z"}"#,
            "\n",
            r#"{"op":"context.append_message","message":{"role":"user","content":[{"type":"text","text":"from the old log"}],"toolCalls":[]},"time":"2024-01-01T00:00:02Z"}"#,
            "\n",
        ),
    )
    .await;

    // When resuming the session
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then every record is migrated and replayed — the message survives
    let agent = resumed
        .get_agent(&AgentId::new("main"))
        .await
        .expect("main agent recreated");
    assert_eq!(
        history_texts(&agent),
        vec!["from the old log".to_string()],
        "migrated v1.0 records must replay into the agent context"
    );

    // And the wire log was rewritten atomically to the current protocol
    let header = read_wire_log_header(&env.manager, &session_id).await;
    assert_eq!(
        header["type"], "metadata",
        "the legacy op discriminator must be rewritten as the current type tag"
    );
    assert_eq!(
        header["protocol_version"], "2.0",
        "the rewritten header must carry the current protocol version"
    );
}

#[tokio::test]
async fn should_still_resume_when_old_log_rewrite_fails_due_to_torn_final_line() {
    // Given a v1.0 wire log whose final line is torn (truncated mid-append,
    // no trailing newline). The on-disk migration rewrite cannot handle the
    // torn line, but the in-memory migration used for replay can — resume
    // must not be blocked by the rewrite failure.
    let env = TestEnv::new().await;
    let session_id = SessionId::new("resume-torn-old-log");
    env.manager
        .persistence()
        .save_state(&session_id, &make_metadata("Torn Old Log", &[]))
        .await
        .unwrap();
    write_raw_wire_log(
        &env.manager,
        &session_id,
        concat!(
            r#"{"op":"metadata","protocolVersion":"1.0","createdAt":"2024-01-01T00:00:00Z"}"#,
            "\n",
            r#"{"op":"permission.set_mode","mode":"auto","time":"2024-01-01T00:00:01Z"}"#,
            "\n",
            r#"{"op":"context.append_message","message":{"role":"user","content":[{"type":"text","text":"before the crash"}],"toolCalls":[]},"time":"2024-01-01T00:00:02Z"}"#,
            "\n",
            r#"{"op":"context.append_message","message":{"role":"us"#,
        ),
    )
    .await;

    // When resuming the session
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then the complete records replay and the torn line is silently dropped
    let agent = resumed
        .get_agent(&AgentId::new("main"))
        .await
        .expect("main agent recreated");
    assert_eq!(
        history_texts(&agent),
        vec!["before the crash".to_string()],
        "the migrated records before the torn line must replay"
    );
}

// ===========================================================================
// SessionManager — blobref media is rehydrated on resume (12F)
// ===========================================================================

#[tokio::test]
async fn should_rehydrate_blobref_media_when_resuming_session_with_blob_urls_in_log() {
    // Given a persisted session whose wire log references a `blobref:` media
    // URL backed by a real blob in the session's blob store
    let env = TestEnv::new().await;
    let session_id = SessionId::new("resume-blobs");
    env.manager
        .persistence()
        .save_state(&session_id, &make_metadata("Blobs", &["media"]))
        .await
        .unwrap();

    let blob_service = AgentBlobService::new(env.manager.persistence().session_dir(&session_id));
    let blob_ref_url = blob_service
        .write_blob("image/png", "aGVsbG8=")
        .expect("blob write should succeed");

    write_wire_log(
        &env.manager,
        &session_id,
        &[WireOp::ContextAppendMessage {
            message: json!({
                "role": "user",
                "content": [{"type": "image_url", "image_url": {"url": blob_ref_url}}],
                "toolCalls": [],
            }),
        }],
    )
    .await;

    // When resuming the session
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then the blobref is rehydrated back to its data: URI
    let agent = resumed
        .get_agent(&AgentId::new("main"))
        .await
        .expect("main agent recreated");
    let memory = agent.context_memory();
    let guard = memory.lock().unwrap();
    let history = guard.history();
    assert_eq!(history.len(), 1);
    match &history[0].message.content[0] {
        mirri_core::ContentPart::ImageUrl { image_url, .. } => {
            assert_eq!(
                image_url.url, "data:image/png;base64,aGVsbG8=",
                "the blobref URL must be rehydrated to its data URI"
            );
        }
        other => panic!("expected ImageUrl, got {other:?}"),
    }
}

#[tokio::test]
async fn should_rehydrate_multi_part_file_and_video_blobs_when_resuming_session_with_blob_refs() {
    // Given a persisted session whose wire log records a multi-part user
    // message — text + a video blobref + a file part whose inline data is a
    // blobref — all backed by real blobs in the session's blob store
    let env = TestEnv::new().await;
    let session_id = SessionId::new("resume-blobs-multipart");
    env.manager
        .persistence()
        .save_state(&session_id, &make_metadata("Blobs", &["media"]))
        .await
        .unwrap();

    let blob_service = AgentBlobService::new(env.manager.persistence().session_dir(&session_id));
    let video_blob = blob_service
        .write_blob("video/mp4", "dmlkZW8=")
        .expect("video blob write should succeed");
    let file_blob = blob_service
        .write_blob("application/pdf", "ZmlsZQ==")
        .expect("file blob write should succeed");

    write_wire_log(
        &env.manager,
        &session_id,
        &[WireOp::ContextAppendMessage {
            message: json!({
                "role": "user",
                "content": [
                    { "type": "text", "text": "see attachments" },
                    { "type": "video_url", "video_url": video_blob },
                    { "type": "file", "data": file_blob, "name": "doc.pdf", "media_type": "application/pdf", "size": 5 },
                ],
                "toolCalls": [],
            }),
        }],
    )
    .await;

    // When resuming the session
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then every media part is rehydrated back to its data: URI and the
    // text part survives untouched
    let agent = resumed
        .get_agent(&AgentId::new("main"))
        .await
        .expect("main agent recreated");
    let memory = agent.context_memory();
    let guard = memory.lock().unwrap();
    let history = guard.history();
    assert_eq!(history.len(), 1);
    let parts = &history[0].message.content;
    assert_eq!(parts.len(), 3, "all three parts survive: {parts:?}");
    match &parts[0] {
        mirri_core::ContentPart::Text { text } => {
            assert_eq!(text, "see attachments");
        }
        other => panic!("expected Text, got {other:?}"),
    }
    match &parts[1] {
        mirri_core::ContentPart::VideoUrl { video_url } => {
            assert_eq!(
                video_url, "data:video/mp4;base64,dmlkZW8=",
                "the video blobref must be rehydrated to its data URI"
            );
        }
        other => panic!("expected VideoUrl, got {other:?}"),
    }
    match &parts[2] {
        mirri_core::ContentPart::File {
            file_id,
            data,
            name,
            media_type,
            size,
        } => {
            assert_eq!(file_id, &None, "no bare files-store reference in memory");
            assert_eq!(
                data.as_deref(),
                Some("data:application/pdf;base64,ZmlsZQ=="),
                "the file blobref must be rehydrated to its data URI"
            );
            assert_eq!(name.as_deref(), Some("doc.pdf"));
            assert_eq!(media_type.as_deref(), Some("application/pdf"));
            assert_eq!(*size, Some(5));
        }
        other => panic!("expected File, got {other:?}"),
    }
}

// ===========================================================================
// SessionManager — cron tasks are restored on resume (P2b)
// ===========================================================================

/// A `cron.add` op carrying a task in the kernel's `CronTask` serde shape.
/// `next_fire_at` is deliberately stale (long past) so tests can observe that
/// restore recomputes it instead of trusting the persisted timestamp.
fn cron_add_op(id: &str, cron: &str, recurring: bool) -> WireOp {
    WireOp::CronAdd {
        task: json!({
            "id": id,
            "cron": cron,
            "prompt": "tick",
            "session_id": "resume-cron",
            "recurring": recurring,
            "created_at": "2020-01-01T00:00:00Z",
            "last_fired": null,
            "next_fire_at": "2020-01-01T00:01:00Z",
        }),
    }
}

#[tokio::test]
async fn should_restore_cron_tasks_with_recomputed_next_fire_when_session_with_cron_ops_resumed() {
    use chrono::TimeZone;
    // Given a persisted session whose wire log registered two cron tasks
    // (both with stale, long-past next fire times) and deleted one
    let env = TestEnv::new().await;
    let session_id = SessionId::new("resume-cron-active");
    env.manager
        .persistence()
        .save_state(&session_id, &make_metadata("Cron", &["cron"]))
        .await
        .unwrap();
    write_wire_log(
        &env.manager,
        &session_id,
        &[
            WireOp::Metadata {
                protocol_version: Some("1.5".to_string()),
                created_at: Some(1_700_000_000_000),
            },
            cron_add_op("cron-1", "*/5 * * * *", true),
            cron_add_op("cron-2", "0 9 * * *", true),
            WireOp::CronDelete {
                ids: vec!["cron-2".to_string()],
            },
        ],
    )
    .await;
    // When resuming the session
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then the surviving task is restored with a next fire time rebuilt
    // from the record's cursor (created_at — the TS baseFromMs), never
    // from the persisted absolute timestamp: it is the first */5
    // occurrence after 2020-01-01T00:00:00Z plus the deterministic
    // per-id jitter (<= 30s). A schedule in the past stays due — the
    // next check_due delivers the missed periods as one coalesced fire
    // (the TS catch-up semantics; B5).
    let cron = resumed.cron();
    let tasks = cron.list();
    assert_eq!(tasks.len(), 1, "deleted task stays deleted: {tasks:?}");
    assert_eq!(tasks[0].id, "cron-1");
    let next = tasks[0]
        .next_fire_at
        .expect("next fire is recomputed on restore");
    let base = chrono::Utc.with_ymd_and_hms(2020, 1, 1, 0, 0, 0).unwrap();
    let persisted = chrono::Utc.with_ymd_and_hms(2020, 1, 1, 0, 1, 0).unwrap();
    assert!(
        next != persisted,
        "stale persisted next_fire_at must be recomputed: {next}"
    );
    assert!(next > base, "the walk starts at the cursor: {next}");
    assert!(
        next <= base + chrono::Duration::minutes(5) + chrono::Duration::seconds(31),
        "the first */5 occurrence after the cursor (+jitter): {next}"
    );
}

#[tokio::test]
async fn should_not_refire_a_fired_one_shot_when_session_with_cron_cursor_resumed() {
    // Given a persisted session whose wire log registered a one-shot task
    // and recorded its fire cursor before the restart
    let env = TestEnv::new().await;
    let session_id = SessionId::new("resume-cron-fired");
    env.manager
        .persistence()
        .save_state(&session_id, &make_metadata("Cron", &["cron"]))
        .await
        .unwrap();
    write_wire_log(
        &env.manager,
        &session_id,
        &[
            cron_add_op("cron-1", "*/5 * * * *", false),
            WireOp::CronCursor {
                id: "cron-1".to_string(),
                last_fired_at: chrono::Utc::now().timestamp_millis() as u64,
                fire_count: None,
            },
        ],
    )
    .await;

    // When resuming the session
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then the fired one-shot stays listed with its last_fired preserved but
    // never fires again
    let cron = resumed.cron();
    let task = cron.get("cron-1").expect("fired one-shot stays listed");
    assert!(
        task.last_fired.is_some(),
        "the cursor's last_fired is preserved"
    );
    assert!(
        task.next_fire_at.is_none(),
        "a fired one-shot is never rescheduled"
    );
    assert!(
        cron.check_due().await.is_empty(),
        "a fired one-shot must not re-fire after resume"
    );
}

#[tokio::test]
async fn should_restore_held_fires_when_session_with_cron_held_op_resumed() {
    // C.4: a session that closed with a fire still held reopens with that
    // fire in the held table — the resume redelivers it on the first idle
    // tick instead of dropping the injection.
    let env = TestEnv::new().await;
    let session_id = SessionId::new("resume-cron-held");
    env.manager
        .persistence()
        .save_state(&session_id, &make_metadata("Cron", &["cron"]))
        .await
        .unwrap();
    write_wire_log(
        &env.manager,
        &session_id,
        &[
            cron_add_op("cron-1", "*/5 * * * *", true),
            WireOp::CronHeldAdd {
                task: json!({
                    "task_id": "cron-1",
                    "prompt": "tick",
                    "cron": "*/5 * * * *",
                    "recurring": true,
                    "coalesced_count": 2,
                    "stale": false,
                    "maxed_out": false,
                                    }),
            },
        ],
    )
    .await;

    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    let held = resumed.cron().held_fires();
    assert_eq!(held.len(), 1, "the held fire survives the close→reopen");
    assert_eq!(held[0].task_id, "cron-1");
    assert_eq!(held[0].coalesced_count, 2);
}

#[tokio::test]
async fn should_keep_resuming_when_cron_task_record_is_unparseable() {
    let env = TestEnv::new().await;
    let session_id = SessionId::new("resume-cron-bad-record");
    env.manager
        .persistence()
        .save_state(&session_id, &make_metadata("Cron", &["cron"]))
        .await
        .unwrap();
    write_wire_log(
        &env.manager,
        &session_id,
        &[
            WireOp::CronAdd {
                task: json!({ "id": 123, "garbage": true }),
            },
            cron_add_op("cron-good", "*/5 * * * *", true),
        ],
    )
    .await;

    // When resuming the session, the bad record is skipped without failing
    // the resume and the good task is restored
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    let tasks = resumed.cron().list();
    assert_eq!(tasks.len(), 1, "only the parseable task is restored");
    assert_eq!(tasks[0].id, "cron-good");
}

// ===========================================================================
// Detached background task across close→resume (F-P3-3, engine-managed
// stance: tasks do not resume running; the record and its output log
// survive on disk and stay readable)
// ===========================================================================

/// Given a session with a detached background shell task that has produced
/// output, when the session is closed and resumed, then the task record is
/// restored from `<session_dir>/tasks/` (stopped — never resumed running)
/// and its accumulated output is readable through the resumed scope.
#[tokio::test]
async fn should_restore_detached_task_record_and_output_when_session_is_resumed() {
    // Given a session whose detached run keeps producing output
    let env = TestEnv::new().await;
    let session = env
        .manager
        .create_session_with_id(
            SessionId::new("resume-detached-task"),
            make_metadata("Detached", &["task"]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();
    let session_id = session.id().clone();

    // A tiny foreground window forces the run to detach immediately; the
    // marker prints after the detach point so it lands on the background
    // task (output within the foreground window stays in the result's
    // stdout), then the command outlives the test's close.
    let result = run_shell_command(
        ShellRunTarget {
            session_id: session_id.as_str().to_string(),
            registry: std::sync::Arc::new(ShellCommandRegistry::new()),
            tasks: session.tasks(),
            event_bus: std::sync::Arc::new(EventBus::new(64)),
            process: std::sync::Arc::new(mirri_os::LocalProcess::new()),
        },
        "sleep 1 && echo detached-marker && sleep 30",
        None,
        std::time::Duration::from_millis(200),
        |_| {},
    )
    .await;
    let task_id = result
        .backgrounded
        .expect("the run must detach to a background task");

    // Wait for the marker to land on the task (and thus on disk).
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let output = session
            .tasks()
            .output(&task_id)
            .expect("task output while running");
        if output.full_output.contains("detached-marker") {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "detached output never landed, was: {:?}",
            output.full_output
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // When the session is closed and resumed
    env.manager
        .close_session(&session_id, CloseReason::Exit)
        .await
        .unwrap();
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then the task record survived on disk: present, and stopped — the
    // engine-managed stance (a running task never resumes as running).
    let task = resumed
        .tasks()
        .get(&task_id)
        .expect("the detached task record must be restored on resume");
    assert_eq!(
        task.status,
        TaskStatus::Stopped,
        "close marks running tasks stopped; resume must not resurrect them"
    );

    // And its accumulated output is readable through the resumed scope
    // (served from the persisted output.log).
    let output = resumed
        .tasks()
        .output(&task_id)
        .expect("restored task output must be readable");
    assert!(
        output.full_output.contains("detached-marker"),
        "restored output must carry the pre-close marker, was: {}",
        output.full_output
    );
}

// ===========================================================================
// SessionManager — resume restores the todo list from the folded tool store
// ===========================================================================

/// A `tools.update_store` (key "todo") op carrying `items`.
fn todo_store_op(items: serde_json::Value) -> WireOp {
    WireOp::ToolsUpdateStore {
        key: "todo".to_string(),
        value: items,
    }
}

/// Given a closed session whose wire log records one `tools.update_store`
/// (todo) op with two items, when the session is resumed, then the resumed
/// session's todo list holds both items with their statuses preserved.
#[tokio::test]
async fn should_restore_the_todo_list_when_resuming_a_session() {
    let env = TestEnv::new().await;
    let session = env
        .manager
        .create_session_with_id(
            SessionId::new("resume-todo"),
            make_metadata("Todo Session", &["todo"]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();
    let session_id = session.id().clone();
    write_wire_log(
        &env.manager,
        &session_id,
        &[
            append_message_op("user", "plan the work"),
            todo_store_op(json!([
                {"title": "Write tests", "status": "pending"},
                {"title": "Ship it", "status": "in_progress"},
            ])),
        ],
    )
    .await;
    env.manager
        .close_session(&session_id, CloseReason::Exit)
        .await
        .unwrap();

    // When resuming
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then the session-scoped todo list carries both recorded items
    let todos = resumed.todo_list().get();
    assert_eq!(todos.len(), 2, "both recorded items must be restored");
    assert_eq!(todos[0].title, "Write tests");
    assert_eq!(todos[0].status, TodoStatus::Pending);
    assert_eq!(todos[1].title, "Ship it");
    assert_eq!(todos[1].status, TodoStatus::InProgress);
}

/// Given a closed session whose wire log records a `tools.update_store`
/// (todo) op with capitalized statuses (`Pending` / `IN_PROGRESS`), when the
/// session is resumed, then the items round-trip to the canonical statuses.
#[tokio::test]
async fn should_restore_capitalized_statuses_when_resuming_a_session() {
    let env = TestEnv::new().await;
    let session = env
        .manager
        .create_session_with_id(
            SessionId::new("resume-todo-caps"),
            make_metadata("Todo Caps", &[]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();
    let session_id = session.id().clone();
    write_wire_log(
        &env.manager,
        &session_id,
        &[todo_store_op(json!([
            {"title": "Cap pending", "status": "Pending"},
            {"title": "Cap in progress", "status": "IN_PROGRESS"},
            {"title": "Cap done", "status": "Done"},
        ]))],
    )
    .await;
    env.manager
        .close_session(&session_id, CloseReason::Exit)
        .await
        .unwrap();

    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    let todos = resumed.todo_list().get();
    assert_eq!(todos.len(), 3);
    assert_eq!(todos[0].status, TodoStatus::Pending);
    assert_eq!(todos[1].status, TodoStatus::InProgress);
    assert_eq!(todos[2].status, TodoStatus::Done);
}

/// Given a closed session whose wire log records a valid todo store op
/// followed by a malformed one, when the session is resumed, then the
/// malformed op was skipped by the reducer and the first op's list survives.
#[tokio::test]
async fn should_keep_the_first_todo_list_when_a_later_store_op_is_malformed() {
    let env = TestEnv::new().await;
    let session = env
        .manager
        .create_session_with_id(
            SessionId::new("resume-todo-malformed"),
            make_metadata("Todo Malformed", &[]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();
    let session_id = session.id().clone();
    write_wire_log(
        &env.manager,
        &session_id,
        &[
            todo_store_op(json!([
                {"title": "Good item", "status": "pending"},
            ])),
            // A bad status never folds: the reducer's strict "todo" gate
            // skips the whole op rather than corrupting the stored list.
            todo_store_op(json!([
                {"title": "Bad item", "status": "blocked"},
            ])),
        ],
    )
    .await;
    env.manager
        .close_session(&session_id, CloseReason::Exit)
        .await
        .unwrap();

    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    let todos = resumed.todo_list().get();
    assert_eq!(
        todos,
        vec![TodoItem {
            title: "Good item".to_string(),
            status: TodoStatus::Pending,
        }],
        "the malformed op must be skipped; the first op's list survives"
    );
}

/// Given a closed session whose wire log records [A] → a real-user anchor →
/// [B], when the session is resumed and one turn is undone through the live
/// `session.undo` count semantics, then the live todo list rolls back to [A]
/// — the resume rebuilt the undo checkpoint stack from the folded state.
#[tokio::test]
async fn should_restore_the_todo_checkpoints_when_resuming_a_session() {
    let env = TestEnv::new().await;
    let session = env
        .manager
        .create_session_with_id(
            SessionId::new("resume-todo-checkpoints"),
            make_metadata("Todo Checkpoints", &[]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();
    let session_id = session.id().clone();
    write_wire_log(
        &env.manager,
        &session_id,
        &[
            todo_store_op(json!([
                {"title": "Before anchor", "status": "pending"},
            ])),
            // A real-user-input undo anchor: the fold snapshots the current
            // store value ([Before anchor]) onto todo_checkpoints.
            append_message_op("user", "go"),
            todo_store_op(json!([
                {"title": "After anchor", "status": "in_progress"},
            ])),
        ],
    )
    .await;
    env.manager
        .close_session(&session_id, CloseReason::Exit)
        .await
        .unwrap();

    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // The live list starts at the folded store value…
    let todos = resumed.todo_list().get();
    assert_eq!(todos.len(), 1);
    assert_eq!(todos[0].title, "After anchor");

    // …and the checkpoint stack was rebuilt: popping one depth (the
    // memory-side mirror of `context.undo` count 1) restores [Before
    // anchor].
    let applied = resumed.todo_list().pop_checkpoint(1);
    assert_eq!(applied, 1, "one checkpoint must have been restored");
    let rolled_back = resumed.todo_list().get();
    assert_eq!(
        rolled_back,
        vec![TodoItem {
            title: "Before anchor".to_string(),
            status: TodoStatus::Pending,
        }],
        "the popped checkpoint's list must be live again"
    );
}

}

mod session_ {
//! Integration tests for session lifecycle management (Task 32).
//!
//! These tests verify the session management layer that sits above the scope
//! hierarchy: `SessionManager` (create / resume / fork / close / list
//! sessions), `AgentLifecycle` (create / fork / remove / list agents), and
//! `SessionPersistence` (state.json save/load, wire log path computation).
//!
//! Tests use the Given-When-Then naming convention.

use std::collections::HashSet;
use std::sync::Arc;

use mirri_core::task::TaskKind;
use mirri_core::{
    AgentId, AgentLifecycle, AgentScopeConfig, AgentTask, AppScope, AppScopeConfig, CloseReason,
    ProviderType, SessionId, SessionManager, SessionMetadata, SessionPersistence,
    SessionScopeConfig, SessionStatus, TaskStatus, WorkspaceId, WorkspaceScopeConfig,
    WorkspaceTrustLevel,
};

// ===========================================================================
// Helper: create a workspace + session manager backed by a temp dir
// ===========================================================================

struct TestEnv {
    manager: SessionManager,
    _tmp: tempfile::TempDir,
}

impl TestEnv {
    async fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let app = AppScope::new(AppScopeConfig::default());
        let workspace = app
            .create_workspace_with_config(
                WorkspaceId::new("test-ws"),
                tmp.path().to_path_buf(),
                WorkspaceScopeConfig {
                    trust_level: WorkspaceTrustLevel::Trusted,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let manager = SessionManager::with_persistence_dir(
            workspace,
            tmp.path().join(".mirri").join("sessions"),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        Self { manager, _tmp: tmp }
    }
}

/// A resume base config carrying an explicit model (spec 11 R4): the
/// hand-written wire logs in these tests carry no model record, so the
/// base config seeds the model exactly the way the embedded host seeds it
/// from the config snapshot's `default_model` (the no-record fallback
/// chain: config default model → loud error).
fn resume_base_config() -> AgentScopeConfig {
    AgentScopeConfig {
        model_id: "test-resume-model".to_string(),
        ..AgentScopeConfig::default()
    }
}

fn make_metadata(title: &str, tags: &[&str]) -> SessionMetadata {
    let mut tag_set = HashSet::new();
    for tag in tags {
        tag_set.insert(tag.to_string());
    }
    SessionMetadata {
        title: title.to_string(),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        tags: tag_set,
        archived: false,
        is_custom_title: false,
        work_dir: None,
        last_prompt: None,
        forked_from: None,
        cache_root_id: None,
        custom: None,
        last_used_at: None,
    }
}

// ===========================================================================
// SessionManager — create + initial state
// ===========================================================================

#[tokio::test]
async fn should_have_initial_state_when_session_created() {
    // Given a fresh session manager
    let env = TestEnv::new().await;

    // When creating a session
    let metadata = make_metadata("My Session", &["test", "phase3"]);
    let session = env
        .manager
        .create_session(metadata, SessionScopeConfig::default())
        .await
        .unwrap();

    // Then the session has zero agents
    let agents = session.list_agents().await;
    assert_eq!(agents.len(), 0, "new session should have zero agents");

    // And the session is listed
    let sessions = env.manager.list_sessions().await;
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].agent_count, 0);
    assert_eq!(sessions[0].title, "My Session");
    assert!(sessions[0].tags.contains(&"test".to_string()));
}

// ===========================================================================
// AgentLifecycle — create multiple agents and verify isolation
// ===========================================================================

#[tokio::test]
async fn should_be_isolated_when_multiple_agents_created() {
    // Given a session
    let env = TestEnv::new().await;
    let session = env
        .manager
        .create_session(
            make_metadata("Multi-Agent", &[]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();

    // When creating two agents with different configs
    let lifecycle = AgentLifecycle::new(Arc::clone(&session));
    let agent1 = lifecycle
        .create_agent(AgentScopeConfig {
            model_id: "claude-sonnet-4-20250514".to_string(),
            provider_type: ProviderType::Anthropic,
            ..Default::default()
        })
        .await
        .unwrap();
    let agent2 = lifecycle
        .create_agent(AgentScopeConfig {
            model_id: "gpt-4o".to_string(),
            provider_type: ProviderType::OpenAi,
            ..Default::default()
        })
        .await
        .unwrap();

    // Then both agents exist with distinct IDs
    assert_ne!(agent1.id(), agent2.id());

    // And both agents are listed
    let agents = lifecycle.list_agents().await;
    assert_eq!(agents.len(), 2);

    // And each agent's config is independent
    let cm1 = agent1.context_memory();
    let cm2 = agent2.context_memory();
    assert!(
        !Arc::ptr_eq(&cm1, &cm2),
        "agents must not share context memory"
    );
}

// ===========================================================================
// AgentLifecycle — fork agent and verify context copy
// ===========================================================================

#[tokio::test]
async fn should_copy_context_when_agent_forked() {
    // Given a session with one agent that has context
    let env = TestEnv::new().await;
    let session = env
        .manager
        .create_session(
            make_metadata("Fork Test", &[]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();

    let lifecycle = AgentLifecycle::new(Arc::clone(&session));
    let source_agent = lifecycle
        .create_agent(AgentScopeConfig::default())
        .await
        .unwrap();

    // Add a message to the source agent's context memory
    {
        use mirri_core::message::ContentPart;
        use mirri_core::origin::PromptOrigin;

        let memory = source_agent.context_memory();
        let mut guard = memory.lock().unwrap();
        guard.append_user_message(
            vec![ContentPart::Text {
                text: "Hello from source agent".to_string(),
            }],
            PromptOrigin::User,
        );
    }

    // When forking the agent
    let forked_agent = lifecycle
        .fork_agent(&source_agent, AgentScopeConfig::default())
        .await
        .unwrap();

    // Then the forked agent has a different ID
    assert_ne!(source_agent.id(), forked_agent.id());

    // And the forked agent has the same context (history length)
    let source_history = {
        let memory = source_agent.context_memory();
        let guard = memory.lock().unwrap();
        guard.history().len()
    };
    let forked_history = {
        let memory = forked_agent.context_memory();
        let guard = memory.lock().unwrap();
        guard.history().len()
    };
    assert_eq!(
        source_history, forked_history,
        "forked agent should have the same number of messages as source"
    );

    // And both agents are in the session
    let agents = lifecycle.list_agents().await;
    assert_eq!(agents.len(), 2);
}

// ===========================================================================
// AgentLifecycle — remove agent and verify cleanup
// ===========================================================================

#[tokio::test]
async fn should_clean_up_agent_when_removed() {
    // Given a session with two agents
    let env = TestEnv::new().await;
    let session = env
        .manager
        .create_session(
            make_metadata("Remove Test", &[]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();

    let lifecycle = AgentLifecycle::new(Arc::clone(&session));
    let agent1 = lifecycle
        .create_agent(AgentScopeConfig::default())
        .await
        .unwrap();
    let _agent2 = lifecycle
        .create_agent(AgentScopeConfig::default())
        .await
        .unwrap();

    assert_eq!(lifecycle.list_agents().await.len(), 2);

    // When removing agent1
    lifecycle.remove_agent(agent1.id()).await.unwrap();

    // Then only one agent remains
    let remaining = lifecycle.list_agents().await;
    assert_eq!(remaining.len(), 1);
    assert_ne!(remaining[0].id, agent1.id().as_str());

    // And the removed agent cannot be retrieved
    assert!(session.get_agent(agent1.id()).await.is_none());
}

// ===========================================================================
// AgentLifecycle — remove non-existent agent returns error
// ===========================================================================

#[tokio::test]
async fn should_return_error_when_removing_nonexistent_agent() {
    let env = TestEnv::new().await;
    let session = env
        .manager
        .create_session(make_metadata("Empty", &[]), SessionScopeConfig::default())
        .await
        .unwrap();

    let lifecycle = AgentLifecycle::new(session);
    let fake_id = AgentId::new("nonexistent-agent");

    let result = lifecycle.remove_agent(&fake_id).await;
    assert!(result.is_err());
}

// ===========================================================================
// SessionManager — close session and verify cleanup
// ===========================================================================

#[tokio::test]
async fn should_remove_session_from_workspace_when_closed() {
    // Given a session
    let env = TestEnv::new().await;
    let session = env
        .manager
        .create_session(
            make_metadata("To Close", &[]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();
    let session_id = session.id().clone();

    // Verify it exists
    assert!(env.manager.get_session(&session_id).await.is_some());

    // When closing the session
    env.manager
        .close_session(&session_id, CloseReason::Exit)
        .await
        .unwrap();

    // Then the session is no longer in the workspace
    assert!(env.manager.get_session(&session_id).await.is_none());

    // And the session list is empty
    let sessions = env.manager.list_sessions().await;
    assert_eq!(sessions.len(), 0);
}

// ===========================================================================
// SessionManager — close non-existent session returns error
// ===========================================================================

#[tokio::test]
async fn should_return_error_when_closing_nonexistent_session() {
    let env = TestEnv::new().await;
    let fake_id = SessionId::new("nonexistent-session");

    let result = env
        .manager
        .close_session(&fake_id, CloseReason::UserInitiated)
        .await;
    assert!(result.is_err());
}

// ===========================================================================
// SessionManager — list sessions
// ===========================================================================

#[tokio::test]
async fn should_return_all_sessions_when_listed() {
    let env = TestEnv::new().await;

    // Create three sessions
    for i in 0..3 {
        env.manager
            .create_session(
                make_metadata(&format!("Session {}", i), &[]),
                SessionScopeConfig::default(),
            )
            .await
            .unwrap();
    }

    let sessions = env.manager.list_sessions().await;
    assert_eq!(sessions.len(), 3);

    // Each should have a unique ID
    let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
    let unique: std::collections::HashSet<&str> = ids.iter().copied().collect();
    assert_eq!(unique.len(), 3);
}

// ===========================================================================
// SessionManager — list agents in a session
// ===========================================================================

#[tokio::test]
async fn should_return_all_agents_with_info_when_listed() {
    let env = TestEnv::new().await;
    let session = env
        .manager
        .create_session(
            make_metadata("Agent List", &[]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();

    let lifecycle = AgentLifecycle::new(Arc::clone(&session));

    lifecycle
        .create_agent(AgentScopeConfig {
            model_id: "claude-sonnet-4-20250514".to_string(),
            provider_type: ProviderType::Anthropic,
            ..Default::default()
        })
        .await
        .unwrap();
    lifecycle
        .create_agent(AgentScopeConfig {
            model_id: "gpt-4o".to_string(),
            provider_type: ProviderType::OpenAi,
            ..Default::default()
        })
        .await
        .unwrap();

    let agents = lifecycle.list_agents().await;
    assert_eq!(agents.len(), 2);

    // Verify provider strings
    let providers: Vec<&str> = agents.iter().map(|a| a.provider.as_str()).collect();
    assert!(providers.contains(&"anthropic"));
    assert!(providers.contains(&"openai"));
}

// ===========================================================================
// SessionPersistence — save and load state.json
// ===========================================================================

#[tokio::test]
async fn should_roundtrip_metadata_when_state_saved_and_loaded() {
    let tmp = tempfile::tempdir().unwrap();
    let persistence = SessionPersistence::new(
        tmp.path().to_path_buf(),
        std::sync::Arc::new(mirri_os::LocalFs::new()),
    );

    let session_id = SessionId::new("persist-test-1");
    let metadata = make_metadata("Persisted Session", &["tag-a", "tag-b"]);

    // When saving state
    persistence
        .save_state(&session_id, &metadata)
        .await
        .unwrap();

    // Then loading state returns the same metadata
    let loaded = persistence.load_state(&session_id).await.unwrap();
    assert_eq!(loaded.title, "Persisted Session");
    assert_eq!(loaded.tags.len(), 2);
    assert!(loaded.tags.contains("tag-a"));
    assert!(loaded.tags.contains("tag-b"));
}

// ===========================================================================
// SessionPersistence — wire log path generation
// ===========================================================================

#[tokio::test]
async fn should_return_correct_paths_when_wire_log_path_computed() {
    let tmp = tempfile::tempdir().unwrap();
    let persistence = SessionPersistence::new(
        tmp.path().to_path_buf(),
        std::sync::Arc::new(mirri_os::LocalFs::new()),
    );

    let session_id = SessionId::new("path-test-session");

    let wire_path = persistence.wire_log_path(&session_id);
    let state_path = persistence.state_path(&session_id);

    assert_eq!(
        wire_path,
        tmp.path().join("path-test-session").join("wire.jsonl")
    );
    assert_eq!(
        state_path,
        tmp.path().join("path-test-session").join("state.json")
    );
}

// ===========================================================================
// SessionPersistence — delete session files
// ===========================================================================

#[tokio::test]
async fn should_remove_files_when_session_deleted() {
    let tmp = tempfile::tempdir().unwrap();
    let persistence = SessionPersistence::new(
        tmp.path().to_path_buf(),
        std::sync::Arc::new(mirri_os::LocalFs::new()),
    );

    let session_id = SessionId::new("delete-me");
    persistence
        .save_state(&session_id, &SessionMetadata::default())
        .await
        .unwrap();

    assert!(persistence.state_path(&session_id).exists());

    persistence.delete_session_files(&session_id).await.unwrap();

    assert!(!persistence.state_path(&session_id).exists());
}

// ===========================================================================
// SessionPersistence — list persisted sessions
// ===========================================================================

#[tokio::test]
async fn should_return_all_sessions_sorted_when_persisted_sessions_listed() {
    let tmp = tempfile::tempdir().unwrap();
    let persistence = SessionPersistence::new(
        tmp.path().to_path_buf(),
        std::sync::Arc::new(mirri_os::LocalFs::new()),
    );

    for i in 0..3 {
        let sid = SessionId::new(format!("persisted-{}", i));
        persistence
            .save_state(&sid, &make_metadata(&format!("S{}", i), &[]))
            .await
            .unwrap();
    }

    let sessions = persistence.list_persisted_sessions(false).await;
    assert_eq!(sessions.len(), 3);
    // Should be sorted by session_id
    assert_eq!(sessions[0].session_id, "persisted-0");
    assert_eq!(sessions[1].session_id, "persisted-1");
    assert_eq!(sessions[2].session_id, "persisted-2");
}

// ===========================================================================
// SessionPersistence — load state for non-existent session returns error
// ===========================================================================

#[tokio::test]
async fn should_return_error_when_loading_nonexistent_state_file() {
    let tmp = tempfile::tempdir().unwrap();
    let persistence = SessionPersistence::new(
        tmp.path().to_path_buf(),
        std::sync::Arc::new(mirri_os::LocalFs::new()),
    );

    let result = persistence.load_state(&SessionId::new("never-saved")).await;
    assert!(result.is_err());
}

// ===========================================================================
// SessionManager — resume (simplified: verifies flow without full replay)
// ===========================================================================

#[tokio::test]
async fn should_recreate_session_in_workspace_when_resumed() {
    let env = TestEnv::new().await;

    // Create and close a session (state.json is persisted on create)
    let session = env
        .manager
        .create_session(
            make_metadata("To Resume", &["resume"]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();
    let session_id = session.id().clone();
    let wire_log_path = env.manager.persistence().wire_log_path(&session_id);

    // Create the wire log file so resume validation passes
    tokio::fs::create_dir_all(wire_log_path.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(&wire_log_path, "").await.unwrap();

    // Close the session
    env.manager
        .close_session(&session_id, CloseReason::Exit)
        .await
        .unwrap();
    assert!(env.manager.get_session(&session_id).await.is_none());

    // When resuming (the base config carries the model the way the
    // embedded host seeds it from the config snapshot's `default_model` —
    // spec 11 R4: no built-in fallback)
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then the session is back in the workspace with the original metadata
    assert_eq!(resumed.id(), &session_id);
    let metadata = resumed.metadata().await;
    assert_eq!(metadata.title, "To Resume");
    assert!(metadata.tags.contains("resume"));
}

// ===========================================================================
// SessionManager — resume with missing wire log returns error
// ===========================================================================

#[tokio::test]
async fn should_return_error_when_resuming_with_missing_wire_log() {
    let env = TestEnv::new().await;

    // Persist state for a session
    let session_id = SessionId::new("resume-fail-test");
    env.manager
        .persistence()
        .save_state(&session_id, &make_metadata("Fail", &[]))
        .await
        .unwrap();

    // But don't create the wire log file

    let result = env.manager.resume_session(&session_id).await;
    assert!(result.is_err());
}

// ===========================================================================
// SessionManager — fork session (verify new session is created)
// ===========================================================================

#[tokio::test]
async fn should_create_new_session_with_agent_when_session_forked() {
    let env = TestEnv::new().await;

    // Create source session with an agent
    let source_session = env
        .manager
        .create_session(make_metadata("Source", &[]), SessionScopeConfig::default())
        .await
        .unwrap();

    let lifecycle = AgentLifecycle::new(Arc::clone(&source_session));
    let source_agent = lifecycle
        .create_agent(AgentScopeConfig {
            model_id: "claude-sonnet-4-20250514".to_string(),
            ..Default::default()
        })
        .await
        .unwrap();
    let source_agent_id = source_agent.id().clone();

    // When forking the session
    let forked_session = env
        .manager
        .fork_session(
            &source_session,
            &source_agent_id,
            make_metadata("Forked", &[]),
        )
        .await
        .unwrap();

    // Then the forked session has a different ID
    assert_ne!(source_session.id(), forked_session.id());

    // And the forked session has one agent (the forked copy)
    let forked_agents = forked_session.list_agents().await;
    assert_eq!(forked_agents.len(), 1);

    // And the source session still has its agent
    let source_agents = source_session.list_agents().await;
    assert_eq!(source_agents.len(), 1);

    // And both sessions are listed
    let sessions = env.manager.list_sessions().await;
    assert_eq!(sessions.len(), 2);
}

// ===========================================================================
// SessionManager — fork with non-existent agent returns error
// ===========================================================================

#[tokio::test]
async fn should_return_error_when_forking_with_nonexistent_agent() {
    let env = TestEnv::new().await;

    let source_session = env
        .manager
        .create_session(make_metadata("Source", &[]), SessionScopeConfig::default())
        .await
        .unwrap();

    let fake_agent_id = AgentId::new("nonexistent-agent");
    let result = env
        .manager
        .fork_session(
            &source_session,
            &fake_agent_id,
            make_metadata("Forked", &[]),
        )
        .await;

    assert!(result.is_err());
}

// ===========================================================================
// SessionManager — create session writes the metadata row
// ===========================================================================

/// Given a created session, then the metadata ROW (not `state.json` —
/// ADR-006: the row is the create path's durable record) exists and
/// carries the created title and tags.
#[tokio::test]
async fn should_persist_the_metadata_row_when_session_created() {
    let env = TestEnv::new().await;

    let session = env
        .manager
        .create_session(
            make_metadata("Persisted on Create", &["auto"]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();

    let state_path = env.manager.persistence().state_path(session.id());
    assert!(
        !state_path.exists(),
        "a fresh create writes the row, not state.json (ADR-006)"
    );

    // Verify the row matches
    let record = env
        .manager
        .metadata_store()
        .get(session.id())
        .await
        .expect("the created session's row exists");
    assert_eq!(record.title, "Persisted on Create");
    assert!(record.tags.contains(&"auto".to_string()));
}

// ===========================================================================
// SessionManager — concurrent session creation
// ===========================================================================

#[tokio::test]
async fn should_succeed_with_unique_ids_when_sessions_created_concurrently() {
    // Given a fresh session manager
    let tmp = tempfile::tempdir().unwrap();
    let app = AppScope::new(AppScopeConfig::default());
    let workspace = app
        .create_workspace_with_config(
            WorkspaceId::new("concurrent-ws"),
            tmp.path().to_path_buf(),
            WorkspaceScopeConfig::default(),
        )
        .await
        .unwrap();
    let manager = Arc::new(SessionManager::with_persistence_dir(
        workspace,
        tmp.path().join(".mirri").join("sessions"),
        std::sync::Arc::new(mirri_os::LocalFs::new()),
    ));

    // Spawn 10 concurrent session creation tasks
    let mut handles = Vec::new();

    for i in 0..10 {
        let m = Arc::clone(&manager);
        handles.push(tokio::spawn(async move {
            m.create_session(
                make_metadata(&format!("Concurrent {}", i), &[]),
                SessionScopeConfig::default(),
            )
            .await
        }));
    }

    // Wait for all to complete
    let mut session_ids = Vec::new();
    for handle in handles {
        let session = handle.await.unwrap().unwrap();
        session_ids.push(session.id().clone());
    }

    // All sessions should be in the workspace
    let sessions = manager.list_sessions().await;
    assert_eq!(
        sessions.len(),
        10,
        "all 10 concurrent sessions should exist"
    );

    // All IDs should be unique
    let unique_ids: std::collections::HashSet<&str> =
        session_ids.iter().map(|id| id.as_str()).collect();
    assert_eq!(unique_ids.len(), 10, "all session IDs should be unique");
}

// ===========================================================================
// SessionManager — get session returns the correct session
// ===========================================================================

#[tokio::test]
async fn should_return_correct_session_when_session_retrieved_by_id() {
    let env = TestEnv::new().await;

    let session = env
        .manager
        .create_session(make_metadata("Get Me", &[]), SessionScopeConfig::default())
        .await
        .unwrap();
    let session_id = session.id().clone();

    let retrieved = env.manager.get_session(&session_id).await;
    assert!(retrieved.is_some());
    assert_eq!(retrieved.unwrap().id(), &session_id);

    let not_found = env
        .manager
        .get_session(&SessionId::new("nonexistent"))
        .await;
    assert!(not_found.is_none());
}

// ===========================================================================
// SessionManager — SessionInfo fields are correct
// ===========================================================================

#[tokio::test]
async fn should_have_correct_agent_count_when_sessions_listed() {
    let env = TestEnv::new().await;

    let session = env
        .manager
        .create_session(
            make_metadata("Info Test", &["alpha", "beta"]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();

    let lifecycle = AgentLifecycle::new(Arc::clone(&session));
    lifecycle
        .create_agent(AgentScopeConfig::default())
        .await
        .unwrap();
    lifecycle
        .create_agent(AgentScopeConfig::default())
        .await
        .unwrap();
    lifecycle
        .create_agent(AgentScopeConfig::default())
        .await
        .unwrap();

    let sessions = env.manager.list_sessions().await;
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].agent_count, 3);
    assert_eq!(sessions[0].title, "Info Test");
    assert!(sessions[0].tags.contains(&"alpha".to_string()));
    assert!(sessions[0].tags.contains(&"beta".to_string()));
}

// ===========================================================================
// Session lifecycle external command hooks (SessionStart / SessionEnd)
// ===========================================================================

/// Build a manager whose workspace carries the given external command hooks.
async fn manager_with_hooks(
    hooks: Vec<mirri_core::hook_engine::HookDef>,
) -> (SessionManager, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let app = AppScope::new(AppScopeConfig::default());
    let workspace = app
        .create_workspace_with_config(
            WorkspaceId::new("hooks-ws"),
            tmp.path().to_path_buf(),
            WorkspaceScopeConfig {
                trust_level: WorkspaceTrustLevel::Trusted,
                hooks,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let manager = SessionManager::with_persistence_dir(
        workspace,
        tmp.path().join(".mirri").join("sessions"),
        std::sync::Arc::new(mirri_os::LocalFs::new()),
    );
    (manager, tmp)
}

#[tokio::test]
async fn should_fire_session_start_hook_when_session_created() {
    use mirri_core::hook_engine::{HookDef, HookEventType};

    // Given a workspace with a SessionStart hook that dumps its JSON input
    // to a marker file
    let marker_path =
        std::env::temp_dir().join(format!("session_start_hook_{}.json", std::process::id()));
    let _ = std::fs::remove_file(&marker_path);
    let (manager, _tmp) = manager_with_hooks(vec![HookDef::new(
        HookEventType::SessionStart,
        format!("cat > {:?}", marker_path),
    )])
    .await;

    // When a session is created
    manager
        .create_session(make_metadata("Hooked", &[]), SessionScopeConfig::default())
        .await
        .expect("session should be created");

    // Then the hook fired with the TS input shape { source: "startup" }
    let content = std::fs::read_to_string(&marker_path).expect("hook input file should exist");
    let input: serde_json::Value =
        serde_json::from_str(&content).expect("hook input should be JSON");
    assert_eq!(input["source"], serde_json::json!("startup"));
    let _ = std::fs::remove_file(&marker_path);
}

#[tokio::test]
async fn should_fire_session_start_hook_when_resuming_session() {
    use mirri_core::hook_engine::{HookDef, HookEventType};
    use mirri_persistence::JsonlAppendLog;

    // Given a workspace with a SessionStart hook that dumps its JSON input
    // to a marker file, and a persisted session that has been closed
    let marker_path =
        std::env::temp_dir().join(format!("session_resume_hook_{}.json", std::process::id()));
    let _ = std::fs::remove_file(&marker_path);
    let (manager, _tmp) = manager_with_hooks(vec![HookDef::new(
        HookEventType::SessionStart,
        format!("cat > {:?}", marker_path),
    )])
    .await;

    let session = manager
        .create_session(
            make_metadata("Resume Me", &[]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();
    let session_id = session.id().clone();
    manager
        .close_session(&session_id, CloseReason::Exit)
        .await
        .expect("session should close");

    // An empty wire log is enough for resume to reconstruct an empty agent.
    let wire_log_path = manager.persistence().wire_log_path(&session_id);
    JsonlAppendLog::create(&wire_log_path)
        .await
        .expect("create wire log");

    // When the session is resumed (the base config carries the model the
    // way the embedded host seeds it from the config snapshot's
    // `default_model` — spec 11 R4: no built-in fallback)
    let resumed = manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await;
    assert!(
        resumed.is_ok(),
        "resume should succeed: {:?}",
        resumed.err()
    );

    // Then a SessionStart fired with { source: "resume" }
    let content = std::fs::read_to_string(&marker_path).expect("hook input file should exist");
    let input: serde_json::Value =
        serde_json::from_str(&content).expect("hook input should be JSON");
    assert_eq!(input["source"], serde_json::json!("resume"));
    let _ = std::fs::remove_file(&marker_path);
}

#[tokio::test]
async fn should_fire_session_end_hook_when_session_closed() {
    use mirri_core::hook_engine::{HookDef, HookEventType};

    // Given a workspace with a SessionEnd hook that dumps its JSON input to
    // a marker file
    let marker_path =
        std::env::temp_dir().join(format!("session_end_hook_{}.json", std::process::id()));
    let _ = std::fs::remove_file(&marker_path);
    let (manager, _tmp) = manager_with_hooks(vec![HookDef::new(
        HookEventType::SessionEnd,
        format!("cat > {:?}", marker_path),
    )])
    .await;

    let session = manager
        .create_session(make_metadata("Closing", &[]), SessionScopeConfig::default())
        .await
        .unwrap();

    // When the session is closed by the user
    manager
        .close_session(session.id(), CloseReason::UserInitiated)
        .await
        .expect("session should close");

    // Then the hook fired with the TS input shape { reason: "user" }
    let content = std::fs::read_to_string(&marker_path).expect("hook input file should exist");
    let input: serde_json::Value =
        serde_json::from_str(&content).expect("hook input should be JSON");
    assert_eq!(input["reason"], serde_json::json!("user"));
    let _ = std::fs::remove_file(&marker_path);
}

// ===========================================================================
// Task persistence — background tasks survive close/resume
// ===========================================================================

#[tokio::test]
async fn should_persist_and_reload_tasks_when_session_closed_and_resumed() {
    // Given a session with a registered background task and appended output
    let env = TestEnv::new().await;
    let session = env
        .manager
        .create_session(make_metadata("Tasks", &[]), SessionScopeConfig::default())
        .await
        .unwrap();
    let task_id = session.tasks().register(
        TaskKind::Process {
            command: None,
            dispatcher: None,
        },
        "build watcher".into(),
        session.id().to_string(),
    );
    session.tasks().append_output(&task_id, "line one\n");

    // Then the task is persisted under `<session_dir>/tasks/`
    let session_dir = env.manager.persistence().session_dir(session.id());
    let task_path = session_dir.join("tasks").join(format!("{task_id}.json"));
    assert!(
        task_path.exists(),
        "register must persist the task metadata JSON"
    );
    let log_path = session_dir.join("tasks").join(&task_id).join("output.log");
    assert_eq!(
        std::fs::read_to_string(&log_path).unwrap(),
        "line one\n",
        "appended output must land in the task's append-only log"
    );

    // When the session closes and is resumed (the base config carries the
    // model the way the embedded host seeds it from the config snapshot's
    // `default_model` — spec 11 R4: no built-in fallback)
    let session_id = session.id().clone();
    env.manager
        .close_session(&session_id, CloseReason::UserInitiated)
        .await
        .expect("session should close");
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then the task is reloaded, and the still-running task is reconciled
    // to stopped because no live process owns it after the restart
    let task = resumed.tasks().get(&task_id).expect("task reloaded");
    assert_eq!(task.status, TaskStatus::Stopped);
    let on_disk: AgentTask =
        serde_json::from_str(&std::fs::read_to_string(&task_path).unwrap()).unwrap();
    assert_eq!(on_disk.status, TaskStatus::Stopped);
    assert_eq!(on_disk.output, "line one\n");
}

// ===========================================================================
// G32 — list_all_sessions (merged persisted + active)
// ===========================================================================

#[tokio::test]
async fn should_list_persisted_sessions_when_no_active_sessions_exist() {
    // Given several persisted sessions (created then closed, so they exist
    // on disk but not in the workspace)
    let env = TestEnv::new().await;

    let session1 = env
        .manager
        .create_session(
            make_metadata("Persisted One", &[]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();
    let session2 = env
        .manager
        .create_session(
            make_metadata("Persisted Two", &[]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();

    // Close both sessions so they are persisted but not active
    env.manager
        .close_session(session1.id(), CloseReason::Exit)
        .await
        .unwrap();
    env.manager
        .close_session(session2.id(), CloseReason::Exit)
        .await
        .unwrap();

    // When listing all sessions
    let sessions = env.manager.list_all_sessions(false).await;

    // Then both persisted sessions are returned
    assert_eq!(sessions.len(), 2, "should list both persisted sessions");

    // And both have the Closed status
    for info in &sessions {
        assert_eq!(
            info.status,
            SessionStatus::Closed,
            "persisted-only sessions should be Closed"
        );
    }

    // And the titles are correct
    let titles: Vec<&str> = sessions.iter().map(|s| s.title.as_str()).collect();
    assert!(titles.contains(&"Persisted One"));
    assert!(titles.contains(&"Persisted Two"));
}

#[tokio::test]
async fn should_mark_active_sessions_as_running_when_listed() {
    // Given one active session and two persisted-only sessions
    let env = TestEnv::new().await;

    // Create and close two sessions (they become persisted-only)
    let closed1 = env
        .manager
        .create_session(
            make_metadata("Closed One", &[]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();
    let closed2 = env
        .manager
        .create_session(
            make_metadata("Closed Two", &[]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();
    env.manager
        .close_session(closed1.id(), CloseReason::Exit)
        .await
        .unwrap();
    env.manager
        .close_session(closed2.id(), CloseReason::Exit)
        .await
        .unwrap();

    // Create a third session that stays active
    let active = env
        .manager
        .create_session(
            make_metadata("Active One", &[]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();

    // When listing all sessions
    let sessions = env.manager.list_all_sessions(false).await;

    // Then three sessions are returned
    assert_eq!(sessions.len(), 3, "should list active + persisted sessions");

    // And the active session is marked as Running
    let active_info = sessions
        .iter()
        .find(|s| s.id == active.id().as_str())
        .expect("active session should be in the list");
    assert_eq!(
        active_info.status,
        SessionStatus::Running,
        "active session should be Running"
    );
    assert_eq!(active_info.title, "Active One");

    // And the persisted-only sessions are marked as Closed
    for info in sessions.iter().filter(|s| s.id != active.id().as_str()) {
        assert_eq!(
            info.status,
            SessionStatus::Closed,
            "persisted-only sessions should be Closed"
        );
    }
}

#[tokio::test]
async fn should_return_empty_list_when_no_sessions_exist() {
    // Given a fresh session manager with no sessions (active or persisted)
    let env = TestEnv::new().await;

    // When listing all sessions
    let sessions = env.manager.list_all_sessions(false).await;

    // Then an empty list is returned
    assert!(
        sessions.is_empty(),
        "should return empty list when no sessions exist"
    );
}

#[tokio::test]
async fn should_sort_sessions_by_created_at_descending_when_listed() {
    // Given three persisted sessions with different creation times
    let env = TestEnv::new().await;

    let s1 = env
        .manager
        .create_session(make_metadata("First", &[]), SessionScopeConfig::default())
        .await
        .unwrap();
    // Small delay to ensure distinct timestamps
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    let s2 = env
        .manager
        .create_session(make_metadata("Second", &[]), SessionScopeConfig::default())
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    let s3 = env
        .manager
        .create_session(make_metadata("Third", &[]), SessionScopeConfig::default())
        .await
        .unwrap();

    // Close all so they are persisted-only
    env.manager
        .close_session(s1.id(), CloseReason::Exit)
        .await
        .unwrap();
    env.manager
        .close_session(s2.id(), CloseReason::Exit)
        .await
        .unwrap();
    env.manager
        .close_session(s3.id(), CloseReason::Exit)
        .await
        .unwrap();

    // When listing all sessions
    let sessions = env.manager.list_all_sessions(false).await;

    // Then they are sorted by created_at descending (newest first)
    assert_eq!(sessions.len(), 3);
    assert_eq!(sessions[0].title, "Third", "newest session should be first");
    assert_eq!(
        sessions[1].title, "Second",
        "middle session should be second"
    );
    assert_eq!(sessions[2].title, "First", "oldest session should be last");
}

// ===========================================================================
// SessionManager — read-only per-turn usage fold
// ===========================================================================

#[tokio::test]
async fn should_fold_per_turn_usage_when_reading_a_closed_session_wire_log() {
    // Given a persisted session whose wire log carries one completed turn
    // with a single step (four usage buckets) plus an out-of-bracket
    // user injection — `manager_with_hooks(vec![])` is the file's plain
    // "manager + persisted sessions + temp dir kept alive" helper
    // (create installs the wire log eagerly, so `record_op` is durable).
    let (manager, tmp) = manager_with_hooks(vec![]).await;
    let session = manager
        .create_session(
            make_metadata("Per-Turn Usage Fold", &[]),
            SessionScopeConfig::default(),
        )
        .await
        .unwrap();
    let session_id = session.id().clone();
    session
        .record_op(mirri_proto::WireOp::ContextAppendMessage {
            message: serde_json::json!({
                "role": "user",
                "content": "injected",
                "origin": { "kind": "injection" },
            }),
        })
        .await;
    session
        .record_op(mirri_proto::WireOp::TurnPrompt {
            input: serde_json::json!("hi"),
            origin: serde_json::json!({ "kind": "user" }),
        })
        .await;
    session
        .record_op(mirri_proto::WireOp::ContextAppendMessage {
            message: serde_json::json!({
                "role": "user",
                "content": "hi",
                "origin": { "kind": "user" },
            }),
        })
        .await;
    session
        .record_op(mirri_proto::WireOp::ContextAppendLoopEvent {
            event: serde_json::json!({
                "type": "step.end",
                "uuid": "step-1",
                "usage": {
                    "inputTokens": 10,
                    "outputTokens": 5,
                    "cacheReadTokens": 1000,
                    "cacheCreationTokens": 200,
                },
            }),
        })
        .await;
    session
        .record_op(mirri_proto::WireOp::UsageRecord {
            model: "kimi-k2".to_string(),
            usage: serde_json::json!({
                "inputTokens": 10,
                "outputTokens": 5,
                "cacheReadTokens": 1000,
                "cacheCreationTokens": 200,
            }),
            usage_scope: None,
        })
        .await;
    session
        .record_op(mirri_proto::WireOp::TurnEnded {
            turn_id: 1,
            reason: "completed".to_string(),
            error: None,
            duration_ms: Some(1200),
            ended_at: None,
        })
        .await;
    manager
        .close_session(&session_id, CloseReason::Exit)
        .await
        .expect("session closes to disk");
    drop(session);
    drop(manager);

    // When a fresh manager reads the turn-usage fold of the same session
    // (same cwd + persistence dir as the original, so the wire log path
    // resolves identically)
    let app = AppScope::new(AppScopeConfig::default());
    let workspace = app
        .create_workspace_with_config(
            WorkspaceId::new("fold-ws"),
            tmp.path().to_path_buf(),
            WorkspaceScopeConfig::default(),
        )
        .await
        .unwrap();
    let manager = SessionManager::with_persistence_dir(
        workspace,
        tmp.path().join(".mirri").join("sessions"),
        std::sync::Arc::new(mirri_os::LocalFs::new()),
    );
    let fold = manager
        .read_only_turn_usages(&session_id)
        .await
        .expect("the wire log folds");

    // Then the completed turn's buckets, model, and opener anchor are back
    assert_eq!(fold.turns.len(), 1);
    assert_eq!(fold.turns[0].turn_key, "turn-1");
    assert_eq!(fold.turns[0].input_tokens, 10);
    assert_eq!(fold.turns[0].output_tokens, 5);
    assert_eq!(fold.turns[0].cache_read_tokens, 1000);
    assert_eq!(fold.turns[0].cache_creation_tokens, 200);
    assert_eq!(fold.turns[0].models, vec!["kimi-k2"]);
    assert_eq!(fold.openers.len(), 1);
    assert_eq!(fold.openers[0].content_sig, serde_json::json!("hi"));
}

}

mod turn_flow_ {
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
use mirri_core::tool_registry::{
    PrepareResult, ToolManager, ValidatorCache, execute_tool_batch, prepare_tool_call,
};
use mirri_core::todo::{TodoItem, TodoList, TodoStatus};
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
async fn should_stamp_a_wall_clock_completion_instant_when_a_turn_runs_to_completion() {
    // Given a flow about to run one turn through the real engine harness
    let llm = MockLlm::new(vec![MockResponse::end_turn("Hello!")]);
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());

    // When the turn runs to completion between two wall-clock readings
    let before_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the test clock must be after the Unix epoch")
        .as_millis() as u64;
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
    let after_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("the test clock must be after the Unix epoch")
        .as_millis() as u64;

    // Then the settled turn carries a real wall-clock instant (epoch
    // milliseconds) inside the window the turn executed in
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    assert!(
        (before_ms..=after_ms).contains(&result.ended_at),
        "ended_at {} should lie within the turn's execution window [{before_ms}, {after_ms}]",
        result.ended_at
    );
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
        mirri_core::SessionEvent::TurnStepStarted { turn_id: _, step, agent_id: _ } => {
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
        mirri_core::SessionEvent::TurnStepStarted { turn_id, step, agent_id } => {
            assert_eq!(turn_id, "turn-abc-123");
            assert_eq!(step, 1);
            assert_eq!(agent_id, "main", "a standalone flow's steps publish as the main agent");
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
/// `context.append_message` for EVERY flushed steer (user steers included —
/// CR13: the flush itself persists the fold, so a crash between flush and
/// turn end cannot strand a steer outside the context).
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
    // turn-long flow mutex). The busy-path admission tags user steers with
    // the user_steer origin.
    {
        let buffer = turn_flow.steer_buffer_handle();
        let mut buf = buffer.lock().unwrap_or_else(|e| e.into_inner());
        buf.push(
            vec![ContentPart::Text {
                text: "user steer".to_string(),
            }],
            PromptOrigin::UserSteer,
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
    // …followed by exactly one durable append per flushed steer — the user
    // steer records its own row too (CR13).
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
    // Both flushed steers recorded durable appends (the user steer's row is
    // recorded too — CR13).
    let all_appends = ops
        .iter()
        .filter(|op| matches!(op, WireOp::ContextAppendMessage { .. }))
        .count();
    assert_eq!(all_appends, 2, "both flushed steers must record durable appends");

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

/// Given a turn flow whose steer buffer parks one user steer and one
/// background-task notification steer (FIFO), when the flush runs
/// (`before_step`), then EVERY flushed steer lands its own durable
/// `context.append_message` — user steers included (CR13: a steer folded
/// into the running turn must survive a crash between flush and the turn's
/// own context append) — each ordered after the `turn.steer_flushed`
/// marker, with the origin serialized as recorded (`{"kind":"user_steer"}`
/// for the busy-path user steer, the background_task shape for the
/// notification).
#[tokio::test]
async fn should_record_a_durable_append_for_every_steer_when_the_flush_consumes_user_steers() {
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

    // One user steer and one notification steer, parked in FIFO order.
    {
        let buffer = turn_flow.steer_buffer_handle();
        let mut buf = buffer.lock().unwrap_or_else(|e| e.into_inner());
        buf.push(
            vec![ContentPart::Text {
                text: "user steer".to_string(),
            }],
            PromptOrigin::UserSteer,
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

    // Then the consumption marker precedes BOTH durable appends, and each
    // steer's append carries its own recorded origin (user_steer for the
    // busy-path user steer, background_task for the notification).
    let ops = recorded.lock().unwrap_or_else(|e| e.into_inner());
    let flush_at = ops
        .iter()
        .position(|op| matches!(op, WireOp::TurnSteerFlushed { count: 2 }))
        .expect("a turn.steer_flushed { count: 2 } marker must be recorded");
    let steer_appends: Vec<&serde_json::Value> = ops
        .iter()
        .enumerate()
        .filter(|(i, op)| {
            i > &flush_at
                && matches!(op, WireOp::ContextAppendMessage { message }
                    if message["origin"]["kind"] == "user_steer"
                        || message["origin"]["kind"] == "background_task")
        })
        .map(|(_, op)| match op {
            WireOp::ContextAppendMessage { message } => message,
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(
        steer_appends.len(),
        2,
        "both flushed steers must record durable appends: {steer_appends:?}"
    );
    assert_eq!(
        steer_appends[0]["origin"],
        serde_json::json!({ "kind": "user_steer" }),
        "the user steer's append must serialize the user_steer origin"
    );
    assert_eq!(
        steer_appends[0]["content"][0]["text"], "user steer",
        "the user steer's append must carry the steer content"
    );
    assert_eq!(
        steer_appends[1]["origin"]["kind"], "background_task",
        "the notification's append keeps the background_task shape"
    );
    assert_eq!(
        steer_appends[1]["role"], "user",
        "steer appends land as user-role rows"
    );
}

/// Given a hooks instance with a wire recorder and a parked user steer,
/// when the stop boundary flushes it (`should_continue_after_stop` Layer 0),
/// then the flush records the durable consumption marker followed by the
/// steer's own `context.append_message` (CR13) BEFORE the steer enters
/// context memory — write-before-mutate, so a crash between the stop
/// boundary and the continuation's context append still replays the row.
#[tokio::test]
async fn should_record_a_durable_append_when_the_stop_boundary_flushes_a_user_steer() {
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let (hooks, steer_buffer) =
        make_hooks_with_goal_and_steer_and_recorder(memory.clone(), None, Arc::clone(&recorded));

    steer_buffer.lock().unwrap_or_else(|e| e.into_inner()).push(
        vec![ContentPart::Text {
            text: "new direction".to_string(),
        }],
        PromptOrigin::UserSteer,
    );

    let cancel = CancelToken::new();
    let ctx = make_continue_ctx(&cancel);

    let result = hooks.should_continue_after_stop(ctx).await.unwrap();
    assert!(
        result.continue_turn,
        "a flushed user steer must still continue the turn"
    );

    // Then the recorder saw the marker followed by the steer's durable
    // append, in that order.
    let ops = recorded.lock().unwrap_or_else(|e| e.into_inner());
    let flush_at = ops
        .iter()
        .position(|op| matches!(op, WireOp::TurnSteerFlushed { count: 1 }))
        .expect("the stop-boundary flush must record its consumption marker");
    let append_at = ops
        .iter()
        .position(
            |op| matches!(op, WireOp::ContextAppendMessage { message }
                if message["origin"]["kind"] == "user_steer"),
        )
        .expect("the flushed user steer must record a durable append");
    assert!(
        append_at > flush_at,
        "the durable append must follow the consumption marker: {ops:?}"
    );
    let WireOp::ContextAppendMessage { message } = &ops[append_at] else {
        unreachable!()
    };
    assert_eq!(message["role"], "user");
    assert_eq!(message["content"][0]["text"], "new direction");
    assert_eq!(message["origin"], serde_json::json!({ "kind": "user_steer" }));
}

/// Given a hooks instance with a wired todo list and a parked user steer,
/// when the stop boundary flushes it, then the live todo undo stack gains
/// one checkpoint — the memory-side mirror of the reducer's fold (a
/// user_steer `context.append_message` anchors and checkpoints), so a
/// later `session.undo` and a resume that rebuilds from the wire agree on
/// the stack depth (live ≡ replay, ADR-009).
#[tokio::test]
async fn should_push_a_todo_undo_checkpoint_when_the_stop_boundary_flushes_a_user_steer() {
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let todo_list = Arc::new(TodoList::new());
    todo_list.set(vec![TodoItem {
        title: "before steer".to_string(),
        status: TodoStatus::Pending,
    }]);
    let (hooks, steer_buffer) = make_hooks_with_goal_and_steer_and_recorder_and_todo(
        memory.clone(),
        None,
        Arc::clone(&recorded),
        Arc::clone(&todo_list),
    );

    steer_buffer.lock().unwrap_or_else(|e| e.into_inner()).push(
        vec![ContentPart::Text {
            text: "new direction".to_string(),
        }],
        PromptOrigin::UserSteer,
    );

    let cancel = CancelToken::new();
    let ctx = make_continue_ctx(&cancel);

    let result = hooks.should_continue_after_stop(ctx).await.unwrap();
    assert!(result.continue_turn, "the flushed user steer continues the turn");

    // Then the live undo stack holds exactly the flush's checkpoint: one
    // push, and popping it restores the pre-steer list.
    let applied = todo_list.pop_checkpoint(1);
    assert_eq!(
        applied, 1,
        "exactly one live checkpoint was pushed by the flush"
    );
    let items = todo_list.get();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].title, "before steer");
    assert_eq!(
        todo_list.pop_checkpoint(1),
        0,
        "no second checkpoint exists — exactly one push per steer"
    );
}

/// Given a mixed steer buffer [notification A, user steer B, notification C]
/// and a wire recorder, when the stop boundary flushes (Layer 0 flushes only
/// B and re-parks A, C), then the wire log records a consumption marker for
/// ALL THREE takes (the reducer's FIFO drain consumes A, B, C — the drain
/// must cover exactly what was taken, or a resume's pending set diverges
/// from the live parked set) followed by a FRESH `turn.steer` record for
/// each re-parked notification, and a durable append for the flushed user
/// steer only — so a crash+resume re-parks exactly {A, C} and never
/// duplicates B (replay faithfulness, ADR-009).
#[tokio::test]
async fn should_record_a_replay_faithful_flush_when_the_stop_boundary_flushes_a_mixed_buffer() {
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let (hooks, steer_buffer) =
        make_hooks_with_goal_and_steer_and_recorder(memory.clone(), None, Arc::clone(&recorded));

    let notification = |task: &str| PromptOrigin::BackgroundTask {
        task_id: task.to_string(),
        status: "completed".to_string(),
        notification_id: format!("task:{task}:completed"),
    };
    {
        let mut buf = steer_buffer.lock().unwrap_or_else(|e| e.into_inner());
        buf.push(
            vec![ContentPart::Text {
                text: "notification A".to_string(),
            }],
            notification("t1"),
        );
        buf.push(
            vec![ContentPart::Text {
                text: "user steer B".to_string(),
            }],
            PromptOrigin::UserSteer,
        );
        buf.push(
            vec![ContentPart::Text {
                text: "notification C".to_string(),
            }],
            notification("t2"),
        );
    }

    let cancel = CancelToken::new();
    let ctx = make_continue_ctx(&cancel);
    let result = hooks.should_continue_after_stop(ctx).await.unwrap();
    assert!(
        result.continue_turn,
        "the flushed user steer must still continue the turn"
    );

    // Then the wire log's ops replay to exactly the live parked set.
    let ops = recorded.lock().unwrap_or_else(|e| e.into_inner());
    let ops = ops.clone();
    // Live parked set: the two notifications (FIFO order preserved).
    let parked_texts: Vec<String> = {
        let buf = steer_buffer.lock().unwrap_or_else(|e| e.into_inner());
        buf.iter()
            .map(|s| match &s.content[0] {
                ContentPart::Text { text } => text.clone(),
                _ => String::new(),
            })
            .collect()
    };
    assert_eq!(
        parked_texts,
        vec!["notification A".to_string(), "notification C".to_string()],
        "the live buffer still parks both notifications in FIFO order"
    );

    // Replay the recorded ops through the reducer (the resume's fold). The
    // wire log the flush window rides carries the three `turn.steer`
    // parking records FIRST (the busy-path admission recorded them when
    // each steer parked), then the flush window's ops.
    let mut ops_with_parks: Vec<WireOp> = vec![
        WireOp::TurnSteer {
            input: serde_json::json!([{ "type": "text", "text": "notification A" }]),
            origin: serde_json::json!({
                "kind": "background_task",
                "taskId": "t1",
                "status": "completed",
                "notificationId": "task:t1:completed",
            }),
        },
        WireOp::TurnSteer {
            input: serde_json::json!([{ "type": "text", "text": "user steer B" }]),
            origin: serde_json::json!({ "kind": "user_steer" }),
        },
        WireOp::TurnSteer {
            input: serde_json::json!([{ "type": "text", "text": "notification C" }]),
            origin: serde_json::json!({
                "kind": "background_task",
                "taskId": "t2",
                "status": "completed",
                "notificationId": "task:t2:completed",
            }),
        },
    ];
    ops_with_parks.extend(ops.iter().cloned());
    let mut state = mirri_proto::AgentState::new();
    for op in &ops_with_parks {
        let (next, _events) = mirri_proto::reduce(&state, op);
        state = next;
    }
    // …then the pending set equals the live parked set — same entries, same
    // order, no duplicates of the flushed user steer B.
    let mut replayed: Vec<String> = Vec::new();
    let mut replayed_origins: Vec<serde_json::Value> = Vec::new();
    for entry in &state.steer_pending {
        replayed.push(
            entry["input"]
                .as_array()
                .and_then(|parts| parts.first())
                .and_then(|p| p.get("text"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_string(),
        );
        replayed_origins.push(entry["origin"].clone());
    }
    assert_eq!(
        replayed, parked_texts,
        "the replayed pending set must equal the live parked set"
    );
    assert_eq!(
        replayed_origins
            .iter()
            .filter(|o| o["kind"] == "background_task")
            .count(),
        2,
        "both re-parked entries keep the background_task origin"
    );
    assert!(
        !replayed.iter().any(|t| t.contains("user steer B")),
        "the flushed user steer must NOT re-park on replay (no duplicate row)"
    );
    // And the flushed user steer's durable append is recorded exactly once.
    let steer_appends = ops
        .iter()
        .filter(|op| matches!(op, WireOp::ContextAppendMessage { message }
            if message["origin"]["kind"] == "user_steer"))
        .count();
    assert_eq!(steer_appends, 1, "exactly one durable append for steer B");
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
        tool_readonly: false,
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
        tool_readonly: false,
        args: &args,
        approval_rule: "",
        display: &display,
        cancel: cancel.clone(),
    };

    let result = hooks.authorize_tool_execution(ctx).await.unwrap();
    assert!(
        matches!(result, AuthorizeToolResult::Approved { metadata: None }),
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
        tool_readonly: false,
        args: &args,
        approval_rule: "",
        display: &display,
        cancel: cancel.clone(),
    };

    let result = hooks.authorize_tool_execution(ctx).await.unwrap();
    assert!(
        matches!(result, AuthorizeToolResult::Approved { metadata: None }),
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
        tool_readonly: false,
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
        tool_readonly: false,
        args: &args,
        approval_rule: "",
        display: &display,
        cancel: cancel.clone(),
    };

    let result = hooks.authorize_tool_execution(ctx).await.unwrap();
    assert!(
        matches!(result, AuthorizeToolResult::Approved { metadata: None }),
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

/// A counting mock callback — proves the approver was never consulted
/// (session memory alone resolved the ask).
struct CountingNeverCalledCallback {
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl mirri_core::reverse_rpc::ReverseRpcCallback for CountingNeverCalledCallback {
    async fn call(&self, _method: &str, _params: Value) -> Result<Value, String> {
        use std::sync::atomic::Ordering;
        self.calls.fetch_add(1, Ordering::SeqCst);
        panic!("session-memory approval must never consult the approver")
    }
}

/// Given a private-network FetchURL (IP literal, no DNS), when the ask is
/// answered approved with session scope, then the authorize result carries
/// the `private_network_exemption` metadata into execution (ADR-010 B1).
#[tokio::test]
async fn should_carry_exemption_when_private_fetch_approved_via_ask() {
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let pm = Arc::new(PermissionManager::new(PermissionMode::Manual, vec![]));
    let mock = Arc::new(MockReverseRpcCallback {
        response: serde_json::json!({ "decision": "approved", "scope": "session" }),
        last_method: Mutex::new(None),
        last_params: Mutex::new(None),
    });
    let callback: Arc<dyn mirri_core::reverse_rpc::ReverseRpcCallback> = mock.clone();
    let hooks = make_hooks_with_permission_and_callback(memory, Some(pm), Some(callback));
    let cancel = CancelToken::new();
    let args = serde_json::json!({"url": "http://127.0.0.1:9/x"});
    let display = ToolInputDisplay::Generic {
        summary: "fetch local".into(),
    };
    let ctx = AuthorizeToolContext {
        turn_id: "1".to_string(),
        step: 1,
        tool_call_id: "call-fetch-1".to_string(),
        tool_name: "FetchURL".to_string(),
        tool_readonly: false,
        args: &args,
        approval_rule: "FetchURL(host:127.0.0.1)",
        display: &display,
        cancel: cancel.clone(),
    };

    let result = hooks.authorize_tool_execution(ctx).await.unwrap();
    let AuthorizeToolResult::Approved { metadata } = result else {
        panic!("expected Approved, got {result:?}")
    };
    let metadata = metadata.expect("an approved private fetch must carry exemption metadata");
    assert_eq!(metadata["private_network_exemption"]["host"], "127.0.0.1");
    assert_eq!(
        metadata["private_network_exemption"]["approved_ips"],
        serde_json::json!(["127.0.0.1"])
    );

    // The ask reached the approver through the reverse-RPC channel.
    let method = mock.last_method.lock().unwrap().clone();
    assert_eq!(method.as_deref(), Some("approval.ask"));
}

/// Given session-approval memory covering the fetch host facet, when the same
/// private host is fetched again, then the chain approves directly — the
/// approver is never consulted — and the authorize result still carries the
/// exemption metadata (ADR-010 B1: without it the second fetch would hit the
/// provider's hard refusal).
#[tokio::test]
async fn should_carry_exemption_when_private_fetch_approved_via_session_memory() {
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let pm = PermissionManager::new(PermissionMode::Manual, vec![]);
    pm.record_approval("FetchURL(host:127.0.0.1)".to_string());
    let pm = Arc::new(pm);
    let callback: Arc<dyn mirri_core::reverse_rpc::ReverseRpcCallback> =
        Arc::new(CountingNeverCalledCallback {
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
    let hooks = make_hooks_with_permission_and_callback(memory, Some(pm), Some(callback));
    let cancel = CancelToken::new();
    let args = serde_json::json!({"url": "http://127.0.0.1:9/x"});
    let display = ToolInputDisplay::Generic {
        summary: "fetch local".into(),
    };
    let ctx = AuthorizeToolContext {
        turn_id: "1".to_string(),
        step: 1,
        tool_call_id: "call-fetch-2".to_string(),
        tool_name: "FetchURL".to_string(),
        tool_readonly: false,
        args: &args,
        approval_rule: "FetchURL(host:127.0.0.1)",
        display: &display,
        cancel: cancel.clone(),
    };

    let result = hooks.authorize_tool_execution(ctx).await.unwrap();
    let AuthorizeToolResult::Approved { metadata } = result else {
        panic!("expected Approved, got {result:?}")
    };
    let metadata = metadata.expect("a session-memory approval must carry exemption metadata");
    assert_eq!(metadata["private_network_exemption"]["host"], "127.0.0.1");
}

/// Given a headless session with no approver, when a private-network fetch
/// ask fires, then the call is Blocked — no user present means no
/// private-network fetch (SDR-007 d).
#[tokio::test]
async fn should_block_when_private_fetch_headless() {
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    // `set_headless` takes `&mut self` — run it before the `Arc` wrapping.
    let mut pm = PermissionManager::new(PermissionMode::Manual, vec![]);
    pm.set_headless(true);
    let pm = Arc::new(pm);
    let hooks = make_hooks_with_permission(memory, Some(pm));
    let cancel = CancelToken::new();
    let args = serde_json::json!({"url": "http://127.0.0.1:9/x"});
    let display = ToolInputDisplay::Generic {
        summary: "fetch local".into(),
    };
    let ctx = AuthorizeToolContext {
        turn_id: "1".to_string(),
        step: 1,
        tool_call_id: "call-fetch-3".to_string(),
        tool_name: "FetchURL".to_string(),
        tool_readonly: false,
        args: &args,
        approval_rule: "FetchURL(host:127.0.0.1)",
        display: &display,
        cancel: cancel.clone(),
    };

    let result = hooks.authorize_tool_execution(ctx).await.unwrap();
    let AuthorizeToolResult::Blocked { reason } = result else {
        panic!("expected Blocked, got {result:?}")
    };
    assert!(
        reason.contains("requires user approval"),
        "the block reason must explain the missing approval, got: {reason}"
    );
}

/// Given a public-network FetchURL (public IP literal, no DNS), when the
/// chain default-approves it, then the authorize result carries no exemption
/// metadata — the provider needs no bypass for public fetches.
#[tokio::test]
async fn should_carry_no_metadata_when_public_fetch_approved() {
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let pm = Arc::new(PermissionManager::new(PermissionMode::Manual, vec![]));
    let hooks = make_hooks_with_permission(memory, Some(pm));
    let cancel = CancelToken::new();
    let args = serde_json::json!({"url": "http://8.8.8.8:80/x"});
    let display = ToolInputDisplay::Generic {
        summary: "fetch public".into(),
    };
    let ctx = AuthorizeToolContext {
        turn_id: "1".to_string(),
        step: 1,
        tool_call_id: "call-fetch-4".to_string(),
        tool_name: "FetchURL".to_string(),
        tool_readonly: false,
        args: &args,
        approval_rule: "FetchURL(host:8.8.8.8)",
        display: &display,
        cancel: cancel.clone(),
    };

    let result = hooks.authorize_tool_execution(ctx).await.unwrap();
    assert!(
        matches!(result, AuthorizeToolResult::Approved { metadata: None }),
        "a public fetch must approve without exemption metadata, got {result:?}"
    );
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
        tool_readonly: false,
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
        matches!(result, AuthorizeToolResult::Approved { metadata: None }),
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
        tool_readonly: false,
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
        tool_readonly: false,
        args: &args,
        approval_rule: "Read",
        display: &display,
        cancel: cancel.clone(),
    };

    let result = hooks.authorize_tool_execution(ctx).await.unwrap();
    assert!(
        matches!(result, AuthorizeToolResult::Approved { metadata: None }),
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
        tool_readonly: false,
        args: &args,
        approval_rule: "Read",
        display: &display,
        cancel: cancel.clone(),
    };

    let result = hooks.authorize_tool_execution(ctx).await.unwrap();
    assert!(matches!(
        result,
        AuthorizeToolResult::Approved { metadata: None }
    ));

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
            tool_readonly: false,
            args: &args,
            approval_rule: "Read",
            display: &display,
            cancel: cancel.clone(),
        };

        let result = hooks.authorize_tool_execution(ctx).await.unwrap();
        if expected {
            assert!(
                matches!(result, AuthorizeToolResult::Approved { metadata: None }),
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
            tool_readonly: false,
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
            AuthorizeToolResult::Approved { metadata: None }
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

/// A custom `LoopHooks` implementation that approves every call with a fixed
/// metadata payload, mirroring how the permission gate attaches the
/// ADR-010 private-network exemption on approval.
struct MetadataApprovingHooks {
    metadata: Value,
}

impl LoopHooks for MetadataApprovingHooks {
    fn authorize_tool_execution(
        &self,
        _ctx: AuthorizeToolContext<'_>,
    ) -> HookFuture<EngineResult<AuthorizeToolResult>> {
        let metadata = self.metadata.clone();
        Box::pin(async move {
            Ok(AuthorizeToolResult::Approved {
                metadata: Some(metadata),
            })
        })
    }

    fn prepare_tool_execution(
        &self,
        _ctx: PrepareToolContext<'_>,
    ) -> HookFuture<EngineResult<PrepareToolResult>> {
        Box::pin(async { Ok(PrepareToolResult::Allowed) })
    }
}

/// An `Echo` variant that records the `approval_metadata` it was executed
/// with, so a test can assert what the batch executor handed to the tool.
struct MetadataCapturingEchoTool {
    seen: Arc<Mutex<Vec<Option<Value>>>>,
}

impl Tool for MetadataCapturingEchoTool {
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
        let seen = Arc::clone(&self.seen);
        Ok(ToolExecution::Runnable {
            plan: ExecutionPlan {
                accesses: Vec::new(),
                display: ToolInputDisplay::Generic {
                    summary: format!("Echo: {}", msg),
                },
                approval_rule: "Echo".to_string(),
                stop_batch_after_this: false,
            },
            execute: Box::new(move |ctx| {
                Box::pin(async move {
                    if let Ok(mut seen) = seen.lock() {
                        seen.push(ctx.approval_metadata.clone());
                    }
                    ToolResult::success(msg)
                })
            }),
        })
    }
}

/// Given a permission gate that approves a call with metadata, when the call
/// travels through `prepare_tool_call` and `execute_tool_batch`, then the
/// tool's execution context carries that exact metadata — the approval-gate
/// channel (ADR-010 exemption pass-through) stays wired end to end.
#[tokio::test]
async fn should_carry_approval_metadata_into_execution_context_when_gate_approves_with_it() {
    let metadata = serde_json::json!({
        "private_network_exemption": {
            "host": "10.0.0.5",
            "approved_ips": ["10.0.0.5"]
        }
    });
    let hooks = MetadataApprovingHooks {
        metadata: metadata.clone(),
    };

    let capturing = Arc::new(MetadataCapturingEchoTool {
        seen: Arc::new(Mutex::new(Vec::new())),
    });
    let mut tools = ToolManager::new();
    tools.set_active_tools(vec!["Echo".to_string()]);
    tools.register_builtin(capturing.clone());
    let mut validators = ValidatorCache::new();
    let cancel = CancelToken::new();

    // Preflight the tool call to get a ValidatedToolCall.
    let validated = match mirri_core::tool_registry::preflight_tool_call(
        "Echo",
        r#"{"message":"hi"}"#,
        "call-meta-1",
        &tools,
        &mut validators,
    ) {
        mirri_core::tool_registry::PreflightResult::Valid(v) => v,
        mirri_core::tool_registry::PreflightResult::Rejected(r) => {
            panic!("expected Valid, got Rejected: {}", r.error)
        }
    };

    let prepared = prepare_tool_call(validated, "1", 1, &cancel, &hooks)
        .await
        .expect("prepare_tool_call should succeed");
    let ready = match prepared {
        PrepareResult::Ready(r) => r,
        PrepareResult::Blocked(_) => panic!("expected PrepareResult::Ready, got Blocked"),
        PrepareResult::Synthetic(_) => panic!("expected PrepareResult::Ready, got Synthetic"),
    };
    assert!(
        ready.approval_metadata.is_some(),
        "prepare_tool_call must keep the gate's metadata"
    );

    let results = execute_tool_batch(
        vec![ready],
        vec![],
        vec![],
        "1",
        1,
        &cancel,
        None,
        &hooks,
        None,
    )
    .await;
    assert_eq!(results.len(), 1);
    assert!(!results[0].1.is_error, "the echo call must succeed");

    let seen = capturing.seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![Some(metadata)],
        "the tool context must carry the exact approval metadata"
    );
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

/// Like `make_hooks_with_goal_and_steer` but also installs a collecting
/// wire recorder, so flush-durability assertions can inspect the recorded
/// ops of the stop-boundary path (`should_continue_after_stop`).
fn make_hooks_with_goal_and_steer_and_recorder(
    memory: Arc<Mutex<ContextMemory>>,
    gm: Option<Arc<GoalManager>>,
    recorded: Arc<Mutex<Vec<WireOp>>>,
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
        TurnFlowHooksConfig {
            wire_recorder: Some(Arc::new(CollectingWireRecorder(recorded))
                as Arc<dyn WireOpRecorder>),
            ..TurnFlowHooksConfig::default()
        },
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

/// Like `make_hooks_with_goal_and_steer_and_recorder` but also wires the
/// session-scoped todo list, so checkpoint-equivalence assertions can
/// inspect the live undo stack the flush maintains.
fn make_hooks_with_goal_and_steer_and_recorder_and_todo(
    memory: Arc<Mutex<ContextMemory>>,
    gm: Option<Arc<GoalManager>>,
    recorded: Arc<Mutex<Vec<WireOp>>>,
    todo_list: Arc<TodoList>,
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
        TurnFlowHooksConfig {
            wire_recorder: Some(Arc::new(CollectingWireRecorder(recorded))
                as Arc<dyn WireOpRecorder>),
            todo_list: Some(todo_list),
            ..TurnFlowHooksConfig::default()
        },
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
    // goal budget. The busy-path admission tags it with the user_steer
    // origin (CR13).
    steer_buffer.lock().unwrap().push(
        vec![ContentPart::Text {
            text: "new direction".to_string(),
        }],
        PromptOrigin::UserSteer,
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
        PromptOrigin::UserSteer,
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
            PromptOrigin::UserSteer,
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

/// Given a turn flow with a wire recorder and a goal whose usage has been
/// accounted for one turn, When the goal is completed and another turn runs,
/// Then the new turn records no goal.update op, the usage counters stay
/// frozen at the pre-completion totals, and the completed goal stays
/// completed (never flipped to blocked by budget enforcement).
#[tokio::test]
async fn should_not_record_goal_update_or_advance_usage_after_the_goal_is_completed() {
    // Given a turn flow with a wire recorder and an active goal carrying a
    // 2-turn budget (the second step would exhaust it), with two scripted
    // steps reporting 100 input + 50 output tokens each
    let llm: Arc<dyn Llm> = Arc::new(MockLlm::new(vec![
        MockResponse::end_turn("First!").with_usage(100, 50),
        MockResponse::end_turn("Second!").with_usage(100, 50),
    ]));
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let cancel = CancelToken::new();
    let gm = Arc::new(GoalManager::new());
    gm.create("Track usage".to_string(), None).unwrap();
    gm.set_budget(GoalBudget {
        turns: Some(2),
        tokens: Some(100_000),
        wall_clock_ms: None,
    });

    let recorded = Arc::new(Mutex::new(Vec::new()));
    let mut turn_flow = TurnFlow::new(TurnFlowConfig::default());
    turn_flow.set_wire_recorder(Some(
        Arc::new(CollectingWireRecorder(Arc::clone(&recorded))) as Arc<dyn WireOpRecorder>,
    ));
    // The accounting-only slot (the embedded main-flow wiring): no goal
    // continuation, so a scripted end_turn ends the turn after one step.
    turn_flow.set_goal_usage_accounting(Arc::clone(&gm));

    // When a turn runs under the active goal
    turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "Hello".to_string(),
            }],
            PromptOrigin::User,
            Arc::new(Mutex::new(ContextMemory::new())),
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::clone(&llm))),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("turn should succeed");

    // Then the first turn's usage was accounted
    assert_eq!(gm.used_turns(), 1);
    assert_eq!(gm.used_tokens(), 150);

    // When the goal is completed and another turn runs
    gm.update(GoalStatus::Complete);
    recorded
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    turn_flow
        .run_one_turn(
            vec![ContentPart::Text {
                text: "More".to_string(),
            }],
            PromptOrigin::User,
            Arc::new(Mutex::new(ContextMemory::new())),
            &tools,
            Arc::new(FixedLlmResolver::new(Arc::clone(&llm))),
            &cancel,
            &mut validators,
            None,
            None,
        )
        .await
        .expect("turn should succeed");

    // Then no goal.update op is recorded for the post-completion turn
    let ops = recorded.lock().unwrap_or_else(|e| e.into_inner());
    assert!(
        !ops.iter().any(|op| matches!(op, WireOp::GoalUpdate { .. })),
        "no goal.update expected after the goal is completed: {ops:?}"
    );
    // The usage counters stay frozen at the pre-completion totals
    assert_eq!(gm.used_turns(), 1);
    assert_eq!(gm.used_tokens(), 150);
    // And the completed goal stays completed — the second step would have
    // exhausted the 2-turn budget and flipped it to blocked if accounting
    // still ran for terminal goals.
    assert_eq!(gm.get().unwrap().status, GoalStatus::Complete);
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

// Env-mutating tests serialize on the workspace-wide env lock
// (mirri-test-support).

/// Saves and restores the hook-command-rewrite env var; every env-mutating
/// test holds the workspace env lock for its whole body.
struct HookRewriteEnvGuard(Option<String>);

impl HookRewriteEnvGuard {
    fn set(value: Option<&str>) -> Self {
        let saved = std::env::var(HOOK_COMMAND_REWRITE_ENV).ok();
        // SAFETY: guarded by the workspace env lock; restored on drop.
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
    let _env_lock = mirri_test_support::env::lock();
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
    let _env_lock = mirri_test_support::env::lock();
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
    let _env_lock = mirri_test_support::env::lock();
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
            .filter(|op| {
                matches!(op, WireOp::ContextAppendMessage { message }
                if message["content"][0]["text"]
                    .as_str()
                    .is_some_and(|t| t.contains("DISREGARD any earlier skill listings")))
            })
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

}

mod restore_interruption_ {
//! Integration tests for interruption-reminder reconciliation after a wire
//! restore (Task: context-injection-rust-wiring, Task 5).
//!
//! These tests verify the resume path's `reconcilePendingReminders`
//! semantics (TS parity): when the replayed `AgentState.interruption_pending`
//! is non-empty — the crash point was between the durable `turn.cancel`
//! write and the reminder delivery — the resume must deliver the reminder
//! into the reconstructed memory and record the delivery ops on the wire
//! log it just re-opened for appending, so the next replay starts from a
//! drained set:
//!
//! - a journal that stopped before the delivery gains the reminder message
//!   plus the `context.append_message` / `interruptionReminder.recorded`
//!   ops after the restore;
//! - a journal that already recorded the full delivery sequence is not
//!   duplicated;
//! - a journal that crashed after the durable reminder append (but before
//!   the `interruptionReminder.recorded` marker) skips the duplicate append
//!   while still recording the pending marker.
//!
//! Tests use the Given-When-Then naming convention.

use std::collections::HashSet;

use mirri_core::{
    AgentId, AgentScopeConfig, AppScope, AppScopeConfig, PromptOrigin, SessionId, SessionManager,
    SessionMetadata, WorkspaceId, WorkspaceScopeConfig, WorkspaceTrustLevel,
};
use mirri_persistence::{AppendLogStore, JsonlAppendLog};
use mirri_proto::WireOp;
use serde_json::json;

// ===========================================================================
// Helpers
// ===========================================================================

/// A temp-dir-backed session manager (same scaffolding as the resume tests).
struct TestEnv {
    manager: SessionManager,
    _tmp: tempfile::TempDir,
}

impl TestEnv {
    async fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let app = AppScope::new(AppScopeConfig::default());
        let workspace = app
            .create_workspace_with_config(
                WorkspaceId::new("test-ws"),
                tmp.path().to_path_buf(),
                WorkspaceScopeConfig {
                    trust_level: WorkspaceTrustLevel::Trusted,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let manager = SessionManager::with_persistence_dir(
            workspace,
            tmp.path().join(".mirri").join("sessions"),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        Self { manager, _tmp: tmp }
    }
}

fn make_metadata(title: &str) -> SessionMetadata {
    SessionMetadata {
        title: title.to_string(),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        tags: HashSet::new(),
        archived: false,
        is_custom_title: false,
        work_dir: None,
        last_prompt: None,
        forked_from: None,
        cache_root_id: None,
        custom: None,
        last_used_at: None,
    }
}

/// A resume base config carrying an explicit model (the hand-written wire
/// logs here carry no model record, so the base config seeds the model the
/// way the embedded host seeds it from the config snapshot's
/// `default_model`).
fn resume_base_config() -> AgentScopeConfig {
    AgentScopeConfig {
        model_id: "test-resume-model".to_string(),
        ..AgentScopeConfig::default()
    }
}

/// The `<system-reminder>`-wrapped interruption reminder text exactly as
/// `ContextMemory::append_system_reminder` stores it.
fn reminder_wire_text() -> String {
    mirri_core::system_reminder_wire::system_reminder_text(
        mirri_core::interruption_reminder::INTERRUPTION_REMINDER_TEXT,
    )
}

/// A user cancellation of the active turn (the only `turn.cancel` shape
/// that parks an interruption reminder).
fn user_cancel_op(turn_id: u64) -> WireOp {
    WireOp::TurnCancel {
        turn_id: Some(turn_id),
        target: Some("active".to_string()),
        reason: Some("user_cancelled".to_string()),
    }
}

/// The durable reminder-append op exactly as the live write side records
/// it (Task 4's `deliver_interruption_reminder`).
fn reminder_append_op() -> WireOp {
    mirri_core::system_reminder_wire::system_reminder_op(
        mirri_core::interruption_reminder::INTERRUPTION_REMINDER_TEXT,
    )
}

/// Write a sequence of wire ops to the session's `wire.jsonl`
/// (truncating any existing file).
async fn write_wire_log(manager: &SessionManager, session_id: &SessionId, ops: &[WireOp]) {
    let wire_log_path = manager.persistence().wire_log_path(session_id);
    let mut log = JsonlAppendLog::create(&wire_log_path).await.unwrap();
    for op in ops {
        log.append(op).await.unwrap();
    }
}

/// Read the session's current `wire.jsonl` back as parsed wire ops.
async fn read_wire_log_ops(manager: &SessionManager, session_id: &SessionId) -> Vec<WireOp> {
    let wire_log_path = manager.persistence().wire_log_path(session_id);
    let contents = std::fs::read_to_string(&wire_log_path)
        .expect("the session's wire log must be readable after the resume");
    contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<WireOp>(line).expect("wire log line must parse"))
        .collect()
}

/// How many interruption reminders (Injection origin, reminder text) the
/// agent's reconstructed memory holds.
fn reminder_count(agent: &mirri_core::AgentScope) -> usize {
    let memory = agent.context_memory();
    let guard = memory.lock().unwrap();
    guard
        .history()
        .iter()
        .filter(|m| {
            m.origin == Some(PromptOrigin::Injection)
                && m.message.text_content() == reminder_wire_text()
        })
        .count()
}

/// The resumed session's recreated main agent.
async fn main_agent(session: &mirri_core::SessionScope) -> std::sync::Arc<mirri_core::AgentScope> {
    session
        .get_agent(&AgentId::new("main"))
        .await
        .expect("resumed session must recreate the main agent")
}

// ===========================================================================
// Restore reconciles a pending interruption reminder
// ===========================================================================

#[tokio::test]
async fn should_replay_pending_interruption_reminder_after_restore() {
    // Given a persisted session whose journal records one user message and
    // a user cancellation of turn 9 — but NOT the reminder delivery (the
    // crash point was between the durable `turn.cancel` write and the
    // reminder injection)
    let env = TestEnv::new().await;
    let session_id = SessionId::new("restore-pending-reminder");
    env.manager
        .persistence()
        .save_state(&session_id, &make_metadata("Pending Reminder"))
        .await
        .unwrap();
    write_wire_log(
        &env.manager,
        &session_id,
        &[
            WireOp::Metadata {
                protocol_version: Some("1.5".to_string()),
                created_at: Some(1700000000000),
            },
            WireOp::ContextAppendMessage {
                message: json!({
                    "role": "user",
                    "content": [{"type": "text", "text": "hello"}],
                    "toolCalls": [],
                }),
            },
            user_cancel_op(9),
        ],
    )
    .await;
    let ops_before = read_wire_log_ops(&env.manager, &session_id).await;

    // When resuming the session
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then the main agent's memory ends with the interruption reminder
    // (Injection origin, the <system-reminder>-wrapped text)
    let agent = main_agent(&resumed).await;
    {
        let memory = agent.context_memory();
        let guard = memory.lock().unwrap();
        let last = guard
            .history()
            .last()
            .expect("the reconciled memory must not be empty");
        assert_eq!(last.origin, Some(PromptOrigin::Injection));
        assert_eq!(last.message.text_content(), reminder_wire_text());
    }
    assert_eq!(reminder_count(&agent), 1);

    // And the journal gained exactly the durable delivery ops after the
    // restore: `context.append_message` (the reminder) then
    // `interruptionReminder.recorded { turnId: 9 }`
    let ops_after = read_wire_log_ops(&env.manager, &session_id).await;
    assert_eq!(
        ops_after.len(),
        ops_before.len() + 2,
        "the restore must append exactly the reminder append + the recorded marker"
    );
    let tail: Vec<&WireOp> = ops_after[ops_before.len()..].iter().collect();
    match tail[0] {
        WireOp::ContextAppendMessage { message } => {
            let WireOp::ContextAppendMessage { message: expected } = reminder_append_op() else {
                unreachable!()
            };
            assert_eq!(message, &expected);
        }
        other => panic!("expected a context.append_message op, got {other:?}"),
    }
    match tail[1] {
        WireOp::InterruptionReminderRecorded { turn_id } => assert_eq!(*turn_id, 9),
        other => panic!("expected an interruptionReminder.recorded op, got {other:?}"),
    }
}

#[tokio::test]
async fn should_not_duplicate_reminder_when_replaying_journal_that_already_delivered() {
    // Given a persisted session whose journal records the full delivery
    // sequence: the user cancellation of turn 9, the reminder's
    // `context.append_message`, and the `interruptionReminder.recorded`
    // marker — the replayed pending set is empty
    let env = TestEnv::new().await;
    let session_id = SessionId::new("restore-delivered-reminder");
    env.manager
        .persistence()
        .save_state(&session_id, &make_metadata("Delivered Reminder"))
        .await
        .unwrap();
    write_wire_log(
        &env.manager,
        &session_id,
        &[
            WireOp::Metadata {
                protocol_version: Some("1.5".to_string()),
                created_at: Some(1700000000000),
            },
            WireOp::ContextAppendMessage {
                message: json!({
                    "role": "user",
                    "content": [{"type": "text", "text": "hello"}],
                    "toolCalls": [],
                }),
            },
            user_cancel_op(9),
            reminder_append_op(),
            WireOp::InterruptionReminderRecorded { turn_id: 9 },
        ],
    )
    .await;
    let ops_before = read_wire_log_ops(&env.manager, &session_id).await;

    // When resuming the session
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then the memory holds exactly one reminder (the replayed one — no
    // second copy appended) and the journal is unchanged
    let agent = main_agent(&resumed).await;
    assert_eq!(reminder_count(&agent), 1);
    let ops_after = read_wire_log_ops(&env.manager, &session_id).await;
    assert_eq!(
        ops_after.len(),
        ops_before.len(),
        "a fully delivered journal must not gain any ops on restore"
    );
}

#[tokio::test]
async fn should_skip_reminder_append_but_record_marker_when_journal_crashed_after_durable_append() {
    // Given a persisted session whose journal crashed after the durable
    // reminder append but before the `interruptionReminder.recorded`
    // marker: the replayed pending set holds turn 9 while the
    // reconstructed memory already ends with the reminder
    let env = TestEnv::new().await;
    let session_id = SessionId::new("restore-half-delivered-reminder");
    env.manager
        .persistence()
        .save_state(&session_id, &make_metadata("Half Delivered"))
        .await
        .unwrap();
    write_wire_log(
        &env.manager,
        &session_id,
        &[
            WireOp::Metadata {
                protocol_version: Some("1.5".to_string()),
                created_at: Some(1700000000000),
            },
            user_cancel_op(9),
            reminder_append_op(),
        ],
    )
    .await;
    let ops_before = read_wire_log_ops(&env.manager, &session_id).await;

    // When resuming the session
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await
        .unwrap();

    // Then the reminder is not appended a second time — the memory holds
    // exactly one reminder (the replayed one)
    let agent = main_agent(&resumed).await;
    assert_eq!(reminder_count(&agent), 1);

    // And the journal gains exactly one op: the pending turn's
    // `interruptionReminder.recorded` marker, so the next replay starts
    // from a drained set
    let ops_after = read_wire_log_ops(&env.manager, &session_id).await;
    assert_eq!(
        ops_after.len(),
        ops_before.len() + 1,
        "only the missing recorded marker may be appended"
    );
    match ops_after.last() {
        Some(WireOp::InterruptionReminderRecorded { turn_id }) => assert_eq!(*turn_id, 9),
        other => panic!("expected an interruptionReminder.recorded op, got {other:?}"),
    }
}

}

mod recovery_closure_ {
//! Integration tests for the synthetic interrupted turn.ended closure
//! (plan: interrupted-task-recovery, Task 3).
//!
//! Task 3 writes the recovery backfill: a wire log whose tail ends with a
//! dangling `turn.prompt` (the process died mid-turn) gains a synthetic
//! `turn.ended { reason: "interrupted", error: { cause } }` whose turnId is
//! the dangling prompt's ordinal (ADR-009). The resume test then proves the
//! closure replays through a real `SessionManager` resume without error —
//! a closure landing between prompts is a normal append order for the
//! replay's out-of-order tolerance.
//!
//! Tests use the Given-When-Then naming convention.

use std::collections::HashSet;

use mirri_core::{
    AgentId, AgentScopeConfig, AppScope, AppScopeConfig, SessionId, SessionManager,
    SessionMetadata, WorkspaceId, WorkspaceScopeConfig, WorkspaceTrustLevel,
    session::recovery_scan::append_interrupted_closure,
};
use mirri_persistence::{AppendLogStore, JsonlAppendLog};
use mirri_proto::WireOp;
use serde_json::json;

// ===========================================================================
// Helpers (same scaffolding as restore_interruption_tests.rs)
// ===========================================================================

/// A temp-dir-backed session manager (same scaffolding as the resume tests).
struct TestEnv {
    manager: SessionManager,
    _tmp: tempfile::TempDir,
}

impl TestEnv {
    async fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let app = AppScope::new(AppScopeConfig::default());
        let workspace = app
            .create_workspace_with_config(
                WorkspaceId::new("test-ws"),
                tmp.path().to_path_buf(),
                WorkspaceScopeConfig {
                    trust_level: WorkspaceTrustLevel::Trusted,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let manager = SessionManager::with_persistence_dir(
            workspace,
            tmp.path().join(".mirri").join("sessions"),
            std::sync::Arc::new(mirri_os::LocalFs::new()),
        );
        Self { manager, _tmp: tmp }
    }
}

fn make_metadata(title: &str) -> SessionMetadata {
    SessionMetadata {
        title: title.to_string(),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        tags: HashSet::new(),
        archived: false,
        is_custom_title: false,
        work_dir: None,
        last_prompt: None,
        forked_from: None,
        cache_root_id: None,
        custom: None,
        last_used_at: None,
    }
}

/// A resume base config carrying an explicit model (the hand-written wire
/// logs here carry no model record, so the base config seeds the model).
fn resume_base_config() -> AgentScopeConfig {
    AgentScopeConfig {
        model_id: "test-resume-model".to_string(),
        ..AgentScopeConfig::default()
    }
}

/// Write a sequence of wire ops to the session's `wire.jsonl`
/// (truncating any existing file). The `time` stamps on these ops are
/// injected by the append layer itself (TS parity).
async fn write_wire_log(manager: &SessionManager, session_id: &SessionId, ops: &[WireOp]) {
    let wire_log_path = manager.persistence().wire_log_path(session_id);
    let mut log = JsonlAppendLog::create(&wire_log_path).await.unwrap();
    for op in ops {
        log.append(op).await.unwrap();
    }
}

fn turn_prompt_op() -> WireOp {
    WireOp::TurnPrompt {
        input: json!({ "content": [] }),
        origin: json!({ "kind": "user" }),
    }
}

fn turn_ended_op(turn_id: u64) -> WireOp {
    WireOp::TurnEnded {
        turn_id,
        reason: "completed".to_string(),
        error: None,
        duration_ms: None,
        ended_at: None,
    }
}

// ===========================================================================
// The synthetic closure record
// ===========================================================================

#[tokio::test]
async fn should_append_interrupted_turn_ended_with_prompt_ordinal() {
    // Given a dangling-prompt wire log (one closed turn plus one prompt the
    // process never closed)
    let tmp = tempfile::tempdir().unwrap();
    let wire_log_path = tmp.path().join("wire.jsonl");
    let mut log = JsonlAppendLog::create(&wire_log_path).await.unwrap();
    log.append(&turn_prompt_op()).await.unwrap();
    log.append(&turn_ended_op(1)).await.unwrap();
    log.append(&turn_prompt_op()).await.unwrap();
    log.close().await;

    // When the synthetic interrupted closure is appended for ordinal 3
    append_interrupted_closure(&wire_log_path, 3, "service_exit")
        .await
        .unwrap();

    // Then the LAST line is a turn.ended closing prompt 3 as interrupted —
    // parsed-field assertions, not a char-by-char compare (the append layer
    // injects the `time` stamp itself).
    let contents = std::fs::read_to_string(&wire_log_path).unwrap();
    let last_line = contents
        .lines()
        .rfind(|line| !line.trim().is_empty())
        .expect("the wire log must not be empty after the closure");
    let record: serde_json::Value = serde_json::from_str(last_line).unwrap();
    assert_eq!(record["type"], "turn.ended");
    assert_eq!(record["turnId"], 3);
    assert_eq!(record["reason"], "interrupted");
    assert_eq!(record["error"]["cause"], "service_exit");
    assert!(record.get("time").and_then(|t| t.as_u64()).is_some());

    // And the pre-existing records are intact (the closure appends, never
    // truncates — open_for_append semantics)
    let line_count = contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();
    assert_eq!(line_count, 4);
}

// ===========================================================================
// Replay tolerance: the closure survives a real resume
// ===========================================================================

#[tokio::test]
async fn should_replay_interrupted_closure_without_error() {
    // Given a persisted session whose wire log holds one closed turn and a
    // dangling prompt (time 4000 — the crash shape), then gains the
    // synthetic interrupted closure via the recovery path
    let env = TestEnv::new().await;
    let session_id = SessionId::new("replay-interrupted-closure");
    env.manager
        .persistence()
        .save_state(&session_id, &make_metadata("Interrupted Closure"))
        .await
        .unwrap();
    write_wire_log(
        &env.manager,
        &session_id,
        &[
            WireOp::Metadata {
                protocol_version: Some("1.5".to_string()),
                created_at: Some(1700000000000),
            },
            turn_prompt_op(),
            turn_ended_op(1),
            turn_prompt_op(),
        ],
    )
    .await;
    let wire_log_path = env.manager.persistence().wire_log_path(&session_id);
    append_interrupted_closure(&wire_log_path, 2, "service_exit")
        .await
        .unwrap();

    // When the session is resumed, the closure landing between prompts is
    // a normal append order for the replay — the resume must succeed
    let resumed = env
        .manager
        .resume_session_with_limits(&session_id, resume_base_config(), None)
        .await;

    // Then the resume succeeds (no error, the main agent is recreated)
    let resumed =
        resumed.expect("resuming a journal with a synthetic interrupted closure must succeed");
    resumed
        .get_agent(&AgentId::new("main"))
        .await
        .expect("the resumed session must recreate the main agent");
}

}

