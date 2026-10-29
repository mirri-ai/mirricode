//! `chat_with_retry` — calls `Llm::chat()` with exponential backoff and jitter.
//!
//! The retry loop:
//! 0. Materialize media once (`llm.materialize_media(&mut params)`, T11a) —
//!    retries reuse the already materialized params.
//! 1. For each attempt (1..=max_attempts):
//!    a. Call `llm.chat(params)` to get an event stream.
//!    b. If `chat()` returns an error → check retryability.
//!    c. Consume the stream, collecting text, think, tool calls, usage, stop reason.
//!    d. If the stream emits an `Error` event → check retryability.
//!    e. On success → return [`ChatResult`].
//!    f. On retryable error → compute backoff, emit `StepRetrying` live event, sleep (cancelable).
//!    g. On non-retryable error or exhausted attempts → throw.
//!
//! Backoff uses exponential growth with jitter:
//! ```text
//! base = min(BASE_DELAY_MS * RETRY_FACTOR^(attempt-1), MAX_DELAY_MS)
//! delay = base + random_jitter * JITTER_FACTOR * base
//! ```

use crate::error::{EngineError, EngineResult};
use crate::llm::{Llm, LlmChatParams, LlmChatResponse, ProviderErrorInfo};
use crate::loop_engine::events::{LiveEvent, LiveEventCallback};
use crate::loop_engine::shared::{
    CollectedStream, backoff_delay, cancelable_sleep, collect_stream_events, random_jitter,
};
use crate::turn_flow::wait_registry::{WaitKind, WaitSubject};

// Re-export shared constants for tests in this module.
#[cfg(test)]
use crate::llm::StopReason;
#[cfg(test)]
use crate::loop_engine::shared::{
    BASE_DELAY_MS, JITTER_FACTOR, MAX_DELAY_MS, RETRY_FACTOR, normalize_stop_reason,
};

/// Default maximum retry attempts.
const DEFAULT_MAX_RETRY_ATTEMPTS: u32 = 10;

/// Result of consuming the LLM event stream, including data not present in
/// [`LlmChatResponse`] (think text) that the loop needs for recording.
#[derive(Debug, Clone)]
pub struct ChatResult {
    /// The normalized chat response (stop reason, text, tool calls, usage).
    pub response: LlmChatResponse,
    /// Concatenated thinking/reasoning text from `ThinkPart` events.
    pub think_text: String,
    /// True when the stream ended because of cancellation (no `Done` and no
    /// `Error` event) — the response carries the partial delivered output
    /// and the caller must close the step as interrupted (方式 B 取消闭合)
    /// rather than treating it as a complete answer.
    pub interrupted: bool,
}

/// Try to extract a `Retry-After` header value (in milliseconds) from an
/// error message.
///
/// Some providers include `Retry-After: N` in the error body. This function
/// searches for patterns like `"retry-after: N"` or `"retry_after": N` and
/// returns `N * 1000` (converting seconds to milliseconds).
fn read_retry_after_ms(error_message: &str) -> Option<u64> {
    let lower = error_message.to_lowercase();
    // Search for "retry-after" or "retry_after" followed by a number.
    for prefix in [
        "retry-after: ",
        "retry_after: ",
        "retry-after=",
        "retry_after=",
    ] {
        if let Some(pos) = lower.find(prefix) {
            let after = &error_message[pos + prefix.len()..];
            // Parse leading digits (possibly with a decimal point for seconds).
            let num_str: String = after
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            if let Ok(seconds) = num_str.parse::<f64>() {
                return Some((seconds * 1000.0) as u64);
            }
        }
    }
    None
}

/// Check whether an `EngineError` is retryable by consulting the LLM adapter.
///
/// For `ProviderError`, the error message is wrapped in a [`ProviderErrorInfo`]
/// and passed to `llm.is_retryable_error`. All other error variants are
/// non-retryable.
fn is_retryable(llm: &dyn Llm, error: &EngineError) -> bool {
    match error {
        EngineError::ProviderError(msg) => {
            llm.is_retryable_error(&ProviderErrorInfo::from_message(msg))
        }
        _ => false,
    }
}

