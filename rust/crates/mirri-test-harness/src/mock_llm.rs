//! Scripted LLM provider for tests: a [`MockLlm`] that plays back a queue of
//! [`MockResponse`]s instead of calling a real provider, plus a
//! [`MockLlmFactory`] that hands the shared mock to every agent of a
//! [`mirri_server::ServerState`] or a [`mirri_embedded::EmbeddedServer`], so
//! the engine loop runs without API keys.
//!
//! Responses are consumed in FIFO order, one per `chat()` call. The stop
//! reason strings follow the provider-stream convention understood by the
//! loop engine ([`mirri_core::loop_engine`]): `"completed"` ends the turn
//! (after any tool calls have been executed), `"tool_calls"` keeps the loop
//! running for the carried tool calls.
//!
//! Beyond the plain queue, the mock supports the behaviors the server e2e
//! suite needs:
//!
//! - **Token streaming** — [`MockResponse::streaming`] emits the response
//!   text as one [`LlmEvent::TextDelta`] per character, `token_delay` apart.
//!   The shared mock counts every yielded delta and records the first/last
//!   yield times (see [`MockLlm::delta_yield_count`] / [`MockLlm::delta_spread`]),
//!   so tests can assert that the engine consumed an incrementally streamed
//!   response — not a single bulk part.
//! - **Never responding** — [`MockLlm::hang_forever`] makes every `chat()`
//!   call return a stream that never yields and never completes, driving the
//!   server's engine-turn timeout.
//! - **Cancellation observation** — streamed responses observe the
//!   [`CancelToken`] carried in [`LlmChatParams`] between deltas, like a real
//!   provider adapter watching its HTTP connection: when the token fires the
//!   stream stops mid-way and [`MockLlm::cancel_seen`] flips.
//! - **Configurable usage** — each scripted response can report non-zero
//!   token buckets via [`MockResponse::with_usage`], so tests can drive the
//!   engine's per-turn usage read face end to end (all zeros by default).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::stream;
use mirri_core::AgentScopeConfig;
use mirri_core::LlmFactory;
use mirri_core::TokenUsage;
use mirri_core::cancel::CancelToken;
use mirri_core::error::{EngineError, EngineResult};
use mirri_core::llm::{Llm, LlmChatParams, LlmEvent, LlmEventStream, LlmToolCall, ModelCapability};

/// Stop-reason string that ends a turn once carried tool calls (if any) have
/// been executed — the loop engine maps it to `EndTurn`.
pub const STOP_REASON_COMPLETED: &str = "completed";

/// Stop-reason string that keeps the loop running so the carried tool calls
/// are executed — the loop engine maps it to `ToolUse`.
pub const STOP_REASON_TOOL_CALLS: &str = "tool_calls";

/// One scripted provider answer, replayed by [`MockLlm::chat`].
#[derive(Debug, Clone)]
pub struct MockResponse {
    pub text: String,
    /// Optional thinking/reasoning text streamed ahead of `text` as one
    /// [`LlmEvent::ThinkDelta`] per character (no block-level `ThinkPart`,
    /// matching how a provider streams reasoning incrementally) — used to
    /// exercise the thinking-delta plumbing end to end.
    pub thinking: Option<String>,
    pub tool_calls: Vec<LlmToolCall>,
    /// Provider stop-reason string; see [`STOP_REASON_COMPLETED`] and
    /// [`STOP_REASON_TOOL_CALLS`].
    pub stop_reason: String,
    /// When `Some(delay)`, `text` is streamed as one [`LlmEvent::TextDelta`]
    /// per character, `delay` apart; `None` (the default) emits the text as
    /// a single delta without pacing.
    pub token_delay: Option<Duration>,
    /// Token usage reported by the stream's trailing `Usage` event.
    /// Defaults to all zeros (the mock's historic behavior); set it via
    /// [`Self::with_usage`] when a test must drive a usage-bearing turn.
    pub usage: TokenUsage,
}

