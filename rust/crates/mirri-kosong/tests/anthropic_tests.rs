use mirri_kosong::{AnthropicProvider, ChatProvider, StreamEvent};
use serde_json::json;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::*};

/// Verify that consecutive `text_delta` events are surfaced as separate
/// [`StreamEvent::TextDelta`]s whose concatenation reconstructs the full
/// assistant message.
#[tokio::test]
async fn should_emit_text_deltas_when_streaming_text_only_response() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(
                    "event: message_start\n\
                     data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-sonnet-4-20250514\",\"usage\":{\"input_tokens\":10}}}\n\n\
                     event: content_block_start\n\
                     data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
                     event: content_block_delta\n\
                     data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n\
                     event: content_block_delta\n\
                     data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\n\
                     event: content_block_stop\n\
                     data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
                     event: message_delta\n\
                     data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":10}}\n\n\
                     event: message_stop\n\
                     data: {\"type\":\"message_stop\"}\n\n",
                ),
        )
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new("test-api-key".to_string(), format!("{}/v1", server.uri()));

    let mut stream = provider
        .chat(json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .await
        .expect("chat should succeed");

    let mut text = String::new();
    while let Some(event) = stream.next_event().await {
        match event.expect("event should parse") {
            StreamEvent::TextDelta { text: delta } => text.push_str(&delta),
            StreamEvent::Done { .. } => break,
            _ => {}
        }
    }

    assert_eq!(text, "Hello world");
}

/// Verify that a `tool_use` content block — declared in
/// `content_block_start` and assembled from `input_json_delta` fragments — is
/// emitted as a complete [`StreamEvent::ToolCall`] on `content_block_stop`.
#[tokio::test]
async fn should_emit_tool_call_when_streaming_tool_use_response() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(
                    "event: content_block_start\n\
                     data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"Read\",\"input\":{}}}\n\n\
                     event: content_block_delta\n\
                     data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"/tmp\\\"}\"}}\n\n\
                     event: content_block_stop\n\
                     data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
                     event: message_delta\n\
                     data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":15}}\n\n\
                     event: message_stop\n\
                     data: {\"type\":\"message_stop\"}\n\n",
                ),
        )
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new("test-api-key".to_string(), format!("{}/v1", server.uri()));

    let mut stream = provider
        .chat(json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "read /tmp"}],
        }))
        .await
        .expect("chat should succeed");

    let mut tool_calls = vec![];
    while let Some(event) = stream.next_event().await {
        match event.expect("event should parse") {
            StreamEvent::ToolCall {
                tool,
                args,
                call_id,
            } => tool_calls.push((tool, args, call_id)),
            StreamEvent::Done { .. } => break,
            _ => {}
        }
    }

    assert_eq!(tool_calls.len(), 1, "expected exactly one tool call");
    assert_eq!(tool_calls[0].0, "Read");
    assert_eq!(tool_calls[0].1["path"], "/tmp");
    assert_eq!(tool_calls[0].2, "toolu_1");
}

/// Given a tool-use streaming response whose `partial_json` fragments do not
/// accumulate into valid JSON (a truncated stream), When the stream is
/// consumed, Then the flushed `ToolCall` must carry an empty JSON object
/// instead of `null` — `null` arguments are rejected by servers when the
/// call is replayed in later turns.
#[tokio::test]
async fn should_flush_empty_object_args_when_partial_json_is_invalid() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(
                    "event: content_block_start\n\
                     data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_bad\",\"name\":\"Edit\",\"input\":{}}}\n\n\
                     event: content_block_delta\n\
                     data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"/tm\"}}\n\n\
                     event: content_block_stop\n\
                     data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
                     event: message_delta\n\
                     data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":15}}\n\n\
                     event: message_stop\n\
                     data: {\"type\":\"message_stop\"}\n\n",
                ),
        )
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new("test-api-key".to_string(), format!("{}/v1", server.uri()));

    let mut stream = provider
        .chat(json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "edit a file"}],
        }))
        .await
        .expect("chat should succeed");

    let mut tool_calls = vec![];
    while let Some(event) = stream.next_event().await {
        match event.expect("event should parse") {
            StreamEvent::ToolCall {
                tool,
                args,
                call_id,
            } => tool_calls.push((tool, args, call_id)),
            StreamEvent::Done { .. } => break,
            _ => {}
        }
    }

    assert_eq!(tool_calls.len(), 1, "expected exactly one tool call");
    assert_eq!(tool_calls[0].0, "Edit");
    assert_eq!(
        tool_calls[0].1, json!({}),
        "invalid accumulated partial_json must degrade to an empty JSON object, never null"
    );
    assert_eq!(tool_calls[0].2, "toolu_bad");
}