/// Consume an `LlmEventStream`, collecting all data into a [`ChatResult`].
///
/// Forwards live events (TextDelta, ThinkDelta, ToolCallPart) via the optional
/// callback. Returns an error if the stream emits an `Error` event. When the
/// stream ends because of cancellation (no `Done` and no `Error` event while
/// the cancel token is set), returns `Ok` with `interrupted: true` so the
/// partial delivered output survives into the turn's append-only closure
/// (方式 B 取消闭合) instead of being dropped with the failed request.
///
/// ADR-006: while NO first event has arrived yet, the collection is under a
/// `WaitingProvider` registration in the turn's wait registry — the wait
/// shape the watchdog's probe reads (the `LlmPending` heartbeat feed stays
/// wired for the migration period; the registration is the pull-model fact).
async fn consume_stream(
    stream: crate::llm::LlmEventStream,
    live_callback: Option<&LiveEventCallback>,
    cancel: &crate::cancel::CancelToken,
    wait_registry: Option<&std::sync::Arc<crate::turn_flow::WaitRegistry>>,
) -> EngineResult<ChatResult> {
    let _provider_wait = wait_registry
        .as_ref()
        .map(|registry| registry.enter(WaitKind::WaitingProvider, WaitSubject::None));
    let collected = collect_stream_events(stream, live_callback).await;

    if let Some(msg) = collected.stream_error {
        // A stream error that raced with cancellation is classified as an
        // abort (never retried, never surfaced as a provider failure) so the
        // turn closes through the cancellation path.
        if cancel.is_cancelled() {
            return Err(EngineError::Aborted(format!(
                "cancelled during llm stream: {msg}"
            )));
        }
        return Err(EngineError::ProviderError(msg));
    }

    let interrupted = cancel.is_cancelled() && collected.raw_stop_reason.is_none();
    let stop_reason = collected.derived_stop_reason();
    let CollectedStream {
        text_parts,
        think_parts,
        tool_calls,
        usage,
        ..
    } = collected;

    let response = LlmChatResponse {
        stop_reason,
        text: text_parts.join(""),
        tool_calls,
        usage,
    };

    Ok(ChatResult {
        response,
        think_text: think_parts.join(""),
        interrupted,
    })
}

/// Classify a provider error from `chat()`/stream consumption: quota
/// exhaustion (P8b-1) is terminal and surfaces as
/// [`EngineError::ProviderQuotaExhausted`] carrying the provider's
/// `Retry-After` hint; everything else passes through unchanged.
fn classify_provider_error(error: EngineError) -> EngineError {
    if let EngineError::ProviderError(message) = &error {
        let info = ProviderErrorInfo::from_message(message.as_str());
        if info.is_quota_exhausted() {
            return EngineError::ProviderQuotaExhausted {
                message: message.clone(),
                retry_after_ms: read_retry_after_ms(message),
            };
        }
    }
    error
}

/// Classify a *surfaced* provider error into its fine-grained domain-code
/// variant (T7, F-P8-8): rate limit / auth failure / connection failure,
/// via the single mapping table
/// [`ProviderErrorInfo::provider_domain_code`]. The auth-failure branch
/// attaches the adapter's provider attribution (O-5, ADR-009) so the
/// surfaced error names the provider whose credential failed.
///
/// Runs only when the error is about to surface (after the retry
/// decision), never before: a transient `ProviderError` must keep its
/// plain shape while the retry loop consults `llm.is_retryable_error`,
/// and only the final verdict — the error a client sees on the wire —
/// gets the precise domain code (`provider.rate_limit`,
/// `provider.auth_error`, `provider.connection_error`). Quota exhaustion
/// is classified earlier ([`classify_provider_error`]) because it is
/// terminal and must short-circuit the retry decision itself.
fn classify_surface_error(llm: &dyn Llm, error: EngineError) -> EngineError {
    if let EngineError::ProviderError(message) = &error {
        return match ProviderErrorInfo::from_message(message.as_str()).provider_domain_code() {
            "provider.rate_limit" => EngineError::ProviderRateLimit(message.clone()),
            "provider.auth_error" => EngineError::ProviderAuthError {
                message: message.clone(),
                provider_id: llm.provider_id().map(str::to_string),
            },
            "provider.connection_error" => EngineError::ProviderConnectionError(message.clone()),
            _ => error,
        };
    }
    error
}

