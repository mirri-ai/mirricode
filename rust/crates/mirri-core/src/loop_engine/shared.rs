//! Shared utilities used by both [`crate::loop_engine::retry`] (the main
//! agent loop's retry path) and [`crate::compaction::round`] (the compaction
//! round's fallback chain).
//!
//! Previously, `consume_stream`, `normalize_stop_reason`, `backoff_delay`,
//! `random_jitter`, and `cancelable_sleep` were duplicated across the two
//! modules. This module centralizes them to prevent drift.

use std::time::Duration;

use futures::StreamExt;

use crate::cancel::CancelToken;
use crate::error::{EngineError, EngineResult};
use crate::llm::{LlmEvent, LlmEventStream, LlmToolCall, LlmUsage, StopReason};
use crate::loop_engine::events::{LiveEvent, LiveEventCallback};

/// The user-facing message of a turn aborted by the [`crate::loop_engine`]'s
/// turn-abort guard — the engine-loop-generic wording (the loop only knows
/// its guard predicate tripped, never WHY; the domain cause lives in the
/// guard-installing layer's warn log). Shared by both poll sites — the
/// between-steps check (`run_turn`) and the pre-tool-batch check
/// (`turn_step`) — so the two surfaces never drift apart.
pub const TURN_ABORTED_BY_GUARD_MESSAGE: &str = "turn aborted by guard";

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Base delay for the first retry (milliseconds).
pub(crate) const BASE_DELAY_MS: u64 = 500;
/// Maximum delay cap (milliseconds).
pub(crate) const MAX_DELAY_MS: u64 = 32_000;
/// Exponential growth factor.
pub(crate) const RETRY_FACTOR: u64 = 2;
/// Jitter factor — adds up to this fraction of the base delay as random jitter.
pub(crate) const JITTER_FACTOR: f64 = 0.25;

// ---------------------------------------------------------------------------
// Backoff / jitter / sleep
// ---------------------------------------------------------------------------

/// Compute the backoff delay for a given attempt number.
///
/// ```text
/// base = min(BASE_DELAY_MS * RETRY_FACTOR^(attempt-1), MAX_DELAY_MS)
/// delay = base + jitter * JITTER_FACTOR * base
/// ```
///
/// `jitter` should be a value in `[0.0, 1.0)`.
pub(crate) fn backoff_delay(attempt: u32, jitter: f64) -> u64 {
    let exponent = attempt.saturating_sub(1) as u64;
    let base = BASE_DELAY_MS
        .saturating_mul(RETRY_FACTOR.saturating_pow(exponent as u32))
        .min(MAX_DELAY_MS);
    let jitter_amount = (jitter * JITTER_FACTOR * base as f64) as u64;
    base + jitter_amount
}

/// Generate a pseudo-random jitter factor in `[0.0, 1.0)`.
///
/// Uses `std::time::SystemTime` nanoseconds as a lightweight entropy source.
/// This is not cryptographically secure, but is sufficient for retry jitter
/// and avoids adding a `rand` dependency.
pub(crate) fn random_jitter() -> f64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    (nanos as f64) / 1_000_000_000.0
}

/// Sleep for the given delay, cancelable via the `CancelToken`.
///
/// Returns `EngineError::Aborted` if cancelled during the sleep.
pub(crate) async fn cancelable_sleep(cancel: &CancelToken, delay_ms: u64) -> EngineResult<()> {
    if delay_ms == 0 {
        cancel.check()?;
        return Ok(());
    }

    tokio::select! {
        _ = tokio::time::sleep(Duration::from_millis(delay_ms)) => {
            Ok(())
        }
        _ = cancel.cancelled() => {
            Err(EngineError::Aborted("cancelled during retry delay".into()))
        }
    }
}

// ---------------------------------------------------------------------------
// normalize_stop_reason
// ---------------------------------------------------------------------------

/// Normalize a raw provider stop reason string to [`StopReason`].
///
/// Mapping (matching the TypeScript `deriveStopReason`):
/// - `"truncated"` → `MaxTokens`
/// - `"filtered"` → `Filtered`
/// - `"paused"` → `Paused`
/// - `"other"` → `Unknown`
/// - `"completed"` or `""` → `ToolUse` if `has_tool_calls`, else `EndTurn`
/// - `"tool_calls"` → `ToolUse` if `has_tool_calls`, else `Unknown`
/// - anything else → `Unknown`
pub(crate) fn normalize_stop_reason(raw: &str, has_tool_calls: bool) -> StopReason {
    match raw {
        "truncated" => StopReason::MaxTokens,
        "filtered" => StopReason::Filtered,
        "paused" => StopReason::Paused,
        "other" => StopReason::Unknown,
        "completed" | "" => {
            if has_tool_calls {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            }
        }
        "tool_calls" => {
            if has_tool_calls {
                StopReason::ToolUse
            } else {
                StopReason::Unknown
            }
        }
        _ => StopReason::Unknown,
    }
}