impl MockResponse {
    /// A plain-text response that ends the turn.
    pub fn end_turn(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            thinking: None,
            tool_calls: Vec::new(),
            stop_reason: STOP_REASON_COMPLETED.to_string(),
            token_delay: None,
            usage: TokenUsage::default(),
        }
    }

    /// A response that requests tool calls, keeping the loop running.
    pub fn tool_use(tool_calls: Vec<LlmToolCall>) -> Self {
        Self {
            text: String::new(),
            thinking: None,
            tool_calls,
            stop_reason: STOP_REASON_TOOL_CALLS.to_string(),
            token_delay: None,
            usage: TokenUsage::default(),
        }
    }

    /// A response whose text is streamed as one `TextDelta` per character,
    /// `token_delay` between consecutive characters, before the final
    /// block-level `TextPart` and `Done`. Use a non-zero delay to make the
    /// incremental delivery observable (see [`MockLlm::delta_spread`]).
    pub fn streaming(text: impl Into<String>, token_delay: Duration) -> Self {
        Self {
            text: text.into(),
            thinking: None,
            tool_calls: Vec::new(),
            stop_reason: STOP_REASON_COMPLETED.to_string(),
            token_delay: Some(token_delay),
            usage: TokenUsage::default(),
        }
    }

    /// A response that streams the given thinking text (one `ThinkDelta` per
    /// character) ahead of a plain-text answer that ends the turn.
    pub fn thinking_then_end_turn(thinking: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            thinking: Some(thinking.into()),
            ..Self::end_turn(text)
        }
    }

    /// Report the given four-bucket token usage in this response's trailing
    /// `Usage` event — the lever for driving the engine's per-turn usage
    /// read face (the `step.end` buckets) from a scripted turn.
    pub fn with_usage(mut self, usage: TokenUsage) -> Self {
        self.usage = usage;
        self
    }
}

/// Yield statistics shared between a [`MockLlm`] and the streams it hands
/// out, so tests can assert *how* the engine consumed the scripted deltas
/// (count, pacing, cancellation) without peeking into server internals.
///
/// The recorded yields happen inside the server process as the engine polls
/// the stream; the test observes them through the same `Arc<MockLlm>` that
/// was handed to the [`MockLlmFactory`].
#[derive(Debug, Default)]
struct MockStreamStats {
    /// How many `TextDelta` events have been polled out of this mock.
    delta_yield_count: AtomicU64,
    /// When the first delta was yielded (`None` until then).
    first_delta_at: Mutex<Option<Instant>>,
    /// When the most recent delta was yielded (`None` until then).
    last_delta_at: Mutex<Option<Instant>>,
    /// Whether any stream observed its cancel token fire mid-stream.
    cancel_seen: AtomicBool,
}

impl MockStreamStats {
    fn record_delta_yield(&self) {
        self.delta_yield_count.fetch_add(1, Ordering::Relaxed);
        let now = Instant::now();
        let mut first = self
            .first_delta_at
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if first.is_none() {
            *first = Some(now);
        }
        drop(first);
        *self
            .last_delta_at
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(now);
    }
}

/// A scripted [`Llm`] that replays [`MockResponse`]s in FIFO order.
///
/// The same instance can be shared through [`Arc`] between the test and the
/// server's [`MockLlmFactory`], so tests can queue responses before a
/// `message.send` and assert [`Self::chat_call_count`] afterwards.
#[derive(Debug)]
pub struct MockLlm {
    /// Queue of scripted responses, popped one per `chat()` call.
    responses: Arc<Mutex<VecDeque<MockResponse>>>,
    /// How many times `chat()` has been invoked.
    chat_calls: AtomicU64,
    /// When `true`, every `chat()` returns a stream that never yields —
    /// simulating an LLM that never responds.
    hangs: AtomicBool,
    /// Yield/timing/cancellation statistics shared with every stream this
    /// mock hands out.
    stats: Arc<MockStreamStats>,
}

impl MockLlm {
    /// Create an empty mock; queue responses with
    /// [`Self::push_response`]/[`Self::push_text_response`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a scripted response to the replay queue.
    pub fn push_response(&self, response: MockResponse) {
        self.responses
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push_back(response);
    }

    /// Append a plain-text response that ends the turn.
    pub fn push_text_response(&self, text: &str) {
        self.push_response(MockResponse::end_turn(text));
    }

    /// Append a response requesting the given tool calls, keeping the loop
    /// running so the engine executes them (then continues with the next
    /// queued response).
    pub fn push_tool_use_response(&self, tool_calls: Vec<LlmToolCall>) {
        self.push_response(MockResponse::tool_use(tool_calls));
    }

    /// Switch this mock into never-respond mode: every current and future
    /// `chat()` call returns a stream that never yields and never completes,
    /// so only an external deadline (the server's engine-turn timeout) can
    /// end the turn. Queued responses are ignored while the mode is active.
    ///
    /// This deliberately takes `&self` (not `self`) so tests can flip a mock
    /// that is already shared with a running server.
    pub fn hang_forever(&self) {
        self.hangs.store(true, Ordering::Relaxed);
    }