/// Chat with retry. Calls `llm.chat()` with exponential backoff.
///
/// Retry-After header value (if present in the error) overrides the computed
/// backoff. If `llm.is_retryable_error()` returns `false` for an error, no
/// retry is attempted.
///
/// # Arguments
/// * `llm` — The LLM adapter.
/// * `params` — Chat parameters (messages, tools, cancel token).
/// * `max_attempts` — Maximum number of attempts (including the first).
/// * `live_callback` — Optional callback for live events (StepRetrying, deltas).
/// * `turn_id` — Turn ID for logging.
/// * `step` — Step number for logging.
pub async fn chat_with_retry(
    llm: &dyn Llm,
    mut params: LlmChatParams,
    max_attempts: u32,
    live_callback: Option<&LiveEventCallback>,
    turn_id: &str,
    step: u32,
    wait_registry: Option<&std::sync::Arc<crate::turn_flow::WaitRegistry>>,
) -> EngineResult<ChatResult> {
    let max_attempts = if max_attempts == 0 {
        DEFAULT_MAX_RETRY_ATTEMPTS
    } else {
        max_attempts
    };

    // Media materialization (T11a): rewrite inline data-URI images onto
    // provider file references exactly once per chat_with_retry, before the
    // attempt loop — every attempt (including retries) reuses the already
    // materialized params. A materialization error keeps the inline payloads
    // and is only warned: materialization never fails the turn.
    if let Err(error) = llm.materialize_media(&mut params).await {
        tracing::warn!(
            turn_id = %turn_id,
            step = %step,
            %error,
            "media materialization failed; sending inline media unchanged"
        );
    }

    for attempt in 1..=max_attempts {
        tracing::debug!(
            turn_id = %turn_id,
            step = %step,
            attempt = %attempt,
            max_attempts = %max_attempts,
            "Calling LLM chat"
        );

        // Step 1: Call llm.chat() to get the stream.
        let stream_result = llm.chat(params.clone());

        let stream = match stream_result {
            Ok(s) => s,
            Err(error) => {
                // chat() itself returned an error.
                if params.cancel.is_cancelled() && !error.is_abort() {
                    // An error racing a cancellation is classified as an
                    // abort so the turn closes through the cancellation path.
                    return Err(EngineError::Aborted(format!(
                        "cancelled during llm chat: {error}"
                    )));
                }
                // Quota exhaustion is terminal (P8b-1): classify before the
                // retry decision so it is never retried and surfaces with
                // the provider's Retry-After hint.
                let error = classify_provider_error(error);
                if attempt >= max_attempts || !is_retryable(llm, &error) {
                    tracing::warn!(
                        turn_id = %turn_id,
                        step = %step,
                        attempt = %attempt,
                        error = %error,
                        "LLM chat failed (non-retryable or exhausted)"
                    );
                    // Fine-grained domain-code classification happens only
                    // at surfacing (T7) — the retry semantics above are
                    // untouched.
                    return Err(classify_surface_error(llm, error));
                }

                // Retryable error — compute delay and sleep.
                let delay = compute_retry_delay(&error, attempt);
                emit_retry_event(live_callback, attempt, max_attempts, delay);
                cancelable_sleep(&params.cancel, delay).await?;
                continue;
            }
        };

        // Step 2: Consume the stream.
        match consume_stream(stream, live_callback, &params.cancel, wait_registry).await {
            Ok(result) => return Ok(result),
            Err(error) => {
                if params.cancel.is_cancelled() && !error.is_abort() {
                    return Err(EngineError::Aborted(format!(
                        "cancelled during llm stream: {error}"
                    )));
                }
                // Quota exhaustion is terminal (P8b-1): classify before the
                // retry decision so it is never retried and surfaces with
                // the provider's Retry-After hint.
                let error = classify_provider_error(error);
                if attempt >= max_attempts || !is_retryable(llm, &error) {
                    tracing::warn!(
                        turn_id = %turn_id,
                        step = %step,
                        attempt = %attempt,
                        error = %error,
                        "LLM stream error (non-retryable or exhausted)"
                    );
                    // Fine-grained domain-code classification happens only
                    // at surfacing (T7) — the retry semantics above are
                    // untouched.
                    return Err(classify_surface_error(llm, error));
                }

                // Retryable stream error — compute delay and sleep.
                let delay = compute_retry_delay(&error, attempt);
                emit_retry_event(live_callback, attempt, max_attempts, delay);
                cancelable_sleep(&params.cancel, delay).await?;
                continue;
            }
        }
    }

    // Should be unreachable — the loop either returns or continues.
    Err(EngineError::ProviderError(
        "Retry loop exhausted without returning".to_string(),
    ))
}

/// Compute the retry delay, preferring `Retry-After` header value if present.
fn compute_retry_delay(error: &EngineError, attempt: u32) -> u64 {
    if let EngineError::ProviderError(msg) = error
        && let Some(retry_after_ms) = read_retry_after_ms(msg)
    {
        return retry_after_ms;
    }
    backoff_delay(attempt, random_jitter())
}

