//! Unit tests for the extended [`MockLlm`]: per-character token streaming
//! with pacing, never-respond mode, and mid-stream cancellation observation.
//!
//! These exercise the `Llm` contract directly (the same stream shape the
//! engine loop consumes); the server-level e2e tests in `mirri-server` drive
//! the same behaviors through a full `message.send` turn.

use std::time::Duration;

use futures::StreamExt;
use mirri_core::cancel::CancelToken;
use mirri_core::error::EngineResult;
use mirri_core::llm::{Llm, LlmChatParams, LlmEvent, LlmEventStream};
use mirri_test_harness::{MockLlm, MockResponse};

/// Drain an event stream to completion (used with streams that do end).
async fn drain_stream(stream: LlmEventStream) -> Vec<LlmEvent> {
    let mut events = Vec::new();
    let mut stream = stream;
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    events
}

/// A `chat()` call with a fresh, never-cancelled token and no tools.
fn chat_params() -> LlmChatParams {
    LlmChatParams {
        messages: Vec::new(),
        tools: Vec::new(),
        cancel: CancelToken::new(),
        request_log_fields: None,
        cache_root_id: None,
    }
}

#[tokio::test]
async fn should_emit_one_text_delta_per_character_when_response_streams() {
    // Given a mock with a streaming response paced at 10 ms per character
    let mock = MockLlm::new();
    mock.push_response(MockResponse::streaming("abc", Duration::from_millis(10)));

    // When the chat stream is fully consumed
    let events = drain_stream(mock.chat(chat_params()).expect("chat succeeds")).await;

    // Then one delta per character arrived, in order, before the block-level part
    let deltas: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            LlmEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, vec!["a", "b", "c"], "one delta per character");
    assert_eq!(mock.delta_yield_count(), 3);

    // ...the full text still arrives as the ordered block-level part...
    assert!(
        events
            .iter()
            .any(|event| matches!(event, LlmEvent::TextPart { text } if text == "abc")),
        "the full text part follows the deltas: {events:?}"
    );
    // ...and the stream ends with the conventional done/usage tail
    assert!(
        events
            .iter()
            .any(|event| matches!(event, LlmEvent::Usage { .. }))
    );
    assert!(
        events.iter().any(
            |event| matches!(event, LlmEvent::Done { stop_reason } if stop_reason == "completed")
        ),
        "Done carries the completed stop reason: {events:?}"
    );

    // And the deltas were paced: 3 characters × 10 ms ⇒ ≥ 20 ms of spread
    let spread = mock
        .delta_spread()
        .expect("at least two deltas yield a measurable spread");
    assert!(
        spread >= Duration::from_millis(20),
        "streaming paced the deltas: {spread:?}"
    );
    assert!(!mock.cancel_seen(), "no cancellation was involved");
}

#[tokio::test]
async fn should_never_yield_when_mock_hangs() {
    // Given a mock switched into never-respond mode
    let mock = MockLlm::new();
    mock.hang_forever();

    // When the chat stream is polled
    let mut stream = mock.chat(chat_params()).expect("chat succeeds");

    // Then it never yields (the caller's deadline must end the turn)
    let outcome = tokio::time::timeout(Duration::from_millis(50), stream.next()).await;
    assert!(
        outcome.is_err(),
        "the never-responding stream must stay pending"
    );
    assert_eq!(mock.chat_call_count(), 1, "the mock was still consulted");
    assert!(mock.hangs());
    assert_eq!(mock.delta_yield_count(), 0, "no deltas were ever produced");
}

#[tokio::test]
async fn should_stop_streaming_early_when_cancel_token_fires() {
    // Given a long streaming response and a cancellable chat call
    let mock = MockLlm::new();
    mock.push_response(MockResponse::streaming(
        "abcdefghij",
        Duration::from_millis(5),
    ));
    let cancel = CancelToken::new();
    let params = LlmChatParams {
        messages: Vec::new(),
        tools: Vec::new(),
        cancel: cancel.clone(),
        request_log_fields: None,
        cache_root_id: None,
    };
    let stream = mock.chat(params).expect("chat succeeds");
    let drain = tokio::spawn(drain_stream(stream));

    // When the caller cancels while deltas are still flowing
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(
        mock.delta_yield_count() >= 2,
        "deltas started flowing before the cancellation"
    );
    cancel.cancel();
    let events = drain.await.expect("drain task completes");

    // Then the mock observed the cancellation and stopped mid-stream
    assert!(mock.cancel_seen(), "the stream observed the cancel token");
    assert!(
        mock.delta_yield_count() < 10,
        "the stream must not outlive the cancellation"
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, LlmEvent::TextPart { text } if text == "abcdefghij")),
        "a cancelled stream must not deliver the final block-level part"
    );
}

/// A plain (non-streaming) response still yields a single delta burst and
/// the full block-level tail — the legacy behavior harness tests rely on.
#[tokio::test]
async fn should_emit_single_unpaced_delta_when_no_token_delay_configured() {
    let mock = MockLlm::new();
    mock.push_text_response("hi");
    let events = drain_stream(mock.chat(chat_params()).expect("chat succeeds")).await;
    let deltas: Vec<&str> = events
        .iter()
        .filter_map(|event| match event {
            LlmEvent::TextDelta { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, vec!["h", "i"]);
    assert!(
        mock.delta_spread()
            .is_some_and(|s| s < Duration::from_millis(50))
    );
}

/// `chat()` on an empty queue fails with a provider error (the engine turns
/// that into a failed turn instead of hanging).
#[tokio::test]
async fn should_error_when_no_response_is_queued() {
    let mock = MockLlm::new();
    let outcome: EngineResult<_> = mock.chat(chat_params());
    assert!(outcome.is_err(), "an empty queue must fail the chat call");
}