// ---------------------------------------------------------------------------
// Stream consumption (shared core)
// ---------------------------------------------------------------------------

/// All data collected from consuming an [`LlmEventStream`].
///
/// Both the retry path and the compaction round need to drain an
/// `LlmEventStream` into structured data. This struct holds the raw collected
/// fields; each caller then assembles its own result type from it.
#[derive(Default)]
pub(crate) struct CollectedStream {
    /// Concatenated text from `TextPart` events.
    pub text_parts: Vec<String>,
    /// Concatenated thinking/reasoning text from `ThinkPart` events.
    pub think_parts: Vec<String>,
    /// Tool calls from `ToolCall` events.
    pub tool_calls: Vec<LlmToolCall>,
    /// Token usage from the `Usage` event (if any).
    pub usage: LlmUsage,
    /// Raw stop reason string from the `Done` event (if any).
    pub raw_stop_reason: Option<String>,
    /// Error message from an `Error` event (if any).
    pub stream_error: Option<String>,
}

impl CollectedStream {
    /// Derive the normalized [`StopReason`] from the collected data.
    ///
    /// If the stream emitted a `Done` event, the raw stop reason is normalized
    /// via [`normalize_stop_reason`]. Otherwise, the stop reason is inferred
    /// from whether tool calls were collected.
    pub fn derived_stop_reason(&self) -> StopReason {
        let has_tool_calls = !self.tool_calls.is_empty();
        match &self.raw_stop_reason {
            Some(raw) => normalize_stop_reason(raw, has_tool_calls),
            None => {
                if has_tool_calls {
                    StopReason::ToolUse
                } else {
                    StopReason::EndTurn
                }
            }
        }
    }

    /// Convert into an `EngineError` if a stream error was collected.
    #[allow(dead_code)]
    pub fn into_error(self) -> Option<EngineError> {
        self.stream_error.map(EngineError::ProviderError)
    }
}

/// Consume an [`LlmEventStream`], collecting all data into a [`CollectedStream`].
///
/// If `live_callback` is `Some`, incremental delta events (`TextDelta`,
/// `ThinkDelta`, `ToolCallPart`) are forwarded to it. Block-level events are
/// always collected into the returned [`CollectedStream`].
pub(crate) async fn collect_stream_events(
    stream: LlmEventStream,
    live_callback: Option<&LiveEventCallback>,
) -> CollectedStream {
    let mut collected = CollectedStream::default();
    let mut stream = stream;

    // The in-flight heartbeat (stall-watchdog feed): while NO event has
    // arrived yet, a ticker beats `LiveEvent::LlmPending` through the
    // live callback — a provider whose first token takes minutes must
    // not read as a dead engine. The ticker STOPS at the first event
    // (deltas are their own liveness signal) so mid-stream silence
    // stays watchdog-detectable. Callback-less collection (headless
    // callers) never beats at all.
    let mut first_event_seen = false;
    if live_callback.is_some() {
        let interval = llm_pending_heartbeat_interval();
        let started = tokio::time::Instant::now();
        let mut heartbeat =
            tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        while !first_event_seen {
            tokio::select! {
                event = stream.next() => {
                    match event {
                        Some(event) => {
                            first_event_seen = true;
                            collect_llm_event(&mut collected, live_callback, event);
                        }
                        None => return collected,
                    }
                }
                _ = heartbeat.tick() => {
                    if let Some(cb) = live_callback {
                        cb(LiveEvent::LlmPending {
                            elapsed_ms: started.elapsed().as_millis() as u64,
                        });
                    }
                }
            }
        }
    }
    // First event landed (or no callback): plain collection from here.
    while let Some(event) = stream.next().await {
        collect_llm_event(&mut collected, live_callback, event);
    }
    collected
}

/// The in-flight heartbeat cadence. Production: 30s — many beats inside
/// the engine's 600s stall window and under the subagent forwarding poll's
/// 75s cadence. Tests: 500ms so paused-time tests observe several beats
/// per window.
#[cfg(test)]
fn llm_pending_heartbeat_interval() -> std::time::Duration {
    std::time::Duration::from_millis(500)
}