/// Emit a `StepRetrying` live event if a callback is present.
fn emit_retry_event(
    live_callback: Option<&LiveEventCallback>,
    failed_attempt: u32,
    max_attempts: u32,
    delay_ms: u64,
) {
    if let Some(cb) = live_callback {
        cb(LiveEvent::StepRetrying {
            failed_attempt,
            next_attempt: failed_attempt + 1,
            max_attempts,
            delay_ms,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_grow_exponentially_when_attempt_number_increases() {
        let delay_1 = backoff_delay(1, 0.0);
        let delay_2 = backoff_delay(2, 0.0);
        let delay_3 = backoff_delay(3, 0.0);

        assert_eq!(delay_1, BASE_DELAY_MS);
        assert_eq!(delay_2, BASE_DELAY_MS * RETRY_FACTOR);
        assert_eq!(delay_3, BASE_DELAY_MS * RETRY_FACTOR * RETRY_FACTOR);
    }

    #[test]
    fn should_cap_delay_at_max_when_attempt_number_is_high() {
        let delay = backoff_delay(20, 0.0);
        assert_eq!(delay, MAX_DELAY_MS);
    }

    #[test]
    fn should_add_jitter_to_delay_when_jitter_factor_is_nonzero() {
        let base = backoff_delay(1, 0.0);
        let with_jitter = backoff_delay(1, 0.5);
        assert!(with_jitter > base);
        assert!(with_jitter < base + (JITTER_FACTOR * base as f64) as u64 + 1);
    }

    #[test]
    fn should_parse_retry_after_seconds_when_present_in_error_message() {
        assert_eq!(read_retry_after_ms("retry-after: 5"), Some(5000));
        assert_eq!(read_retry_after_ms("Retry-After: 2"), Some(2000));
    }

    #[test]
    fn should_return_none_when_error_message_lacks_retry_after_header() {
        assert_eq!(read_retry_after_ms("internal server error"), None);
    }

    #[test]
    fn should_map_completed_to_end_turn_when_no_tool_calls() {
        assert_eq!(
            normalize_stop_reason("completed", false),
            StopReason::EndTurn
        );
    }

    #[test]
    fn should_map_completed_to_tool_use_when_tool_calls_present() {
        assert_eq!(
            normalize_stop_reason("completed", true),
            StopReason::ToolUse
        );
    }

    #[test]
    fn should_map_tool_calls_string_to_unknown_when_no_tool_calls() {
        assert_eq!(
            normalize_stop_reason("tool_calls", false),
            StopReason::Unknown
        );
    }

    #[test]
    fn should_map_truncated_to_max_tokens_when_normalizing_stop_reason() {
        assert_eq!(
            normalize_stop_reason("truncated", false),
            StopReason::MaxTokens
        );
    }

    #[test]
    fn should_return_unknown_when_stop_reason_string_is_unrecognized() {
        assert_eq!(
            normalize_stop_reason("something_else", false),
            StopReason::Unknown
        );
    }

    // -- P8b-1: quota-exhausted 429 classification --------------------------

    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicU32, Ordering};

    use crate::cancel::CancelToken;
    use crate::llm::{LlmEvent, LlmEventStream, ModelCapability};

    /// A minimal LLM stub for the retry-layer tests: pops scripted outcomes
    /// (an immediate error, or a stream of events) from a queue and counts
    /// `chat()` calls.
    struct ScriptedLlm {
        outcomes: std::sync::Mutex<Vec<Result<Vec<LlmEvent>, EngineError>>>,
        chat_calls: AtomicU32,
        retryable: bool,
    }

    impl ScriptedLlm {
        fn new(outcomes: Vec<Result<Vec<LlmEvent>, EngineError>>) -> Self {
            Self {
                outcomes: std::sync::Mutex::new(outcomes),
                chat_calls: AtomicU32::new(0),
                retryable: true,
            }
        }

        fn calls(&self) -> u32 {
            self.chat_calls.load(Ordering::SeqCst)
        }
    }

    impl Llm for ScriptedLlm {
        fn system_prompt(&self) -> &str {
            ""
        }

        fn model_name(&self) -> &str {
            "scripted"
        }

        fn capability(&self) -> &ModelCapability {
            static CAP: std::sync::OnceLock<ModelCapability> = std::sync::OnceLock::new();
            CAP.get_or_init(ModelCapability::default)
        }

        fn is_retryable_error(&self, _error: &ProviderErrorInfo) -> bool {
            self.retryable
        }

        fn chat(&self, _params: LlmChatParams) -> EngineResult<LlmEventStream> {
            self.chat_calls.fetch_add(1, Ordering::SeqCst);
            let mut queue = self.outcomes.lock().unwrap();
            match queue.remove(0) {
                Ok(events) => Ok(Box::pin(futures::stream::iter(events))),
                Err(error) => Err(error),
            }
        }
    }

    fn completed_stream() -> Vec<LlmEvent> {
        vec![
            LlmEvent::TextPart {
                text: "ok".to_string(),
            },
            LlmEvent::Done {
                stop_reason: "completed".to_string(),
            },
        ]
    }

    #[tokio::test]
    async fn should_not_retry_and_surface_retry_after_when_429_is_quota_exhausted() {
        // Given an LLM whose every call fails with the Kimi quota wording,
        // and a provider that would otherwise retry 429s
        let llm = ScriptedLlm::new(vec![Err(EngineError::ProviderError(
            "429 Too Many Requests: You exceeded your current token quota, \
             please check your account balance and recharge your account \
             (retry-after: 3)"
                .to_string(),
        ))]);

        // When a chat is attempted with a retry budget
        let result = chat_with_retry(
            &llm,
            LlmChatParams {
                messages: Vec::new(),
                tools: Vec::new(),
                cancel: CancelToken::new(),
                request_log_fields: None,
                cache_root_id: None,
            },
            5,
            None,
            "turn-1",
            1,
            None,
        )
        .await;

        // Then the quota exhaustion is terminal: surfaced as the dedicated
        // error carrying the provider's Retry-After hint, with exactly one
        // call (no retry ever happened).
        let error = result.expect_err("quota exhaustion must surface an error");
        match error {
            EngineError::ProviderQuotaExhausted { retry_after_ms, .. } => {
                assert_eq!(retry_after_ms, Some(3000));
            }
            other => panic!("expected ProviderQuotaExhausted, got {other:?}"),
        }
        assert_eq!(llm.calls(), 1, "a quota-exhausted 429 must never retry");
    }

    #[tokio::test]
    async fn should_retry_transient_rate_limit_when_429_is_a_throttle_not_quota() {
        // Given an LLM whose first call hits a transient rate limit (the
        // "token quota per minute" throttle wording — deliberately NOT
        // billing-anchored) and whose second call succeeds
        let llm = ScriptedLlm::new(vec![
            Err(EngineError::ProviderError(
                "429 Too Many Requests: token quota per minute exceeded, slow down".to_string(),
            )),
            Ok(completed_stream()),
        ]);

        // When a chat is attempted with a retry budget
        let result = chat_with_retry(
            &llm,
            LlmChatParams {
                messages: Vec::new(),
                tools: Vec::new(),
                cancel: CancelToken::new(),
                request_log_fields: None,
                cache_root_id: None,
            },
            5,
            None,
            "turn-1",
            1,
            None,
        )
        .await;

        // Then the transient throttle was retried and the chat succeeded.
        assert!(result.is_ok(), "a transient 429 must stay retryable");
        assert_eq!(llm.calls(), 2);
    }

    #[test]
    fn should_classify_structured_quota_code_without_status_or_wording() {
        // Given a provider error carrying the structured quota code
        let info = ProviderErrorInfo {
            status_code: None,
            message: "request rejected".to_string(),
            error_type: Some("exceeded_current_quota_error".to_string()),
        };

        // Then it classifies as quota exhaustion (the structured code alone
        // is sufficient — the Kimi backend hoists it onto the error type).
        assert!(info.is_quota_exhausted());
    }

    #[test]
    fn should_classify_the_arrears_wording_with_the_optional_is_when_a_429_reports_it() {
        // Given a 429 whose message carries the "account is in arrears"
        // spelling (the TS regex allows the optional "is" —
        // /account (?:is )?in arrears/)
        let info = ProviderErrorInfo {
            status_code: Some(429),
            message: "HTTP 429: your account is in arrears, please top up".to_string(),
            error_type: None,
        };

        // Then it classifies as quota exhaustion — both spellings of the
        // arrears wording must hit, or the message re-parses as a
        // retryable throttle.
        assert!(
            info.is_quota_exhausted(),
            "the 'account is in arrears' spelling must classify as quota exhaustion"
        );
    }

    // -- T7 (F-P8-8): provider domain-code classification table -----------

    /// The mapping table's wordings, one representative sample per branch
    /// (status-anchored and message-only spellings both covered).
    #[test]
    fn should_classify_provider_errors_into_their_domain_codes() {
        let cases = [
            // Quota exhaustion stays the precise rust-only code.
            (
                ProviderErrorInfo {
                    status_code: Some(429),
                    message: "You exceeded your current quota, please recharge".to_string(),
                    error_type: None,
                },
                "provider.quota_exhausted",
            ),
            // Transient 429 (status and message spellings) → rate limit.
            (
                ProviderErrorInfo {
                    status_code: Some(429),
                    message: "throttled".to_string(),
                    error_type: None,
                },
                "provider.rate_limit",
            ),
            (
                ProviderErrorInfo::from_message("OpenAI API error 429 Too Many Requests"),
                "provider.rate_limit",
            ),
            (
                ProviderErrorInfo::from_message("request rate-limited, slow down"),
                "provider.rate_limit",
            ),
            // Auth failures (status and wordings) → auth error.
            (
                ProviderErrorInfo {
                    status_code: Some(401),
                    message: "Unauthorized".to_string(),
                    error_type: None,
                },
                "provider.auth_error",
            ),
            (
                ProviderErrorInfo::from_message("403 Forbidden: invalid api key"),
                "provider.auth_error",
            ),
            // Connection/timeout wordings → connection error.
            (
                ProviderErrorInfo::from_message("error sending request: connection refused"),
                "provider.connection_error",
            ),
            (
                ProviderErrorInfo::from_message("request timed out after 30s"),
                "provider.connection_error",
            ),
            // Everything else stays the generic API error.
            (
                ProviderErrorInfo::from_message("OpenAI API error 500: internal"),
                "provider.api_error",
            ),
        ];
        for (info, expected) in cases {
            assert_eq!(info.provider_domain_code(), expected, "{:?}", info.message);
        }
    }

    /// A surfaced transient rate limit (retries exhausted) carries the
    /// `provider.rate_limit` domain code — while the retry itself still
    /// happened (the classification only shapes the surfaced error).
    #[tokio::test]
    async fn should_surface_rate_limit_when_a_transient_429_exhausts_the_retry_budget() {
        // Given an LLM whose every call fails with a transient throttle and
        // would retry 429s
        let rate_limit_error = || {
            Err(EngineError::ProviderError(
                "429 Too Many Requests: token quota per minute".to_string(),
            ))
        };
        let llm = ScriptedLlm::new(vec![
            rate_limit_error(),
            rate_limit_error(),
            rate_limit_error(),
        ]);

        // When a chat is attempted with a 3-attempt budget
        let result = chat_with_retry(
            &llm,
            LlmChatParams {
                messages: Vec::new(),
                tools: Vec::new(),
                cancel: CancelToken::new(),
                request_log_fields: None,
                cache_root_id: None,
            },
            3,
            None,
            "turn-1",
            1,
            None,
        )
        .await;

        // Then all attempts ran and the surfaced error is the rate-limit
        // variant (domain code provider.rate_limit), not the generic error.
        assert_eq!(llm.calls(), 3, "a transient 429 must be retried");
        let error = result.expect_err("exhausted retries must surface an error");
        match error {
            EngineError::ProviderRateLimit(ref message) => {
                assert!(message.contains("429"), "message preserved: {message}");
                assert_eq!(error.code().as_str(), "provider.rate_limit");
            }
            other => panic!("expected ProviderRateLimit, got {other:?}"),
        }
    }

    /// A non-retryable auth failure surfaces immediately as
    /// `provider.auth_error` (no retry, precise domain code).
    #[tokio::test]
    async fn should_surface_auth_error_immediately_when_the_provider_rejects_credentials() {
        // Given an LLM whose call fails with an auth rejection (and whose
        // adapter would never retry a 401)
        let mut llm = ScriptedLlm::new(vec![Err(EngineError::ProviderError(
            "OpenAI API error 401 Unauthorized: invalid api key".to_string(),
        ))]);
        llm.retryable = false;

        // When a chat is attempted with a retry budget
        let result = chat_with_retry(
            &llm,
            LlmChatParams {
                messages: Vec::new(),
                tools: Vec::new(),
                cancel: CancelToken::new(),
                request_log_fields: None,
                cache_root_id: None,
            },
            5,
            None,
            "turn-1",
            1,
            None,
        )
        .await;

        // Then the auth failure never retried and surfaces as the
        // auth-error variant (domain code provider.auth_error).
        assert_eq!(llm.calls(), 1, "an auth failure must not be retried");
        let error = result.expect_err("auth failure must surface an error");
        match error {
            EngineError::ProviderAuthError { .. } => {
                assert_eq!(error.code().as_str(), "provider.auth_error");
            }
            other => panic!("expected ProviderAuthError, got {other:?}"),
        }
    }

    // -- O-5: provider attribution rides the surfaced auth error ----------

    /// An LLM stub carrying a provider id: the surfaced auth error must
    /// attribute to that provider (the credential-status reporting's
    /// attribution key, ADR-009).
    struct AttributedLlm {
        provider_id: Option<&'static str>,
    }

    impl Llm for AttributedLlm {
        fn provider_id(&self) -> Option<&str> {
            self.provider_id
        }

        fn system_prompt(&self) -> &str {
            ""
        }

        fn model_name(&self) -> &str {
            "attributed"
        }

        fn capability(&self) -> &ModelCapability {
            static CAP: std::sync::OnceLock<ModelCapability> = std::sync::OnceLock::new();
            CAP.get_or_init(ModelCapability::default)
        }

        fn chat(&self, _params: LlmChatParams) -> EngineResult<LlmEventStream> {
            Err(EngineError::ProviderError(
                "OpenAI API error 401 Unauthorized: invalid api key".to_string(),
            ))
        }
    }

    /// Given an attributed adapter, when a chat fails with an auth
    /// rejection, then the surfaced error carries the provider id while
    /// its Display stays the original message (wire compatibility — the
    /// attribution is a structured field, never new user-visible text).
    #[tokio::test]
    async fn should_carry_provider_attribution_when_classifying_auth_error() {
        let llm = AttributedLlm {
            provider_id: Some("openai"),
        };

        let result = chat_with_retry(
            &llm,
            LlmChatParams {
                messages: Vec::new(),
                tools: Vec::new(),
                cancel: CancelToken::new(),
                request_log_fields: None,
                cache_root_id: None,
            },
            5,
            None,
            "turn-1",
            1,
            None,
        )
        .await;

        let error = result.expect_err("the auth failure must surface");
        match &error {
            EngineError::ProviderAuthError { message, provider_id } => {
                assert_eq!(
                    provider_id.as_deref(),
                    Some("openai"),
                    "the surfaced error attributes to the adapter's provider"
                );
                assert_eq!(
                    message, "OpenAI API error 401 Unauthorized: invalid api key",
                    "the message is preserved verbatim"
                );
                assert_eq!(
                    error.to_string(),
                    "Provider authentication error: OpenAI API error 401 Unauthorized: invalid api key",
                    "Display renders the message verbatim (wire-compatible)"
                );
            }
            other => panic!("expected ProviderAuthError, got {other:?}"),
        }
    }

    /// Given an adapter without attribution (the trait default — test
    /// mocks, direct constructions), when a chat fails with an auth
    /// rejection, then the surfaced error's attribution stays `None` and
    /// is never reported (ADR-009 boundary).
    #[tokio::test]
    async fn should_leave_attribution_none_for_factories_without_provider() {
        let llm = AttributedLlm { provider_id: None };

        let result = chat_with_retry(
            &llm,
            LlmChatParams {
                messages: Vec::new(),
                tools: Vec::new(),
                cancel: CancelToken::new(),
                request_log_fields: None,
                cache_root_id: None,
            },
            5,
            None,
            "turn-1",
            1,
            None,
        )
        .await;

        let error = result.expect_err("the auth failure must surface");
        match &error {
            EngineError::ProviderAuthError { provider_id, .. } => {
                assert!(
                    provider_id.is_none(),
                    "an unattributed adapter must surface an unattributed error"
                );
            }
            other => panic!("expected ProviderAuthError, got {other:?}"),
        }
    }

    /// A connection/timeout failure surfaces as `provider.connection_error`
    /// (the TS vocabulary's retryable connection code).
    #[tokio::test]
    async fn should_surface_connection_error_when_the_transport_fails() {
        // Given an LLM whose call fails with a connection refusal (no HTTP
        // status to retry on)
        let mut llm = ScriptedLlm::new(vec![Err(EngineError::ProviderError(
            "error sending request: connection reset by peer".to_string(),
        ))]);
        llm.retryable = false;

        // When a chat is attempted
        let result = chat_with_retry(
            &llm,
            LlmChatParams {
                messages: Vec::new(),
                tools: Vec::new(),
                cancel: CancelToken::new(),
                request_log_fields: None,
                cache_root_id: None,
            },
            5,
            None,
            "turn-1",
            1,
            None,
        )
        .await;

        // Then the surfaced error carries the connection-error domain code.
        let error = result.expect_err("connection failure must surface an error");
        match error {
            EngineError::ProviderConnectionError(_) => {
                assert_eq!(error.code().as_str(), "provider.connection_error");
            }
            other => panic!("expected ProviderConnectionError, got {other:?}"),
        }
    }

    /// A generic provider failure keeps the `provider.api_error` code.
    #[tokio::test]
    async fn should_keep_the_generic_api_error_for_unclassified_provider_failures() {
        // Given an LLM whose call fails with a plain server error
        let mut llm = ScriptedLlm::new(vec![Err(EngineError::ProviderError(
            "OpenAI API error 500: internal server error".to_string(),
        ))]);
        llm.retryable = false;

        // When a chat is attempted
        let result = chat_with_retry(
            &llm,
            LlmChatParams {
                messages: Vec::new(),
                tools: Vec::new(),
                cancel: CancelToken::new(),
                request_log_fields: None,
                cache_root_id: None,
            },
            5,
            None,
            "turn-1",
            1,
            None,
        )
        .await;

        // Then the surfaced error keeps the generic provider code.
        let error = result.expect_err("the failure must surface");
        assert!(
            matches!(error, EngineError::ProviderError(_)),
            "generic failures must stay ProviderError, got {error:?}"
        );
        assert_eq!(error.code().as_str(), "provider.api_error");
    }

    // -- T11a: the media materialization hook --------------------------------

    /// An LLM that counts `chat` and `materialize_media` calls: the first
    /// chat fails with a retryable error, the second succeeds — proving the
    /// retry loop reuses the materialized params instead of re-running the
    /// hook.
    struct MaterializeSpyLlm {
        chat_calls: AtomicU32,
        materialize_calls: AtomicU32,
    }

    impl MaterializeSpyLlm {
        fn new() -> Self {
            Self {
                chat_calls: AtomicU32::new(0),
                materialize_calls: AtomicU32::new(0),
            }
        }
    }

    impl Llm for MaterializeSpyLlm {
        fn system_prompt(&self) -> &str {
            ""
        }

        fn model_name(&self) -> &str {
            "spy"
        }

        fn capability(&self) -> &ModelCapability {
            static CAP: std::sync::OnceLock<ModelCapability> = std::sync::OnceLock::new();
            CAP.get_or_init(ModelCapability::default)
        }

        fn is_retryable_error(&self, _error: &ProviderErrorInfo) -> bool {
            true
        }

        fn chat(&self, _params: LlmChatParams) -> EngineResult<LlmEventStream> {
            let call = self.chat_calls.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                Err(EngineError::ProviderError(
                    "500 transient server error".to_string(),
                ))
            } else {
                Ok(Box::pin(futures::stream::iter(completed_stream())))
            }
        }

        fn materialize_media<'a>(
            &'a self,
            _params: &'a mut LlmChatParams,
        ) -> Pin<Box<dyn Future<Output = EngineResult<()>> + Send + 'a>> {
            self.materialize_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
    }

    #[tokio::test]
    async fn should_materialize_media_exactly_once_when_the_chat_is_retried() {
        // Given an LLM whose first chat call fails retryably
        let llm = MaterializeSpyLlm::new();

        // When a chat with retry runs to success
        let result = chat_with_retry(
            &llm,
            LlmChatParams {
                messages: Vec::new(),
                tools: Vec::new(),
                cancel: CancelToken::new(),
                request_log_fields: None,
                cache_root_id: None,
            },
            5,
            None,
            "turn-1",
            1,
            None,
        )
        .await;

        // Then the chat was retried but materialization ran exactly once —
        // the retry reused the already materialized params
        assert!(result.is_ok(), "chat must succeed on the second attempt");
        assert_eq!(llm.chat_calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            llm.materialize_calls.load(Ordering::SeqCst),
            1,
            "retries must not re-run materialization"
        );
    }

    #[tokio::test]
    async fn should_not_fail_the_chat_when_materialization_errors() {
        // Given an LLM whose materialization fails and whose chat succeeds
        struct FailingMaterializeLlm;
        impl Llm for FailingMaterializeLlm {
            fn system_prompt(&self) -> &str {
                ""
            }
            fn model_name(&self) -> &str {
                "failing"
            }
            fn capability(&self) -> &ModelCapability {
                static CAP: std::sync::OnceLock<ModelCapability> = std::sync::OnceLock::new();
                CAP.get_or_init(ModelCapability::default)
            }
            fn chat(&self, _params: LlmChatParams) -> EngineResult<LlmEventStream> {
                Ok(Box::pin(futures::stream::iter(completed_stream())))
            }
            fn materialize_media<'a>(
                &'a self,
                _params: &'a mut LlmChatParams,
            ) -> Pin<Box<dyn Future<Output = EngineResult<()>> + Send + 'a>> {
                Box::pin(async { Err(EngineError::ProviderError("upload exploded".to_string())) })
            }
        }
        let llm = FailingMaterializeLlm;

        // When a chat with retry runs
        let result = chat_with_retry(
            &llm,
            LlmChatParams {
                messages: Vec::new(),
                tools: Vec::new(),
                cancel: CancelToken::new(),
                request_log_fields: None,
                cache_root_id: None,
            },
            2,
            None,
            "turn-1",
            1,
            None,
        )
        .await;

        // Then the chat still succeeded — a materialization error never
        // fails the turn
        assert!(result.is_ok(), "materialization errors must degrade");
    }

    #[test]
    fn should_default_materialize_media_to_a_no_op_when_not_overridden() {
        // Given an LLM that does not override the hook (the MockLlm shape)
        let llm = ScriptedLlm::new(vec![Ok(completed_stream())]);

        // When the default hook runs against arbitrary params
        let mut params = LlmChatParams {
            messages: Vec::new(),
            tools: Vec::new(),
            cancel: CancelToken::new(),
            request_log_fields: None,
            cache_root_id: None,
        };
        let result = futures::executor::block_on(llm.materialize_media(&mut params));

        // Then it succeeds and leaves the params untouched
        assert!(result.is_ok());
        assert!(params.messages.is_empty());
    }
}