    /// Whether [`Self::hang_forever`] has been called.
    pub fn hangs(&self) -> bool {
        self.hangs.load(Ordering::Relaxed)
    }

    /// How many times `chat()` has been invoked since creation.
    pub fn chat_call_count(&self) -> u64 {
        self.chat_calls.load(Ordering::Relaxed)
    }

    /// Whether any scripted responses are still queued.
    pub fn has_pending_responses(&self) -> bool {
        !self
            .responses
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty()
    }

    /// How many `TextDelta` events the engine has polled out of this mock's
    /// streams. For a [`MockResponse::streaming`] response this equals the
    /// number of characters once the stream has been fully consumed.
    pub fn delta_yield_count(&self) -> u64 {
        self.stats.delta_yield_count.load(Ordering::Relaxed)
    }

    /// Time elapsed between the first and the most recent yielded delta
    /// (`None` if no delta has been yielded yet). For a streaming response
    /// with `delay`, this is at least `(chars - 1) × delay` once the stream
    /// has been fully consumed — the evidence that tokens were delivered
    /// incrementally rather than in one bulk part.
    pub fn delta_spread(&self) -> Option<Duration> {
        let first = *self
            .stats
            .first_delta_at
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let last = *self
            .stats
            .last_delta_at
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match (first, last) {
            (Some(first), Some(last)) => {
                Some(last.checked_duration_since(first).unwrap_or_default())
            }
            _ => None,
        }
    }

    /// Whether any stream handed out by this mock observed its
    /// [`CancelToken`] fire while streaming (see [`MockLlm::hang_forever`]
    /// for the companion never-respond mode).
    pub fn cancel_seen(&self) -> bool {
        self.stats.cancel_seen.load(Ordering::Relaxed)
    }
}

impl Default for MockLlm {
    fn default() -> Self {
        Self {
            responses: Arc::new(Mutex::new(VecDeque::new())),
            chat_calls: AtomicU64::new(0),
            hangs: AtomicBool::new(false),
            stats: Arc::new(MockStreamStats::default()),
        }
    }
}

/// Internal state of the scripted event stream: thinking deltas first, then
/// per-character text deltas (paced by the configured delay), then the
/// ordered block-level tail.
struct MockStreamState {
    /// Characters still to emit as `ThinkDelta` events, in order.
    thinking_chars: VecDeque<char>,
    /// Characters still to emit as `TextDelta` events, in order.
    chars: VecDeque<char>,
    /// Delay between consecutive character deltas; `None` emits them
    /// without pacing (the legacy single-delta delivery).
    token_delay: Option<Duration>,
    /// Whether at least one delta has been emitted (the delay applies
    /// *between* deltas, not before the first one).
    deltas_emitted: bool,
    /// Cancellation token from the [`LlmChatParams`], observed between
    /// deltas like a real provider adapter watching its HTTP connection.
    cancel: CancelToken,
    /// Shared per-mock statistics recorded as the stream yields.
    stats: Arc<MockStreamStats>,
    /// Block-level events emitted after the deltas: the full `TextPart`,
    /// any tool calls, `Usage`, then `Done`.
    tail: VecDeque<LlmEvent>,
}

impl MockStreamState {
    /// Whether the cancel token has fired; records the observation and ends
    /// the stream early so the caller stops awaiting it.
    fn cancelled(&mut self) -> bool {
        if self.cancel.is_cancelled() {
            self.stats.cancel_seen.store(true, Ordering::Relaxed);
            true
        } else {
            false
        }
    }
}