/// Given a tool_use block whose accumulated `partial_json` deltas form
/// valid JSON that is not an object (e.g. an array), When the stream is
/// consumed, Then the flushed `ToolCall` must carry an empty JSON object —
/// non-object arguments are rejected by strict servers when the call is
/// replayed, mirroring the OpenAI dispatchers' test matrix.
#[tokio::test]
async fn should_flush_empty_object_args_when_partial_json_is_non_object_json() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(
                    "event: content_block_start\n\
                     data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_arr\",\"name\":\"Edit\",\"input\":{}}}\n\n\
                     event: content_block_delta\n\
                     data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"[1\"}}\n\n\
                     event: content_block_delta\n\
                     data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\",2]\"}}\n\n\
                     event: content_block_stop\n\
                     data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
                     event: message_delta\n\
                     data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":15}}\n\n\
                     event: message_stop\n\
                     data: {\"type\":\"message_stop\"}\n\n",
                ),
        )
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new("test-api-key".to_string(), format!("{}/v1", server.uri()));

    let mut stream = provider
        .chat(json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "edit a file"}],
        }))
        .await
        .expect("chat should succeed");

    let mut tool_calls = vec![];
    while let Some(event) = stream.next_event().await {
        match event.expect("event should parse") {
            StreamEvent::ToolCall {
                tool,
                args,
                call_id,
            } => tool_calls.push((tool, args, call_id)),
            StreamEvent::Done { .. } => break,
            _ => {}
        }
    }

    assert_eq!(tool_calls.len(), 1, "expected exactly one tool call");
    assert_eq!(tool_calls[0].0, "Edit");
    assert_eq!(
        tool_calls[0].1, json!({}),
        "non-object accumulated partial_json must degrade to an empty JSON object"
    );
    assert_eq!(tool_calls[0].2, "toolu_arr");
}

/// Verify a response that interleaves a text block and a tool block: both are
/// emitted in order with correct indices.
#[tokio::test]
async fn should_emit_text_and_tool_call_in_order_when_streaming_mixed_response() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(
                    "event: content_block_start\n\
                     data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
                     event: content_block_delta\n\
                     data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Let me read that.\"}}\n\n\
                     event: content_block_stop\n\
                     data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
                     event: content_block_start\n\
                     data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_2\",\"name\":\"Read\",\"input\":{}}}\n\n\
                     event: content_block_delta\n\
                     data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"/tmp/test\\\"}\"}}\n\n\
                     event: content_block_stop\n\
                     data: {\"type\":\"content_block_stop\",\"index\":1}\n\n\
                     event: message_delta\n\
                     data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":25}}\n\n\
                     event: message_stop\n\
                     data: {\"type\":\"message_stop\"}\n\n",
                ),
        )
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new("test-api-key".to_string(), format!("{}/v1", server.uri()));

    let mut stream = provider
        .chat(json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "read /tmp/test"}],
        }))
        .await
        .expect("chat should succeed");

    let mut text = String::new();
    let mut tool_calls = vec![];
    while let Some(event) = stream.next_event().await {
        match event.expect("event should parse") {
            StreamEvent::TextDelta { text: delta } => text.push_str(&delta),
            StreamEvent::ToolCall {
                tool,
                args,
                call_id,
            } => tool_calls.push((tool, args, call_id)),
            StreamEvent::Done { .. } => break,
            _ => {}
        }
    }

    assert_eq!(text, "Let me read that.");
    assert_eq!(tool_calls.len(), 1, "expected exactly one tool call");
    assert_eq!(tool_calls[0].0, "Read");
    assert_eq!(tool_calls[0].1["path"], "/tmp/test");
}