#[cfg(not(test))]
fn llm_pending_heartbeat_interval() -> std::time::Duration {
    std::time::Duration::from_secs(30)
}

/// Fold one LLM stream event into `collected`, forwarding the delta
/// variants through the live callback (the old inline match, unchanged).
fn collect_llm_event(
    collected: &mut CollectedStream,
    live_callback: Option<&LiveEventCallback>,
    event: crate::llm::LlmEvent,
) {
    match event {
        LlmEvent::TextDelta { text } => {
            if let Some(cb) = live_callback {
                cb(LiveEvent::TextDelta { text });
            }
        }
        LlmEvent::ThinkDelta { text } => {
            if let Some(cb) = live_callback {
                cb(LiveEvent::ThinkDelta { text });
            }
        }
        LlmEvent::ToolCallPart {
            index,
            arguments_part,
            call_id,
            tool_name,
        } => {
            if let Some(cb) = live_callback {
                cb(LiveEvent::ToolCallPart {
                    index,
                    arguments_part,
                    call_id,
                    tool_name,
                });
            }
        }
        LlmEvent::TextPart { text } => {
            collected.text_parts.push(text);
        }
        LlmEvent::ThinkPart { text } => {
            collected.think_parts.push(text);
        }
        LlmEvent::ToolCall {
            tool,
            args,
            call_id,
        } => {
            collected.tool_calls.push(LlmToolCall {
                id: call_id,
                name: tool,
                args,
            });
        }
        LlmEvent::Usage {
            input_tokens,
            output_tokens,
            cache_read_tokens,
            cache_creation_tokens,
        } => {
            // Providers split usage across several `Usage` events:
            // Anthropic reports input (plus cache buckets) in
            // `message_start` and output in `message_delta`, while Google
            // repeats cumulative `usageMetadata` per chunk. Merge field
            // by field — a non-zero reported value wins over the
            // previously collected one — so the split events combine
            // into the full four-bucket accounting.
            let usage = &mut collected.usage;
            if input_tokens > 0 {
                usage.input_tokens = input_tokens;
            }
            if output_tokens > 0 {
                usage.output_tokens = output_tokens;
            }
            if cache_read_tokens > 0 {
                usage.cache_read_tokens = cache_read_tokens;
            }
            if cache_creation_tokens > 0 {
                usage.cache_creation_tokens = cache_creation_tokens;
            }
        }
        LlmEvent::Done { stop_reason } => {
            collected.raw_stop_reason = Some(stop_reason);
        }
        LlmEvent::Error { message } => {
            collected.stream_error = Some(message);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::LlmEvent;

    /// Build a boxed stream of events for [`collect_stream_events`].
    fn stream_of(events: Vec<LlmEvent>) -> LlmEventStream {
        Box::pin(futures::stream::iter(events))
    }

    #[tokio::test]
    async fn should_merge_split_usage_events_when_collecting_stream() {
        // Given a stream carrying an input-only Usage event (Anthropic
        // message_start, with cache buckets) followed by an output-only
        // Usage event (Anthropic message_delta)
        let stream = stream_of(vec![
            LlmEvent::Usage {
                input_tokens: 15,
                output_tokens: 0,
                cache_read_tokens: 200,
                cache_creation_tokens: 40,
            },
            LlmEvent::Usage {
                input_tokens: 0,
                output_tokens: 5,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
            },
        ]);

        // When the stream is collected
        let collected = collect_stream_events(stream, None).await;

        // Then all four buckets survive the merge instead of the second
        // event wiping the first's input-side accounting
        assert_eq!(collected.usage.input_tokens, 15);
        assert_eq!(collected.usage.output_tokens, 5);
        assert_eq!(collected.usage.cache_read_tokens, 200);
        assert_eq!(collected.usage.cache_creation_tokens, 40);
    }

    #[tokio::test]
    async fn should_keep_latest_nonzero_values_when_usage_events_repeat() {
        // Given a stream whose Usage events repeat cumulative values with a
        // growing output count (Google's per-chunk usageMetadata pattern)
        let stream = stream_of(vec![
            LlmEvent::Usage {
                input_tokens: 10,
                output_tokens: 2,
                cache_read_tokens: 4,
                cache_creation_tokens: 1,
            },
            LlmEvent::Usage {
                input_tokens: 10,
                output_tokens: 7,
                cache_read_tokens: 4,
                cache_creation_tokens: 1,
            },
        ]);

        // When the stream is collected
        let collected = collect_stream_events(stream, None).await;

        // Then the latest non-zero value per field wins without
        // double-counting the repeated input-side buckets
        assert_eq!(collected.usage.input_tokens, 10);
        assert_eq!(collected.usage.output_tokens, 7);
        assert_eq!(collected.usage.cache_read_tokens, 4);
        assert_eq!(collected.usage.cache_creation_tokens, 1);
    }

    // -------------------------------------------------------------------------
    // The in-flight LLM heartbeat (stall-watchdog feed, the
    // session-fdf98268 incident fix): while the stream's FIRST event is
    // outstanding, a ticker beats `LiveEvent::LlmPending` through the
    // live callback — a provider whose first token takes minutes must
    // not read as a dead engine.
    // -------------------------------------------------------------------------

    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// Count `LiveEvent::LlmPending` beats seen by a live callback.
    fn pending_counter() -> (Arc<Mutex<u32>>, crate::LiveEventCallback) {
        let seen: Arc<Mutex<u32>> = Arc::new(Mutex::new(0));
        let seen_cb = Arc::clone(&seen);
        let callback: crate::LiveEventCallback = Arc::new(move |event| {
            if matches!(event, crate::LiveEvent::LlmPending { .. }) {
                *seen_cb.lock().unwrap() += 1;
            }
        });
        (seen, callback)
    }

    /// Given an LLM stream whose first event arrives after several
    /// heartbeat intervals, when the stream is collected with a live
    /// callback, then `LiveEvent::LlmPending` heartbeats fired while the
    /// first event was outstanding — pre-fix the wait produced no live
    /// events at all, so a provider with a long first-token latency
    /// starved the turn's stall-watchdog feed.
    #[tokio::test(start_paused = true)]
    async fn should_emit_llm_pending_heartbeats_while_the_first_event_is_outstanding() {
        let (seen, callback) = pending_counter();

        // A stream whose single (first) event arrives after 1.5s of
        // silence — the slow-provider shape. `stream::once` holds the
        // sleep inside the stream, so a dropped `next()` future (the
        // select's heartbeat arm) never loses state.
        let stream: LlmEventStream = Box::pin(futures::stream::once(async move {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            LlmEvent::Done {
                stop_reason: "completed".to_string(),
            }
        }));
        let _ = collect_stream_events(stream, Some(&callback)).await;

        // 1.5s of silence with heartbeats every 500ms → 2-3 beats (the
        // 1500ms tick and the event race at the boundary); >= 2 is the
        // robust assertion.
        let count = *seen.lock().unwrap();
        assert!(
            count >= 2,
            "the pending heartbeats must fire while the first event is outstanding, got {count}"
        );
    }

    /// Given a stream that yields events immediately (the normal case),
    /// when collected with a live callback, then no `LlmPending`
    /// heartbeats fire — deltas are their own liveness signal and the
    /// ticker never gets a chance.
    #[tokio::test(start_paused = true)]
    async fn should_not_emit_llm_pending_heartbeats_when_events_flow_immediately() {
        let (seen, callback) = pending_counter();

        let stream = stream_of(vec![
            LlmEvent::TextPart {
                text: "hello".into(),
            },
            LlmEvent::Done {
                stop_reason: "completed".into(),
            },
        ]);
        let _ = collect_stream_events(stream, Some(&callback)).await;

        assert!(
            *seen.lock().unwrap() == 0,
            "no pending heartbeats must fire when the stream answers immediately"
        );
    }

    /// Given a stream that delivers its first event promptly and THEN
    /// goes silent mid-stream (a stalled connection after the first
    /// token), when collected with a live callback, then no
    /// `LlmPending` heartbeats fire during the mid-stream silence —
    /// the heartbeat guards only the first-event wait; a dead
    /// connection mid-stream must stay detectable by the watchdog.
    #[tokio::test(start_paused = true)]
    async fn should_not_emit_llm_pending_heartbeats_during_mid_stream_silence() {
        let (seen, callback) = pending_counter();

        // First event immediate, then 2s of silence before the Done —
        // longer than two heartbeat intervals, but after first event.
        let stream: LlmEventStream = Box::pin(
            futures::stream::iter(vec![LlmEvent::TextPart {
                text: "first".into(),
            }])
            .chain(futures::stream::once(async move {
                tokio::time::sleep(Duration::from_millis(2000)).await;
                LlmEvent::Done {
                    stop_reason: "completed".to_string(),
                }
            })),
        );
        let _ = collect_stream_events(stream, Some(&callback)).await;

        assert!(
            *seen.lock().unwrap() == 0,
            "mid-stream silence must not be masked by pending heartbeats"
        );
    }
}