/// Turn one scripted response into the ordered `LlmEvent` stream the loop
/// engine consumes: any scripted thinking text as one `ThinkDelta` per
/// character, then one `TextDelta` per character (paced by `token_delay`
/// when configured — the *block-level* ordering is preserved by emitting the
/// full `TextPart` after the deltas), then tool calls, usage, and `Done`.
fn build_response_stream(
    response: MockResponse,
    cancel: CancelToken,
    stats: Arc<MockStreamStats>,
) -> LlmEventStream {
    let mut tail = VecDeque::new();
    if !response.text.is_empty() {
        tail.push_back(LlmEvent::TextPart {
            text: response.text.clone(),
        });
    }
    for tool_call in &response.tool_calls {
        tail.push_back(LlmEvent::ToolCall {
            tool: tool_call.name.clone(),
            args: tool_call.args.clone(),
            call_id: tool_call.id.clone(),
        });
    }
    let usage = response.usage;
    tail.push_back(LlmEvent::Usage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_tokens: usage.cache_read_tokens,
        cache_creation_tokens: usage.cache_creation_tokens,
    });
    tail.push_back(LlmEvent::Done {
        stop_reason: response.stop_reason,
    });

    let state = MockStreamState {
        thinking_chars: response.thinking.unwrap_or_default().chars().collect(),
        chars: response.text.chars().collect(),
        token_delay: response.token_delay,
        deltas_emitted: false,
        cancel,
        stats,
        tail,
    };

    Box::pin(stream::unfold(state, |mut state| async move {
        // Thinking first (a provider streams reasoning ahead of the answer),
        // then the text, then the block-level tail.
        if let Some(ch) = state.thinking_chars.pop_front() {
            if state.cancelled() {
                return None;
            }
            return Some((
                LlmEvent::ThinkDelta {
                    text: ch.to_string(),
                },
                state,
            ));
        }
        if let Some(ch) = state.chars.pop_front() {
            // Providers abort their streams when the caller's cancellation
            // fires; end the stream early so the engine stops awaiting it.
            if state.cancelled() {
                return None;
            }
            if let Some(delay) = state.token_delay
                && state.deltas_emitted
            {
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    _ = state.cancel.cancelled() => {
                        state.stats.cancel_seen.store(true, Ordering::Relaxed);
                        return None;
                    }
                }
            }
            state.stats.record_delta_yield();
            state.deltas_emitted = true;
            return Some((
                LlmEvent::TextDelta {
                    text: ch.to_string(),
                },
                state,
            ));
        }
        state.tail.pop_front().map(|event| (event, state))
    }))
}

impl Llm for MockLlm {
    fn system_prompt(&self) -> &str {
        "You are a scripted test assistant. Reply with the queued mock response."
    }

    fn model_name(&self) -> &str {
        "mock-llm"
    }

    fn capability(&self) -> &ModelCapability {
        static CAPABILITY: ModelCapability = ModelCapability {
            image_in: false,
            video_in: false,
            audio_in: false,
            dynamically_loaded_tools: false,
        };
        &CAPABILITY
    }

    fn chat(&self, params: LlmChatParams) -> EngineResult<LlmEventStream> {
        self.chat_calls.fetch_add(1, Ordering::Relaxed);
        if self.hangs.load(Ordering::Relaxed) {
            // Never responds: the stream stays pending forever, so only the
            // caller's own deadline (the server's engine-turn timeout) ends
            // the turn. The mock was still consulted — `chat_call_count`
            // reflects it.
            return Ok(Box::pin(stream::pending()));
        }
        let response = self
            .responses
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pop_front();
        let Some(response) = response else {
            return Err(EngineError::ProviderError(
                "mock LLM has no scripted responses left; queue one with MockLlm::push_response"
                    .to_string(),
            ));
        };
        Ok(build_response_stream(
            response,
            params.cancel,
            Arc::clone(&self.stats),
        ))
    }
}

/// An [`LlmFactory`] that serves one shared [`MockLlm`] to every agent,
/// regardless of its [`AgentScopeConfig`].
#[derive(Debug, Clone)]
pub struct MockLlmFactory {
    llm: Arc<MockLlm>,
}

impl MockLlmFactory {
    /// Wrap the shared mock the factory will serve to every agent.
    pub fn new(llm: Arc<MockLlm>) -> Self {
        Self { llm }
    }

    /// Create a factory preloaded with a plain-text response.
    ///
    /// Calls [`MockLlm::new`] and [`MockLlm::push_text_response`] on its
    /// internal mock; use [`Self::shared`] to reach it for assertions.
    pub fn with_text(text: &str) -> Self {
        let llm = Arc::new(MockLlm::new());
        llm.push_text_response(text);
        Self::new(llm)
    }

    /// The shared mock, so tests can queue responses and assert call counts.
    pub fn shared(&self) -> Arc<MockLlm> {
        Arc::clone(&self.llm)
    }
}

impl LlmFactory for MockLlmFactory {
    fn create_llm(&self, _config: &AgentScopeConfig) -> Result<Arc<dyn Llm>, String> {
        Ok(Arc::clone(&self.llm) as Arc<dyn Llm>)
    }
}