/// Verify that a non-success HTTP status is surfaced as an error from
/// `chat()` (before any streaming begins).
#[tokio::test]
async fn should_return_error_when_api_returns_error_status() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(401).set_body_string(
                r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#,
            ),
        )
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new("bad-key".to_string(), format!("{}/v1", server.uri()));

    let result = provider
        .chat(json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .await;

    let err = match result {
        Ok(_) => panic!("expected an error for 401 response, got a stream"),
        Err(e) => e.to_string(),
    };
    assert!(
        err.contains("401") || err.contains("authentication_error"),
        "error should mention the status or error type, got: {err}"
    );
}

/// Verify that input/output token usage is correctly reported across
/// `message_start` and `message_delta`.
#[tokio::test]
async fn should_emit_usage_events_when_response_includes_token_usage() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(
                    "event: message_start\n\
                     data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-sonnet-4-20250514\",\"usage\":{\"input_tokens\":15}}}\n\n\
                     event: content_block_start\n\
                     data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
                     event: content_block_delta\n\
                     data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n\
                     event: content_block_stop\n\
                     data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
                     event: message_delta\n\
                     data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n\
                     event: message_stop\n\
                     data: {\"type\":\"message_stop\"}\n\n",
                ),
        )
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new("test-api-key".to_string(), format!("{}/v1", server.uri()));

    let mut stream = provider
        .chat(json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .await
        .expect("chat should succeed");

    let mut input_tokens = 0u64;
    let mut output_tokens = 0u64;
    while let Some(event) = stream.next_event().await {
        match event.expect("event should parse") {
            StreamEvent::Usage {
                input_tokens: it,
                output_tokens: ot,
                ..
            } => {
                input_tokens += it;
                output_tokens += ot;
            }
            StreamEvent::Done { .. } => break,
            _ => {}
        }
    }

    assert_eq!(input_tokens, 15);
    assert_eq!(output_tokens, 5);
}

/// Given an Anthropic SSE stream whose `message_start` usage carries
/// `cache_read_input_tokens` and `cache_creation_input_tokens`, When the
/// stream is consumed, Then the `Usage` event should surface both cache
/// buckets alongside the non-cached `input_tokens`.
#[tokio::test]
async fn should_carry_cache_buckets_when_message_start_reports_cache_usage() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(
                    "event: message_start\n\
                     data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-sonnet-4-20250514\",\"usage\":{\"input_tokens\":15,\"cache_read_input_tokens\":200,\"cache_creation_input_tokens\":40}}}\n\n\
                     event: message_delta\n\
                     data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n\
                     event: message_stop\n\
                     data: {\"type\":\"message_stop\"}\n\n",
                ),
        )
        .mount(&server)
        .await;

    let provider = AnthropicProvider::new("test-api-key".to_string(), format!("{}/v1", server.uri()));

    let mut stream = provider
        .chat(json!({
            "model": "claude-sonnet-4-20250514",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "hi"}],
        }))
        .await
        .expect("chat should succeed");

    let mut saw_cache_usage = false;
    while let Some(event) = stream.next_event().await {
        match event.expect("event should parse") {
            StreamEvent::Usage {
                input_tokens,
                cache_read_tokens,
                cache_creation_tokens,
                ..
            } if input_tokens > 0 || cache_read_tokens > 0 || cache_creation_tokens > 0 => {
                assert_eq!(input_tokens, 15, "non-cached input from message_start");
                assert_eq!(cache_read_tokens, 200, "cache read bucket");
                assert_eq!(cache_creation_tokens, 40, "cache creation bucket");
                saw_cache_usage = true;
            }
            StreamEvent::Done { .. } => break,
            _ => {}
        }
    }

    assert!(saw_cache_usage, "expected a Usage event with cache buckets");
}
