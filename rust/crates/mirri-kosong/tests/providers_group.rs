// layer: T1
// Domain group binary consolidated from per-file test binaries
// per 16-test-execution-strategy.md §4.4 (pure file moves).
mod anthropic_tests {
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

}

mod google_tests {
use mirri_kosong::{ChatProvider, GoogleGenAIProvider, StreamEvent};
use serde_json::json;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::*};

/// Helper: mount a mock SSE response on `server` matching the Google GenAI
/// streaming endpoint and return the provider.
async fn mount_sse(server: &MockServer, body: &str) {
    Mock::given(method("POST"))
        .and(path_regex(r"/v1beta/models/.+:generateContentStream"))
        .and(query_param("key", "test-key"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body.to_string()),
        )
        .mount(server)
        .await;
}

/// Helper: collect all events from a stream until `None`.
async fn collect_events(stream: &mut mirri_kosong::SseStream) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    while let Some(event) = stream.next_event().await {
        events.push(event.expect("event should parse"));
    }
    events
}

/// Given a text-only streaming response, When the stream is consumed, Then
/// separate `TextDelta` events should be emitted for each content chunk
/// followed by a `Done` with stop reason "completed" and a `Usage` event.
#[tokio::test]
async fn should_emit_text_deltas_when_streaming_text_only_response() {
    let server = MockServer::start().await;

    mount_sse(
        &server,
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hello\"}],\"role\":\"model\"}}]}\n\n\
         data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\" world\"}],\"role\":\"model\"},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":5}}\n\n",
    )
    .await;

    let provider = GoogleGenAIProvider::new("test-key".to_string(), format!("{}/v1beta", server.uri()));

    let mut stream = provider
        .chat(json!({
            "model": "gemini-2.0-flash",
            "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
        }))
        .await
        .expect("chat should succeed");

    let events = collect_events(&mut stream).await;

    // Expect: TextDelta("Hello"), TextDelta(" world"), Done("completed"),
    //         Usage(input=10, output=5)
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Hello world");

    let done = events.iter().find_map(|e| match e {
        StreamEvent::Done { stop_reason } => Some(stop_reason.clone()),
        _ => None,
    });
    assert_eq!(done.as_deref(), Some("completed"));

    let usage = events.iter().find_map(|e| match e {
        StreamEvent::Usage {
            input_tokens,
            output_tokens,
            ..
        } => Some((*input_tokens, *output_tokens)),
        _ => None,
    });
    assert_eq!(usage, Some((10, 5)));
}

/// Given a function-call streaming response, When the stream is consumed,
/// Then a complete `ToolCall` should be emitted with the pre-parsed args
/// object (not a JSON string), followed by `Done` with stop reason
/// "completed".
#[tokio::test]
async fn should_emit_tool_call_when_streaming_function_call_response() {
    let server = MockServer::start().await;

    mount_sse(
        &server,
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"get_weather\",\"args\":{\"city\":\"SF\"}}}],\"role\":\"model\"},\"finishReason\":\"STOP\"}]}\n\n",
    )
    .await;

    let provider = GoogleGenAIProvider::new("test-key".to_string(), format!("{}/v1beta", server.uri()));

    let mut stream = provider
        .chat(json!({
            "model": "gemini-2.0-flash",
            "contents": [{"role": "user", "parts": [{"text": "weather in SF"}]}],
        }))
        .await
        .expect("chat should succeed");

    let events = collect_events(&mut stream).await;

    let tool_calls: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCall {
                tool,
                args,
                call_id,
            } => Some((tool.clone(), args.clone(), call_id.clone())),
            _ => None,
        })
        .collect();

    assert_eq!(tool_calls.len(), 1, "expected exactly one tool call");
    assert_eq!(tool_calls[0].0, "get_weather");
    // Google's args are already a parsed JSON object, not a string.
    assert_eq!(tool_calls[0].1["city"], "SF");
    // Google does not provide a call_id — the dispatcher synthesizes a
    // unique one so id-keyed pairing survives parallel tool calls.
    assert!(
        !tool_calls[0].2.is_empty(),
        "expected a synthesized call id"
    );

    let done = events.iter().find_map(|e| match e {
        StreamEvent::Done { stop_reason } => Some(stop_reason.clone()),
        _ => None,
    });
    assert_eq!(done.as_deref(), Some("completed"));
}

/// Given a function-call response whose `args` is not a JSON object (e.g.
/// `null` from a misbehaving proxy), When the stream is consumed, Then the
/// `ToolCall` must carry an empty JSON object — non-object arguments are
/// rejected by servers when the call is replayed in later turns.
#[tokio::test]
async fn should_flush_empty_object_args_when_function_call_args_is_not_an_object() {
    let server = MockServer::start().await;

    mount_sse(
        &server,
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"Edit\",\"args\":null}}],\"role\":\"model\"},\"finishReason\":\"STOP\"}]}\n\n",
    )
    .await;

    let provider = GoogleGenAIProvider::new("test-key".to_string(), format!("{}/v1beta", server.uri()));

    let mut stream = provider
        .chat(json!({
            "model": "gemini-2.0-flash",
            "contents": [{"role": "user", "parts": [{"text": "edit a file"}]}],
        }))
        .await
        .expect("chat should succeed");

    let events = collect_events(&mut stream).await;

    let tool_calls: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCall {
                tool,
                args,
                call_id,
            } => Some((tool.clone(), args.clone(), call_id.clone())),
            _ => None,
        })
        .collect();

    assert_eq!(tool_calls.len(), 1, "expected exactly one tool call");
    assert_eq!(tool_calls[0].0, "Edit");
    assert_eq!(
        tool_calls[0].1, json!({}),
        "non-object args must degrade to an empty JSON object"
    );
}

/// Given a multi-part response containing both text and a function call,
/// When the stream is consumed, Then a `TextDelta` should be emitted for the
/// text part and a `ToolCall` for the function call part, followed by
/// `Done`.
#[tokio::test]
async fn should_emit_text_and_tool_call_when_streaming_multi_part_response() {
    let server = MockServer::start().await;

    mount_sse(
        &server,
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Let me check the weather.\"},{\"functionCall\":{\"name\":\"get_weather\",\"args\":{\"city\":\"SF\"}}}],\"role\":\"model\"},\"finishReason\":\"STOP\"}]}\n\n",
    )
    .await;

    let provider = GoogleGenAIProvider::new("test-key".to_string(), format!("{}/v1beta", server.uri()));

    let mut stream = provider
        .chat(json!({
            "model": "gemini-2.0-flash",
            "contents": [{"role": "user", "parts": [{"text": "weather in SF"}]}],
        }))
        .await
        .expect("chat should succeed");

    let events = collect_events(&mut stream).await;

    // Verify text delta was emitted.
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Let me check the weather.");

    // Verify tool call was emitted.
    let tool_calls: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCall {
                tool,
                args,
                call_id,
            } => Some((tool.clone(), args.clone(), call_id.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(tool_calls.len(), 1, "expected exactly one tool call");
    assert_eq!(tool_calls[0].0, "get_weather");
    assert_eq!(tool_calls[0].1["city"], "SF");

    let done = events.iter().find_map(|e| match e {
        StreamEvent::Done { stop_reason } => Some(stop_reason.clone()),
        _ => None,
    });
    assert_eq!(done.as_deref(), Some("completed"));
}

/// Given a streaming response that includes `thought: true` parts, When the
/// stream is consumed, Then `ThinkDelta` events should be emitted for the
/// thought content (not `TextDelta`), followed by normal `TextDelta` for the
/// answer.
#[tokio::test]
async fn should_emit_think_delta_when_thought_content_is_present() {
    let server = MockServer::start().await;

    mount_sse(
        &server,
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"thought\":true,\"text\":\"Thinking about the answer...\"}],\"role\":\"model\"}}]}\n\n\
         data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"The answer is 42.\"}],\"role\":\"model\"},\"finishReason\":\"STOP\"}]}\n\n",
    )
    .await;

    let provider = GoogleGenAIProvider::new("test-key".to_string(), format!("{}/v1beta", server.uri()));

    let mut stream = provider
        .chat(json!({
            "model": "gemini-2.0-flash",
            "contents": [{"role": "user", "parts": [{"text": "what is 40+2"}]}],
        }))
        .await
        .expect("chat should succeed");

    let events = collect_events(&mut stream).await;

    // Thought text should be emitted as ThinkDelta, not TextDelta.
    let thinking: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ThinkDelta { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(thinking, "Thinking about the answer...");

    // Answer text should be emitted as TextDelta.
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "The answer is 42.");

    let done = events.iter().find_map(|e| match e {
        StreamEvent::Done { stop_reason } => Some(stop_reason.clone()),
        _ => None,
    });
    assert_eq!(done.as_deref(), Some("completed"));
}

/// Given a streaming response with `MAX_TOKENS` finish reason, When the
/// stream is consumed, Then `Done` should carry stop reason "truncated".
#[tokio::test]
async fn should_emit_truncated_stop_reason_when_finish_reason_is_max_tokens() {
    let server = MockServer::start().await;

    mount_sse(
        &server,
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"partial...\"}],\"role\":\"model\"},\"finishReason\":\"MAX_TOKENS\"}]}\n\n",
    )
    .await;

    let provider = GoogleGenAIProvider::new("test-key".to_string(), format!("{}/v1beta", server.uri()));

    let mut stream = provider
        .chat(json!({
            "model": "gemini-2.0-flash",
            "contents": [{"role": "user", "parts": [{"text": "long story"}]}],
        }))
        .await
        .expect("chat should succeed");

    let events = collect_events(&mut stream).await;

    let text: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "partial...");

    let done = events.iter().find_map(|e| match e {
        StreamEvent::Done { stop_reason } => Some(stop_reason.clone()),
        _ => None,
    });
    assert_eq!(done.as_deref(), Some("truncated"));
}

/// Given a response with `usageMetadata` that includes
/// `cachedContentTokenCount`, When the stream is consumed, Then the `Usage`
/// event should report `input_tokens` as `promptTokenCount -
/// cachedContentTokenCount` and preserve the cached count in the cache-read
/// bucket.
#[tokio::test]
async fn should_emit_usage_with_cached_token_subtraction_when_response_includes_cached_tokens() {
    let server = MockServer::start().await;

    mount_sse(
        &server,
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hi\"}],\"role\":\"model\"},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":100,\"cachedContentTokenCount\":30,\"candidatesTokenCount\":10}}\n\n",
    )
    .await;

    let provider = GoogleGenAIProvider::new("test-key".to_string(), format!("{}/v1beta", server.uri()));

    let mut stream = provider
        .chat(json!({
            "model": "gemini-2.0-flash",
            "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
        }))
        .await
        .expect("chat should succeed");

    let events = collect_events(&mut stream).await;

    let usage_events: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::Usage {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_creation_tokens,
            } => Some((
                *input_tokens,
                *output_tokens,
                *cache_read_tokens,
                *cache_creation_tokens,
            )),
            _ => None,
        })
        .collect();

    assert_eq!(usage_events.len(), 1, "expected exactly one Usage event");
    // promptTokenCount(100) - cachedContentTokenCount(30) = 70 non-cached input
    assert_eq!(
        usage_events[0].0, 70,
        "input_tokens should subtract cached tokens"
    );
    assert_eq!(
        usage_events[0].1, 10,
        "output_tokens should match candidatesTokenCount"
    );
    // Then the subtracted cached tokens are preserved in the cache-read bucket
    assert_eq!(
        usage_events[0].2, 30,
        "cache_read_tokens should carry cachedContentTokenCount"
    );
    assert_eq!(
        usage_events[0].3, 0,
        "cache_creation_tokens should stay zero for Google"
    );
}

/// Given a non-success HTTP status, When `chat()` is called, Then an error
/// should be returned (before any streaming begins).
#[tokio::test]
async fn should_return_error_when_api_returns_non_success_status() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path_regex(r"/v1beta/models/.+:generateContentStream"))
        .respond_with(
            ResponseTemplate::new(403)
                .set_body_string(r#"{"error":{"message":"API key not valid"}}"#),
        )
        .mount(&server)
        .await;

    let provider = GoogleGenAIProvider::new("bad-key".to_string(), format!("{}/v1beta", server.uri()));

    let result = provider
        .chat(json!({
            "model": "gemini-2.0-flash",
            "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
        }))
        .await;

    let err = match result {
        Ok(_) => panic!("expected an error for 403 response, got a stream"),
        Err(e) => e.to_string(),
    };
    assert!(
        err.contains("403"),
        "error should mention the status code, got: {err}"
    );
}

}

mod openai_responses_tests {
use mirri_kosong::{ChatProvider, OpenAIResponsesProvider, StreamEvent};
use serde_json::json;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::*};

/// Helper: mount a mock SSE response on `server` and return the provider.
async fn mount_sse(server: &MockServer, body: &str) {
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body.to_string()),
        )
        .mount(server)
        .await;
}

/// Helper: collect all events from a stream until `None`.
async fn collect_events(stream: &mut mirri_kosong::SseStream) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    while let Some(event) = stream.next_event().await {
        events.push(event.expect("event should parse"));
    }
    events
}

/// Given a text-only streaming Responses API response, When the stream is
/// consumed, Then separate `TextDelta` events should be emitted for each
/// content chunk followed by a `Done` with stop reason "completed" and a
/// `Usage` event.
#[tokio::test]
async fn should_emit_text_deltas_when_streaming_text_only_response() {
    let server = MockServer::start().await;

    mount_sse(
        &server,
        "event: response.created\n\
         data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"status\":\"in_progress\"}}\n\n\
         event: response.output_text.delta\n\
         data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello\"}\n\n\
         event: response.output_text.delta\n\
         data: {\"type\":\"response.output_text.delta\",\"delta\":\" world\"}\n\n\
         event: response.completed\n\
         data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\n",
    )
    .await;

    let provider = OpenAIResponsesProvider::new("test-key".to_string(), format!("{}/v1", server.uri()));

    let mut stream = provider
        .chat(json!({
            "model": "gpt-4o",
            "input": "hi",
            "stream": true,
        }))
        .await
        .expect("chat should succeed");

    let events = collect_events(&mut stream).await;

    // Expect: TextDelta("Hello"), TextDelta(" world"), Done("completed"),
    //         Usage(10, 5)
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Hello world");

    let done = events.iter().find_map(|e| match e {
        StreamEvent::Done { stop_reason } => Some(stop_reason.clone()),
        _ => None,
    });
    assert_eq!(done.as_deref(), Some("completed"));

    let usage = events.iter().find_map(|e| match e {
        StreamEvent::Usage {
            input_tokens,
            output_tokens,
            ..
        } => Some((*input_tokens, *output_tokens)),
        _ => None,
    });
    assert_eq!(usage, Some((10, 5)));
}

/// Given a function-call streaming Responses API response, When the stream is
/// consumed, Then a `ToolCallPart` should be emitted for the argument delta
/// followed by a complete `ToolCall` with the assembled arguments, and a
/// `Done` with stop reason "completed".
#[tokio::test]
async fn should_emit_tool_call_when_streaming_function_call_response() {
    let server = MockServer::start().await;

    mount_sse(
        &server,
        "event: response.output_item.added\n\
         data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"name\":\"get_weather\",\"call_id\":\"call_abc\"}}\n\n\
         event: response.function_call_arguments.delta\n\
         data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"delta\":\"{\\\"city\\\":\\\"SF\\\"}\"}\n\n\
         event: response.completed\n\
         data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[{\"type\":\"function_call\",\"id\":\"fc_1\",\"name\":\"get_weather\",\"call_id\":\"call_abc\",\"arguments\":\"{\\\"city\\\":\\\"SF\\\"}\"}]}}\n\n",
    )
    .await;

    let provider = OpenAIResponsesProvider::new("test-key".to_string(), format!("{}/v1", server.uri()));

    let mut stream = provider
        .chat(json!({
            "model": "gpt-4o",
            "input": "weather in SF",
            "stream": true,
        }))
        .await
        .expect("chat should succeed");

    let events = collect_events(&mut stream).await;

    // Should have at least one ToolCallPart for the streaming delta.
    let parts: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCallPart { arguments_part, .. } => Some(arguments_part.clone()),
            _ => None,
        })
        .collect();
    assert!(!parts.is_empty(), "expected at least one ToolCallPart");

    let tool_calls: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCall {
                tool,
                args,
                call_id,
            } => Some((tool.clone(), args.clone(), call_id.clone())),
            _ => None,
        })
        .collect();

    assert_eq!(tool_calls.len(), 1, "expected exactly one tool call");
    assert_eq!(tool_calls[0].0, "get_weather");
    assert_eq!(tool_calls[0].1["city"], "SF");
    assert_eq!(tool_calls[0].2, "call_abc");

    let done = events.iter().find_map(|e| match e {
        StreamEvent::Done { stop_reason } => Some(stop_reason.clone()),
        _ => None,
    });
    assert_eq!(done.as_deref(), Some("completed"));
}

/// Given a function-call response whose authoritative `arguments` string is
/// valid JSON but not an object (e.g. an array), When the stream is consumed,
/// Then the flushed `ToolCall` must carry an empty JSON object — non-object
/// arguments are rejected by OpenAI-compatible servers
/// ("function.arguments must be a JSON object") when the call is replayed.
#[tokio::test]
async fn should_flush_empty_object_args_when_function_call_arguments_are_non_object_json() {
    let server = MockServer::start().await;

    mount_sse(
        &server,
        "event: response.output_item.added\n\
         data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"fc_arr\",\"name\":\"Edit\",\"call_id\":\"call_arr\"}}\n\n\
         event: response.function_call_arguments.delta\n\
         data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_arr\",\"delta\":\"[1\"}\n\n\
         event: response.function_call_arguments.delta\n\
         data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_arr\",\"delta\":\",2]\"}\n\n\
         event: response.completed\n\
         data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[{\"type\":\"function_call\",\"id\":\"fc_arr\",\"name\":\"Edit\",\"call_id\":\"call_arr\",\"arguments\":\"[1,2]\"}]}}\n\n",
    )
    .await;

    let provider = OpenAIResponsesProvider::new("test-key".to_string(), format!("{}/v1", server.uri()));

    let mut stream = provider
        .chat(json!({
            "model": "gpt-4o",
            "input": "edit a file",
            "stream": true,
        }))
        .await
        .expect("chat should succeed");

    let events = collect_events(&mut stream).await;

    let tool_calls: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCall {
                tool,
                args,
                call_id,
            } => Some((tool.clone(), args.clone(), call_id.clone())),
            _ => None,
        })
        .collect();

    assert_eq!(tool_calls.len(), 1, "expected exactly one tool call");
    assert_eq!(tool_calls[0].0, "Edit");
    assert_eq!(
        tool_calls[0].1, json!({}),
        "non-object arguments must degrade to an empty JSON object"
    );
    assert_eq!(tool_calls[0].2, "call_arr");
}

/// Given a function-call argument delta routed by `item_id` after the
/// `output_item.added` header, When the stream is consumed, Then the
/// `ToolCallPart` should carry the pending call's call id (not the item id)
/// and tool name, so the part's identity matches the eventual block-level
/// `ToolCall` exactly.
#[tokio::test]
async fn should_carry_the_pending_call_id_not_the_item_id_on_tool_call_parts() {
    let server = MockServer::start().await;

    mount_sse(
        &server,
        "event: response.output_item.added\n\
         data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"fc_9\",\"name\":\"get_weather\",\"call_id\":\"call_abc\"}}\n\n\
         event: response.function_call_arguments.delta\n\
         data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_9\",\"delta\":\"{\\\"city\\\":\\\"SF\\\"}\"}\n\n\
         event: response.completed\n\
         data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[{\"type\":\"function_call\",\"id\":\"fc_9\",\"name\":\"get_weather\",\"call_id\":\"call_abc\",\"arguments\":\"{\\\"city\\\":\\\"SF\\\"}\"}]}}\n\n",
    )
    .await;

    let provider = OpenAIResponsesProvider::new("test-key".to_string(), format!("{}/v1", server.uri()));

    let mut stream = provider
        .chat(json!({
            "model": "gpt-4o",
            "input": "weather in SF",
            "stream": true,
        }))
        .await
        .expect("chat should succeed");

    let events = collect_events(&mut stream).await;

    let parts: Vec<(Option<String>, Option<String>)> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCallPart {
                call_id, tool_name, ..
            } => Some((call_id.clone(), tool_name.clone())),
            _ => None,
        })
        .collect();

    assert_eq!(parts.len(), 1, "expected one ToolCallPart event");
    assert_eq!(parts[0].0.as_deref(), Some("call_abc"));
    assert_eq!(parts[0].1.as_deref(), Some("get_weather"));
}

/// Given an argument delta whose `item_id` has no registered pending call
/// (the delta arrived before `output_item.added`), When the stream is
/// consumed, Then the `ToolCallPart` should carry no call id or tool name —
/// its identity cannot be established yet.
#[tokio::test]
async fn should_leave_call_id_absent_on_parts_that_precede_their_item_header() {
    let server = MockServer::start().await;

    mount_sse(
        &server,
        "event: response.function_call_arguments.delta\n\
         data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_orphan\",\"delta\":\"{}\"}\n\n\
         event: response.completed\n\
         data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[]}}\n\n",
    )
    .await;

    let provider = OpenAIResponsesProvider::new("test-key".to_string(), format!("{}/v1", server.uri()));

    let mut stream = provider
        .chat(json!({
            "model": "gpt-4o",
            "input": "orphan delta",
            "stream": true,
        }))
        .await
        .expect("chat should succeed");

    let events = collect_events(&mut stream).await;

    let parts: Vec<(Option<String>, Option<String>)> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCallPart {
                call_id, tool_name, ..
            } => Some((call_id.clone(), tool_name.clone())),
            _ => None,
        })
        .collect();

    assert_eq!(parts.len(), 1, "expected one ToolCallPart event");
    assert!(
        parts[0].0.is_none() && parts[0].1.is_none(),
        "orphan part must not claim an identity: {parts:?}"
    );
}

/// Given a reasoning model that produces `response.reasoning.delta` events,
/// When the stream is consumed, Then `ThinkDelta` events should be emitted for
/// the reasoning fragments, followed by `TextDelta` for the answer, and a
/// `Done`.
#[tokio::test]
async fn should_emit_think_delta_when_reasoning_model_produces_reasoning() {
    let server = MockServer::start().await;

    mount_sse(
        &server,
        "event: response.reasoning.delta\n\
         data: {\"type\":\"response.reasoning.delta\",\"delta\":\"Let me think\"}\n\n\
         event: response.output_text.delta\n\
         data: {\"type\":\"response.output_text.delta\",\"delta\":\"42\"}\n\n\
         event: response.completed\n\
         data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\n",
    )
    .await;

    let provider = OpenAIResponsesProvider::new("test-key".to_string(), format!("{}/v1", server.uri()));

    let mut stream = provider
        .chat(json!({
            "model": "o3",
            "input": "what is 40+2",
            "stream": true,
        }))
        .await
        .expect("chat should succeed");

    let events = collect_events(&mut stream).await;

    let thinking: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ThinkDelta { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(thinking, "Let me think");

    let text: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "42");

    let done = events.iter().find_map(|e| match e {
        StreamEvent::Done { stop_reason } => Some(stop_reason.clone()),
        _ => None,
    });
    assert_eq!(done.as_deref(), Some("completed"));
}

/// Given an incomplete response with `max_output_tokens` reason, When the
/// stream is consumed, Then a `Done` event should be emitted with stop reason
/// "truncated".
#[tokio::test]
async fn should_emit_truncated_when_response_is_incomplete_with_max_output_tokens() {
    let server = MockServer::start().await;

    mount_sse(
        &server,
        "event: response.incomplete\n\
         data: {\"type\":\"response.incomplete\",\"response\":{\"id\":\"resp_1\",\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"}}}\n\n",
    )
    .await;

    let provider = OpenAIResponsesProvider::new("test-key".to_string(), format!("{}/v1", server.uri()));

    let mut stream = provider
        .chat(json!({
            "model": "gpt-4o",
            "input": "write a very long essay",
            "stream": true,
        }))
        .await
        .expect("chat should succeed");

    let events = collect_events(&mut stream).await;

    let done = events.iter().find_map(|e| match e {
        StreamEvent::Done { stop_reason } => Some(stop_reason.clone()),
        _ => None,
    });
    assert_eq!(done.as_deref(), Some("truncated"));
}

/// Given streaming argument deltas that differ from the final `arguments` in
/// `response.completed`, When the stream is consumed, Then a reconciliation
/// `ToolCallPart` should be emitted for the missing suffix, followed by a
/// complete `ToolCall` with the full arguments.
#[tokio::test]
async fn should_reconcile_arguments_when_streaming_delta_differs_from_final() {
    let server = MockServer::start().await;

    // The streaming delta only carries a partial argument string
    // "{\"city\":\"S" — the final arguments in response.completed are
    // "{\"city\":\"SF\"}", so the suffix "F\"}" should be reconciled.
    mount_sse(
        &server,
        "event: response.output_item.added\n\
         data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"name\":\"get_weather\",\"call_id\":\"call_abc\"}}\n\n\
         event: response.function_call_arguments.delta\n\
         data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"delta\":\"{\\\"city\\\":\\\"S\"}\n\n\
         event: response.completed\n\
         data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[{\"type\":\"function_call\",\"id\":\"fc_1\",\"name\":\"get_weather\",\"call_id\":\"call_abc\",\"arguments\":\"{\\\"city\\\":\\\"SF\\\"}\"}]}}\n\n",
    )
    .await;

    let provider = OpenAIResponsesProvider::new("test-key".to_string(), format!("{}/v1", server.uri()));

    let mut stream = provider
        .chat(json!({
            "model": "gpt-4o",
            "input": "weather in SF",
            "stream": true,
        }))
        .await
        .expect("chat should succeed");

    let events = collect_events(&mut stream).await;

    // Collect all ToolCallPart fragments — should be the streaming delta plus
    // the reconciled suffix.
    let parts: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCallPart { arguments_part, .. } => Some(arguments_part.clone()),
            _ => None,
        })
        .collect();

    assert_eq!(
        parts.len(),
        2,
        "expected two ToolCallPart events (delta + suffix)"
    );
    assert_eq!(parts[0], "{\"city\":\"S");
    assert_eq!(parts[1], "F\"}");

    // The complete ToolCall should have the full arguments.
    let tool_calls: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCall {
                tool,
                args,
                call_id,
            } => Some((tool.clone(), args.clone(), call_id.clone())),
            _ => None,
        })
        .collect();

    assert_eq!(
        tool_calls.len(),
        1,
        "expected exactly one complete tool call"
    );
    assert_eq!(tool_calls[0].0, "get_weather");
    assert_eq!(tool_calls[0].1["city"], "SF");
    assert_eq!(tool_calls[0].2, "call_abc");
}

/// Given a `response.completed` event that includes a `usage` object with
/// cached token details, When the stream is consumed, Then a `Usage` event
/// should be emitted with `input_tokens` subtracting `cached_tokens` and the
/// correct `output_tokens`.
#[tokio::test]
async fn should_emit_usage_when_completed_event_includes_usage() {
    let server = MockServer::start().await;

    mount_sse(
        &server,
        "event: response.output_text.delta\n\
         data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n\
         event: response.completed\n\
         data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"usage\":{\"input_tokens\":42,\"output_tokens\":7,\"input_tokens_details\":{\"cached_tokens\":2}}}}\n\n",
    )
    .await;

    let provider = OpenAIResponsesProvider::new("test-key".to_string(), format!("{}/v1", server.uri()));

    let mut stream = provider
        .chat(json!({
            "model": "gpt-4o",
            "input": "hi",
            "stream": true,
        }))
        .await
        .expect("chat should succeed");

    let events = collect_events(&mut stream).await;

    let usage_events: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::Usage {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_creation_tokens,
            } => Some((
                *input_tokens,
                *output_tokens,
                *cache_read_tokens,
                *cache_creation_tokens,
            )),
            _ => None,
        })
        .collect();

    assert_eq!(usage_events.len(), 1, "expected exactly one Usage event");
    // input_tokens(42) - cached_tokens(2) = 40 non-cached input tokens
    assert_eq!(usage_events[0].0, 40, "input_tokens should subtract cached");
    assert_eq!(usage_events[0].1, 7, "output_tokens should match");
    // Then the subtracted cached tokens are preserved in the cache-read bucket
    assert_eq!(
        usage_events[0].2, 2,
        "cache_read_tokens should carry cached"
    );
    assert_eq!(
        usage_events[0].3, 0,
        "no cache creation for OpenAI Responses"
    );
}

/// Given a non-success HTTP status, When `chat()` is called, Then an error
/// should be returned (before any streaming begins).
#[tokio::test]
async fn should_return_error_when_api_returns_non_success_status() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string(r#"{"error":{"message":"Invalid API key"}}"#),
        )
        .mount(&server)
        .await;

    let provider = OpenAIResponsesProvider::new("bad-key".to_string(), format!("{}/v1", server.uri()));

    let result = provider
        .chat(json!({
            "model": "gpt-4o",
            "input": "hi",
            "stream": true,
        }))
        .await;

    let err = match result {
        Ok(_) => panic!("expected an error for 401 response, got a stream"),
        Err(e) => e.to_string(),
    };
    assert!(
        err.contains("401"),
        "error should mention the status code, got: {err}"
    );
}

}

mod openai_tests {
use mirri_kosong::{ChatProvider, OpenAILegacyProvider, StreamEvent};
use serde_json::json;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::*};

/// Helper: mount a mock SSE response on `server` and return the provider.
async fn mount_sse(server: &MockServer, body: &str) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body.to_string()),
        )
        .mount(server)
        .await;
}

/// Helper: collect all events from a stream until `None`.
async fn collect_events(stream: &mut mirri_kosong::SseStream) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    while let Some(event) = stream.next_event().await {
        events.push(event.expect("event should parse"));
    }
    events
}

/// Given a text-only streaming response, When the stream is consumed, Then
/// separate `TextDelta` events should be emitted for each content chunk
/// followed by a `Done` with stop reason "completed".
#[tokio::test]
async fn should_emit_text_deltas_when_streaming_text_only_response() {
    let server = MockServer::start().await;

    mount_sse(
        &server,
        "data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"}}]}\n\n\
         data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"}}]}\n\n\
         data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n\
         data: [DONE]\n\n",
    )
    .await;

    let provider = OpenAILegacyProvider::new("test-key".to_string(), format!("{}/v1", server.uri()));

    let mut stream = provider
        .chat(json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true,
        }))
        .await
        .expect("chat should succeed");

    let events = collect_events(&mut stream).await;

    // Expect: TextDelta("Hello"), TextDelta(" world"), Done("completed"),
    //         Usage(10, 5)
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "Hello world");

    let done = events.iter().find_map(|e| match e {
        StreamEvent::Done { stop_reason } => Some(stop_reason.clone()),
        _ => None,
    });
    assert_eq!(done.as_deref(), Some("completed"));
}

/// Given a function-call streaming response, When the stream is consumed,
/// Then a complete `ToolCall` should be emitted with the accumulated
/// arguments, followed by `Done` with stop reason "tool_calls".
#[tokio::test]
async fn should_emit_tool_call_when_streaming_function_call_response() {
    let server = MockServer::start().await;

    mount_sse(
        &server,
        "data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_abc\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"\"}}]}}]}\n\n\
         data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"city\\\":\\\"SF\\\"}\"}}]}}]}\n\n\
         data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
         data: [DONE]\n\n",
    )
    .await;

    let provider = OpenAILegacyProvider::new("test-key".to_string(), format!("{}/v1", server.uri()));

    let mut stream = provider
        .chat(json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "weather in SF"}],
            "stream": true,
        }))
        .await
        .expect("chat should succeed");

    let events = collect_events(&mut stream).await;

    let tool_calls: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCall {
                tool,
                args,
                call_id,
            } => Some((tool.clone(), args.clone(), call_id.clone())),
            _ => None,
        })
        .collect();

    assert_eq!(tool_calls.len(), 1, "expected exactly one tool call");
    assert_eq!(tool_calls[0].0, "get_weather");
    assert_eq!(tool_calls[0].1["city"], "SF");
    assert_eq!(tool_calls[0].2, "call_abc");

    let done = events.iter().find_map(|e| match e {
        StreamEvent::Done { stop_reason } => Some(stop_reason.clone()),
        _ => None,
    });
    assert_eq!(done.as_deref(), Some("tool_calls"));
}

/// Given a streaming response whose accumulated tool-call `arguments`
/// fragments do not form valid JSON (a truncated or corrupted stream), When
/// the stream is consumed, Then the flushed `ToolCall` must carry an empty
/// JSON object instead of `null` — a `null` arguments value is rejected by
/// OpenAI-compatible servers ("function.arguments must be a JSON object")
/// and would poison every subsequent turn that replays the persisted call.
#[tokio::test]
async fn should_flush_empty_object_args_when_accumulated_arguments_are_invalid_json() {
    let server = MockServer::start().await;

    mount_sse(
        &server,
        "data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_bad\",\"function\":{\"name\":\"Edit\",\"arguments\":\"\"}}]}}]}\n\n\
         data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\":\\\"/tm\"}}]}}]}\n\n\
         data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
         data: [DONE]\n\n",
    )
    .await;

    let provider = OpenAILegacyProvider::new("test-key".to_string(), format!("{}/v1", server.uri()));

    let mut stream = provider
        .chat(json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "edit a file"}],
            "stream": true,
        }))
        .await
        .expect("chat should succeed");

    let events = collect_events(&mut stream).await;

    let tool_calls: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCall {
                tool,
                args,
                call_id,
            } => Some((tool.clone(), args.clone(), call_id.clone())),
            _ => None,
        })
        .collect();

    assert_eq!(tool_calls.len(), 1, "expected exactly one tool call");
    assert_eq!(tool_calls[0].0, "Edit");
    assert_eq!(
        tool_calls[0].1, json!({}),
        "invalid accumulated arguments must degrade to an empty JSON object, never null"
    );
    assert_eq!(tool_calls[0].2, "call_bad");
}

/// Given a streaming response whose accumulated tool-call `arguments` form
/// valid JSON that is not an object (e.g. an array), When the stream is
/// consumed, Then the flushed `ToolCall` must carry an empty JSON object —
/// non-object arguments are rejected by OpenAI-compatible servers
/// ("function.arguments must be a JSON object") when the call is replayed.
#[tokio::test]
async fn should_flush_empty_object_args_when_accumulated_arguments_are_non_object_json() {
    let server = MockServer::start().await;

    mount_sse(
        &server,
        "data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_arr\",\"function\":{\"name\":\"Edit\",\"arguments\":\"\"}}]}}]}\n\n\
         data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"[1\"}}]}}]}\n\n\
         data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\",2]\"}}]}}]}\n\n\
         data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
         data: [DONE]\n\n",
    )
    .await;

    let provider = OpenAILegacyProvider::new("test-key".to_string(), format!("{}/v1", server.uri()));

    let mut stream = provider
        .chat(json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "edit a file"}],
            "stream": true,
        }))
        .await
        .expect("chat should succeed");

    let events = collect_events(&mut stream).await;

    let tool_calls: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCall {
                tool,
                args,
                call_id,
            } => Some((tool.clone(), args.clone(), call_id.clone())),
            _ => None,
        })
        .collect();

    assert_eq!(tool_calls.len(), 1, "expected exactly one tool call");
    assert_eq!(tool_calls[0].0, "Edit");
    assert_eq!(
        tool_calls[0].1, json!({}),
        "non-object accumulated arguments must degrade to an empty JSON object"
    );
    assert_eq!(tool_calls[0].2, "call_arr");
}

/// Given a multi-function streaming response with two tool calls at
/// different indices, When the stream is consumed, Then two separate
/// `ToolCall` events should be emitted — one per function.
#[tokio::test]
async fn should_emit_multiple_tool_calls_when_streaming_multi_function_response() {
    let server = MockServer::start().await;

    mount_sse(
        &server,
        "data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"{\\\"city\\\":\\\"SF\\\"}\"}}]}}]}\n\n\
         data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_2\",\"function\":{\"name\":\"get_time\",\"arguments\":\"{\\\"zone\\\":\\\"PST\\\"}\"}}]}}]}\n\n\
         data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
         data: [DONE]\n\n",
    )
    .await;

    let provider = OpenAILegacyProvider::new("test-key".to_string(), format!("{}/v1", server.uri()));

    let mut stream = provider
        .chat(json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "weather and time in SF"}],
            "stream": true,
        }))
        .await
        .expect("chat should succeed");

    let events = collect_events(&mut stream).await;

    let tool_calls: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCall {
                tool,
                args,
                call_id,
            } => Some((tool.clone(), args.clone(), call_id.clone())),
            _ => None,
        })
        .collect();

    assert_eq!(tool_calls.len(), 2, "expected exactly two tool calls");
    assert_eq!(tool_calls[0].0, "get_weather");
    assert_eq!(tool_calls[0].1["city"], "SF");
    assert_eq!(tool_calls[0].2, "call_1");
    assert_eq!(tool_calls[1].0, "get_time");
    assert_eq!(tool_calls[1].1["zone"], "PST");
    assert_eq!(tool_calls[1].2, "call_2");
}

/// Given tool call arguments that arrive in incremental fragments, When the
/// stream is consumed, Then `ToolCallPart` events should be emitted for each
/// fragment, followed by a complete `ToolCall` with the fully assembled
/// arguments.
#[tokio::test]
async fn should_emit_tool_call_parts_then_complete_tool_call_when_arguments_arrive_incrementally() {
    let server = MockServer::start().await;

    mount_sse(
        &server,
        "data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_xyz\",\"function\":{\"name\":\"search\",\"arguments\":\"\"}}]}}]}\n\n\
         data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"q\\\":\\\"hel\"}}]}}]}\n\n\
         data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"lo\\\"}\"}}]}}]}\n\n\
         data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
         data: [DONE]\n\n",
    )
    .await;

    let provider = OpenAILegacyProvider::new("test-key".to_string(), format!("{}/v1", server.uri()));

    let mut stream = provider
        .chat(json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "search hello"}],
            "stream": true,
        }))
        .await
        .expect("chat should succeed");

    let events = collect_events(&mut stream).await;

    // Collect ToolCallPart fragments
    let parts: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCallPart { arguments_part, .. } => Some(arguments_part.clone()),
            _ => None,
        })
        .collect();

    // Two non-empty argument fragments should produce two ToolCallPart events.
    assert_eq!(parts.len(), 2, "expected two ToolCallPart events");
    assert_eq!(parts[0], "{\"q\":\"hel");
    assert_eq!(parts[1], "lo\"}");

    // The complete ToolCall should have the assembled arguments.
    let tool_calls: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCall {
                tool,
                args,
                call_id,
            } => Some((tool.clone(), args.clone(), call_id.clone())),
            _ => None,
        })
        .collect();

    assert_eq!(
        tool_calls.len(),
        1,
        "expected exactly one complete tool call"
    );
    assert_eq!(tool_calls[0].0, "search");
    assert_eq!(tool_calls[0].1["q"], "hello");
    assert_eq!(tool_calls[0].2, "call_xyz");
}

/// Given incremental tool-call argument fragments whose first chunk carries
/// the call id and function name, When the stream is consumed, Then every
/// `ToolCallPart` should carry that call id and tool name so downstream
/// consumers can route interleaved parallel calls live, without waiting for
/// the block-level `ToolCall` (which only flushes at stream end).
#[tokio::test]
async fn should_carry_call_id_and_tool_name_on_every_tool_call_part() {
    let server = MockServer::start().await;

    mount_sse(
        &server,
        "data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_xyz\",\"function\":{\"name\":\"search\",\"arguments\":\"\"}}]}}]}\n\n\
         data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"q\\\":\\\"hel\"}}]}}]}\n\n\
         data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"lo\\\"}\"}}]}}]}\n\n\
         data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
         data: [DONE]\n\n",
    )
    .await;

    let provider = OpenAILegacyProvider::new("test-key".to_string(), format!("{}/v1", server.uri()));

    let mut stream = provider
        .chat(json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "search hello"}],
            "stream": true,
        }))
        .await
        .expect("chat should succeed");

    let events = collect_events(&mut stream).await;

    let parts: Vec<(Option<String>, Option<String>)> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCallPart {
                call_id, tool_name, ..
            } => Some((call_id.clone(), tool_name.clone())),
            _ => None,
        })
        .collect();

    assert_eq!(parts.len(), 2, "expected two ToolCallPart events");
    for (call_id, tool_name) in &parts {
        assert_eq!(call_id.as_deref(), Some("call_xyz"));
        assert_eq!(tool_name.as_deref(), Some("search"));
    }
}

/// Given an argument fragment that arrives for a stream index before the
/// chunk carrying the call id and function name, When the stream is consumed,
/// Then that `ToolCallPart` should carry no call id or tool name (identity
/// not yet known), while later fragments carry them once the header arrives.
#[tokio::test]
async fn should_leave_call_id_absent_on_parts_that_precede_the_identity_header() {
    let server = MockServer::start().await;

    mount_sse(
        &server,
        "data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"a\\\":\"}}]}}]}\n\n\
         data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_late\",\"function\":{\"name\":\"late\",\"arguments\":\"1\\\"}\"}}]}}]}\n\n\
         data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
         data: [DONE]\n\n",
    )
    .await;

    let provider = OpenAILegacyProvider::new("test-key".to_string(), format!("{}/v1", server.uri()));

    let mut stream = provider
        .chat(json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "late header"}],
            "stream": true,
        }))
        .await
        .expect("chat should succeed");

    let events = collect_events(&mut stream).await;

    let parts: Vec<(Option<String>, Option<String>)> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCallPart {
                call_id, tool_name, ..
            } => Some((call_id.clone(), tool_name.clone())),
            _ => None,
        })
        .collect();

    assert_eq!(parts.len(), 2, "expected two ToolCallPart events");
    assert!(
        parts[0].0.is_none() && parts[0].1.is_none(),
        "pre-header part must not claim an identity: {parts:?}"
    );
    assert_eq!(parts[1].0.as_deref(), Some("call_late"));
    assert_eq!(parts[1].1.as_deref(), Some("late"));
}

/// Given a final chunk that includes a `usage` object (via
/// `stream_options.include_usage`), When the stream is consumed, Then a
/// `Usage` event should be emitted with the correct token counts.
#[tokio::test]
async fn should_emit_usage_when_final_chunk_includes_usage() {
    let server = MockServer::start().await;

    mount_sse(
        &server,
        "data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"}}]}\n\n\
         data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
         data: {\"id\":\"1\",\"choices\":[],\"usage\":{\"prompt_tokens\":42,\"completion_tokens\":7,\"prompt_tokens_details\":{\"cached_tokens\":2}}}\n\n\
         data: [DONE]\n\n",
    )
    .await;

    let provider = OpenAILegacyProvider::new("test-key".to_string(), format!("{}/v1", server.uri()));

    let mut stream = provider
        .chat(json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true,
            "stream_options": {"include_usage": true},
        }))
        .await
        .expect("chat should succeed");

    let events = collect_events(&mut stream).await;

    let usage_events: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::Usage {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_creation_tokens,
            } => Some((
                *input_tokens,
                *output_tokens,
                *cache_read_tokens,
                *cache_creation_tokens,
            )),
            _ => None,
        })
        .collect();

    assert_eq!(usage_events.len(), 1, "expected exactly one Usage event");
    // prompt_tokens(42) - cached_tokens(2) = 40 non-cached input tokens
    assert_eq!(usage_events[0].0, 40, "input_tokens should subtract cached");
    assert_eq!(
        usage_events[0].1, 7,
        "output_tokens should match completion_tokens"
    );
    // Then the subtracted cached tokens are preserved in the cache-read bucket
    assert_eq!(
        usage_events[0].2, 2,
        "cache_read_tokens should carry cached_tokens"
    );
    assert_eq!(
        usage_events[0].3, 0,
        "cache_creation_tokens should stay zero for OpenAI/DeepSeek"
    );
}

/// Given a streaming response that includes `reasoning_content` in the delta,
/// When the stream is consumed, Then `ThinkDelta` events should be emitted
/// for the reasoning fragments.
#[tokio::test]
async fn should_emit_think_delta_when_reasoning_content_present() {
    let server = MockServer::start().await;

    mount_sse(
        &server,
        "data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"Let me think\"}}]}\n\n\
         data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\" about this\"}}]}\n\n\
         data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"42\"}}]}\n\n\
         data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
         data: [DONE]\n\n",
    )
    .await;

    let provider = OpenAILegacyProvider::new("test-key".to_string(), format!("{}/v1", server.uri()));

    let mut stream = provider
        .chat(json!({
            "model": "o1",
            "messages": [{"role": "user", "content": "what is 40+2"}],
            "stream": true,
        }))
        .await
        .expect("chat should succeed");

    let events = collect_events(&mut stream).await;

    let thinking: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ThinkDelta { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(thinking, "Let me think about this");

    let text: String = events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "42");

    let done = events.iter().find_map(|e| match e {
        StreamEvent::Done { stop_reason } => Some(stop_reason.clone()),
        _ => None,
    });
    assert_eq!(done.as_deref(), Some("completed"));
}

/// Given a non-success HTTP status, When `chat()` is called, Then an error
/// should be returned (before any streaming begins).
#[tokio::test]
async fn should_return_error_when_api_returns_non_success_status() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string(r#"{"error":{"message":"Invalid API key"}}"#),
        )
        .mount(&server)
        .await;

    let provider = OpenAILegacyProvider::new("bad-key".to_string(), format!("{}/v1", server.uri()));

    let result = provider
        .chat(json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true,
        }))
        .await;

    let err = match result {
        Ok(_) => panic!("expected an error for 401 response, got a stream"),
        Err(e) => e.to_string(),
    };
    assert!(
        err.contains("401"),
        "error should mention the status code, got: {err}"
    );
}

/// Given a config-provider base URL that already includes the version
/// segment (the TS convention, e.g. `https://…/v1/`), When the Chat
/// Completions endpoint URL is built, Then the base is used as-is — the
/// request goes to `<base>/chat/completions`.
#[tokio::test]
async fn should_hit_the_versioned_base_directly_when_base_url_includes_v1() {
    let server = MockServer::start().await;
    let base = format!("{}/v1/", server.uri());

    mount_sse(&server, "data: [DONE]\n\n").await;

    let provider = OpenAILegacyProvider::new("key".to_string(), base);
    let result = provider
        .chat(json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true,
        }))
        .await;

    assert!(
        result.is_ok(),
        "a versioned base must not double the version segment: {:?}",
        result.map(|_| ())
    );
}

/// Given a provider configured with extra headers (a gateway-required
/// `User-Agent` from the provider's `customHeaders`), When a chat request is
/// sent, Then every extra header must arrive on the request alongside the
/// bearer auth and content-type headers.
#[tokio::test]
async fn should_send_extra_headers_when_the_provider_carries_custom_headers() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(header("authorization", "Bearer test-key"))
        .and(header("user-agent", "opencode/1.18.19"))
        .and(header("x-gateway-tag", "rust-engine"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string("data: [DONE]\n\n".to_string()),
        )
        .mount(&server)
        .await;

    let provider = OpenAILegacyProvider::new("test-key".to_string(), format!("{}/v1", server.uri()))
        .with_extra_headers(vec![
            ("User-Agent".to_string(), "opencode/1.18.19".to_string()),
            ("X-Gateway-Tag".to_string(), "rust-engine".to_string()),
        ]);

    let stream = provider
        .chat(json!({
            "model": "gpt-4",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true,
        }))
        .await;

    assert!(
        stream.is_ok(),
        "request with extra headers must be accepted"
    );
}

/// Given a provider whose upstream accepts the connection but then goes
/// silent (never sends a byte), When `chat()` is called, Then the request must
/// fail fast on the read timeout instead of hanging the turn forever.
#[tokio::test]
async fn should_fail_the_request_when_the_provider_stream_goes_silent() {
    // Given a server that accepts the connection but never sends a byte
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        // Load-bearing: `_socket` keeps the accepted connection open across the never-ending scope, so the client stalls on the response read — exercising read-timeout, not a connect reset.
        let (_socket, _) = listener.accept().await.unwrap();
        std::future::pending::<()>().await;
    });
    let client = mirri_kosong::http_client_with_timeouts(
        std::time::Duration::from_secs(5),
        std::time::Duration::from_millis(200),
    )
    .expect("client builds");
    let provider =
        mirri_kosong::openai::OpenAILegacyProvider::new("key".into(), format!("http://{addr}/v1"))
            .with_client(client);

    // When calling chat, the silent stream times out instead of hanging
    let started = std::time::Instant::now();
    let result = provider
        .chat(serde_json::json!({ "model": "m", "messages": [], "stream": true }))
        .await;
    assert!(result.is_err());
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

/// Given a provider whose upstream keeps the connection alive with SSE
/// keep-alive comments (`: ping`) but never sends a `data:` line, When the
/// stream is consumed, Then the read must fail with an idle timeout error
/// instead of hanging forever.
///
/// This is the 2026-10-19 incident shape: a wedged provider stream sent
/// second-scale keep-alives, which kept resetting reqwest's byte-level
/// `read_timeout` — the stream hung for 14+ minutes until the user aborted.
/// The data-line-level idle timeout must fire despite the keep-alives.
#[tokio::test]
async fn should_fail_when_the_stream_only_sends_keep_alive_comments() {
    use std::time::Duration;

    // Given a raw TCP server that answers the HTTP POST with SSE headers and
    // an unbounded run of keep-alive comment lines, never any `data:` line.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => return,
            };
            tokio::spawn(async move {
                use tokio::io::AsyncWriteExt;
                // Drain the request head (read until end of headers).
                let mut buf = vec![0u8; 8192];
                use tokio::io::AsyncReadExt;
                let mut header_end = Vec::new();
                loop {
                    let n = match socket.read(&mut buf).await {
                        Ok(0) | Err(_) => return,
                        Ok(n) => n,
                    };
                    header_end.extend_from_slice(&buf[..n]);
                    if header_end.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n";
                if socket.write_all(head.as_bytes()).await.is_err() {
                    return;
                }
                // Keep-alive comments forever — bytes keep flowing, so the
                // byte-level read timeout never trips; only a data-line-level
                // idle timeout can catch this.
                let ping = b": ping\n\n";
                loop {
                    if socket.write_all(ping).await.is_err() {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            });
        }
    });

    let client = mirri_kosong::http_client_with_timeouts(
        Duration::from_secs(5),
        Duration::from_secs(10),
    )
    .expect("client builds");
    let provider =
        mirri_kosong::openai::OpenAILegacyProvider::new("key".into(), format!("http://{addr}/v1"))
            .with_client(client);

    // When consuming the stream (with the shortened idle window injected)
    let sse = provider
        .chat(serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true,
        }))
        .await
        .expect("chat connects");

    // Inject a short idle window (the default is 120s — too long to test);
    // the keep-alive cadence (50ms) stays well under it, so only the
    // data-line-level timeout can fire.
    let mut sse = sse.with_idle_timeout(Duration::from_millis(300));

    let started = std::time::Instant::now();
    // Then the next_event read fails with an idle-timeout error rather than
    // hanging.
    let result = sse.next_event().await;
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "the read must not hang; it took {elapsed:?}"
    );
    match result {
        Some(Err(err)) => {
            let text = err.to_string();
            assert!(
                text.to_lowercase().contains("idle timeout"),
                "expected an idle-timeout-shaped error, got: {text}"
            );
        }
        other => panic!("expected an error from the keep-alive-only stream, got {other:?}"),
    }
}

/// Given a stream whose data lines are valid, When an upstream body error
/// occurs, Then the surfaced message must carry the `error reading SSE
/// stream:` prefix EXACTLY ONCE — `next_data_line` already wraps body
/// errors, and double-wrapping would degrade the user-visible wording
/// (regression guard for the idle-timeout refactor).
#[tokio::test]
async fn should_not_double_wrap_the_sse_error_prefix_on_body_errors() {
    use std::time::Duration;

    // Given a server that streams one valid data line, then aborts the
    // connection mid-body (an incomplete chunk → body read error).
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut buf = vec![0u8; 8192];
        let mut header_end = Vec::new();
        loop {
            let n = match socket.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            header_end.extend_from_slice(&buf[..n]);
            if header_end.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n";
        socket.write_all(head.as_bytes()).await.unwrap();
        // One valid OpenAI-style data line, then a truncated JSON chunk and
        // an abrupt close — the reader sees a body/decode error.
        socket
            .write_all(b"data: {\"id\":\"x\",\"choices\":[]}\n\n")
            .await
            .unwrap();
        socket.write_all(b"data: {\"trunc").await.unwrap();
        // Drop without shutdown: the half-open chunk surfaces as a body
        // read/decode error on the client side.
        drop(socket);
    });

    let client = mirri_kosong::http_client_with_timeouts(
        Duration::from_secs(5),
        Duration::from_secs(5),
    )
    .expect("client builds");
    let provider =
        mirri_kosong::openai::OpenAILegacyProvider::new("key".into(), format!("http://{addr}/v1"))
            .with_client(client);

    let sse = provider
        .chat(serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true,
        }))
        .await
        .expect("chat connects");

    // When draining the stream, the error (if any) must carry the prefix
    // exactly once.
    let mut sse = sse;
    let mut saw_error = false;
    while let Some(result) = sse.next_event().await {
        if let Err(err) = result {
            saw_error = true;
            let text = err.to_string();
            let count = text.matches("error reading SSE stream").count();
            assert_eq!(
                count, 1,
                "the SSE error prefix must appear exactly once, got {count} in: {text}"
            );
        }
    }
    // The truncated close may or may not surface as an error depending on
    // TCP timing, but if it does, the prefix shape is asserted above.
    let _ = saw_error;
}

}

mod provider_conformance_tests {
//! Cross-provider conformance tests.
//!
//! These tests verify that all four providers — Anthropic, OpenAI Legacy,
//! OpenAI Responses, and Google GenAI — produce semantically equivalent
//! [`StreamEvent`] sequences when given the same logical input, despite
//! their radically different native SSE wire formats.
//!
//! Each test sets up a [`wiremock::MockServer`] per provider with a mock SSE
//! response in that provider's native format, then collects and compares the
//! normalized [`StreamEvent`]s. The comparison focuses on event *types* and
//! *semantic content* (text, tool name, args) rather than exact token counts
//! or provider-specific ordering quirks (e.g. Anthropic emits an extra
//! `MessageStart` + initial `Usage` from `message_start`, while OpenAI
//! Legacy/Responses emit `Done` before `Usage`).

use mirri_kosong::{
    AnthropicProvider, ChatProvider, GoogleGenAIProvider, OpenAILegacyProvider,
    OpenAIResponsesProvider, SseStream, StreamEvent,
};
use serde_json::{Value, json};
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::*};

// ===========================================================================
// Helper functions
// ===========================================================================

/// Return a static name for a [`StreamEvent`] variant, for comparison.
fn event_type_name(event: &StreamEvent) -> &'static str {
    match event {
        StreamEvent::MessageStart { .. } => "MessageStart",
        StreamEvent::TextDelta { .. } => "TextDelta",
        StreamEvent::ThinkDelta { .. } => "ThinkDelta",
        StreamEvent::ToolCall { .. } => "ToolCall",
        StreamEvent::ToolCallPart { .. } => "ToolCallPart",
        StreamEvent::Usage { .. } => "Usage",
        StreamEvent::Done { .. } => "Done",
        StreamEvent::Error { .. } => "Error",
    }
}

/// Collect all events from a stream until `None`, panicking on any error.
async fn collect_events(stream: &mut SseStream) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    while let Some(result) = stream.next_event().await {
        match result {
            Ok(event) => events.push(event),
            Err(err) => panic!("stream error: {err}"),
        }
    }
    events
}

/// Assert that `events` contains at least the expected count of each event
/// type listed in `expected`. This is a *subset* / *minimum-count* check —
/// providers may emit additional provider-specific events (e.g. Anthropic's
/// `MessageStart` or an initial `Usage` from `message_start`) that are not
/// counted here.
fn assert_contains_event_types(events: &[StreamEvent], expected: &[&str]) {
    for &expected_type in expected {
        let expected_count = expected.iter().filter(|&&t| t == expected_type).count();
        let actual_count = events
            .iter()
            .filter(|e| event_type_name(e) == expected_type)
            .count();
        assert!(
            actual_count >= expected_count,
            "expected at least {expected_count} {expected_type} event(s), got {actual_count} \
             — full event list: {:?}",
            events.iter().map(event_type_name).collect::<Vec<_>>()
        );
    }
}

/// Concatenate all `TextDelta` text from `events`.
fn extract_text(events: &[StreamEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::TextDelta { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// Extract all complete `ToolCall` events as (tool, args, call_id) tuples.
fn extract_tool_calls(events: &[StreamEvent]) -> Vec<(String, Value, String)> {
    events
        .iter()
        .filter_map(|e| match e {
            StreamEvent::ToolCall {
                tool,
                args,
                call_id,
            } => Some((tool.clone(), args.clone(), call_id.clone())),
            _ => None,
        })
        .collect()
}

/// Sum all `Usage` events' input and output tokens.
fn extract_total_usage(events: &[StreamEvent]) -> (u64, u64) {
    let mut input_tokens = 0u64;
    let mut output_tokens = 0u64;
    for event in events {
        if let StreamEvent::Usage {
            input_tokens: it,
            output_tokens: ot,
            ..
        } = event
        {
            input_tokens += it;
            output_tokens += ot;
        }
    }
    (input_tokens, output_tokens)
}

/// Return `true` if `events` contains at least one `Done` event.
fn has_done_event(events: &[StreamEvent]) -> bool {
    events.iter().any(|e| matches!(e, StreamEvent::Done { .. }))
}

// ===========================================================================
// Mock SSE builders — text-only response "Hello world"
// ===========================================================================

fn anthropic_text_sse() -> String {
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
     data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n\
     event: message_stop\n\
     data: {\"type\":\"message_stop\"}\n\n"
        .to_string()
}

fn openai_legacy_text_sse() -> String {
    "data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"}}]}\n\n\
     data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"}}]}\n\n\
     data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n\
     data: [DONE]\n\n"
        .to_string()
}

fn openai_responses_text_sse() -> String {
    "event: response.created\n\
     data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"status\":\"in_progress\"}}\n\n\
     event: response.output_text.delta\n\
     data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello\"}\n\n\
     event: response.output_text.delta\n\
     data: {\"type\":\"response.output_text.delta\",\"delta\":\" world\"}\n\n\
     event: response.completed\n\
     data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}}\n\n"
        .to_string()
}

fn google_text_sse() -> String {
    "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hello\"}],\"role\":\"model\"}}]}\n\n\
     data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\" world\"}],\"role\":\"model\"},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":10,\"candidatesTokenCount\":5}}\n\n"
        .to_string()
}

// ===========================================================================
// Mock SSE builders — tool call response
// ===========================================================================

fn anthropic_tool_call_sse() -> String {
    "event: message_start\n\
     data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-sonnet-4-20250514\",\"usage\":{\"input_tokens\":10}}}\n\n\
     event: content_block_start\n\
     data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call_1\",\"name\":\"get_weather\",\"input\":{}}}\n\n\
     event: content_block_delta\n\
     data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"city\\\":\\\"SF\\\"}\"}}\n\n\
     event: content_block_stop\n\
     data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
     event: message_delta\n\
     data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":5}}\n\n\
     event: message_stop\n\
     data: {\"type\":\"message_stop\"}\n\n"
        .to_string()
}

fn openai_legacy_tool_call_sse() -> String {
    "data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"\"}}]}}]}\n\n\
     data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"city\\\":\\\"SF\\\"}\"}}]}}]}\n\n\
     data: {\"id\":\"1\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
     data: [DONE]\n\n"
        .to_string()
}

fn openai_responses_tool_call_sse() -> String {
    "event: response.output_item.added\n\
     data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"fc_1\",\"name\":\"get_weather\",\"call_id\":\"call_1\"}}\n\n\
     event: response.function_call_arguments.delta\n\
     data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc_1\",\"delta\":\"{\\\"city\\\":\\\"SF\\\"}\"}\n\n\
     event: response.completed\n\
     data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[{\"type\":\"function_call\",\"id\":\"fc_1\",\"name\":\"get_weather\",\"call_id\":\"call_1\",\"arguments\":\"{\\\"city\\\":\\\"SF\\\"}\"}]}}\n\n"
        .to_string()
}

fn google_tool_call_sse() -> String {
    "data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"get_weather\",\"args\":{\"city\":\"SF\"}}}],\"role\":\"model\"},\"finishReason\":\"STOP\"}]}\n\n"
        .to_string()
}

// ===========================================================================
// Mock mount helpers
// ===========================================================================

/// Mount a mock SSE response on `server` for the Anthropic Messages endpoint.
async fn mount_anthropic_sse(server: &MockServer, body: &str) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body.to_string()),
        )
        .mount(server)
        .await;
}

/// Mount a mock SSE response on `server` for the OpenAI Chat Completions
/// endpoint.
async fn mount_openai_legacy_sse(server: &MockServer, body: &str) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body.to_string()),
        )
        .mount(server)
        .await;
}

/// Mount a mock SSE response on `server` for the OpenAI Responses endpoint.
async fn mount_openai_responses_sse(server: &MockServer, body: &str) {
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body.to_string()),
        )
        .mount(server)
        .await;
}

/// Mount a mock SSE response on `server` for the Google GenAI streaming
/// endpoint.
async fn mount_google_sse(server: &MockServer, body: &str) {
    Mock::given(method("POST"))
        .and(path_regex(r"/v1beta/models/.+:generateContentStream"))
        .and(query_param("key", "test-key"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body.to_string()),
        )
        .mount(server)
        .await;
}

/// Mount a 401 Unauthorized error response on `server` for the Anthropic
/// Messages endpoint.
async fn mount_anthropic_error(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string(r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#),
        )
        .mount(server)
        .await;
}

/// Mount a 401 Unauthorized error response on `server` for the OpenAI Chat
/// Completions endpoint.
async fn mount_openai_legacy_error(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string(r#"{"error":{"message":"Invalid API key"}}"#),
        )
        .mount(server)
        .await;
}

/// Mount a 401 Unauthorized error response on `server` for the OpenAI
/// Responses endpoint.
async fn mount_openai_responses_error(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string(r#"{"error":{"message":"Invalid API key"}}"#),
        )
        .mount(server)
        .await;
}

/// Mount a 401 Unauthorized error response on `server` for the Google GenAI
/// streaming endpoint.
async fn mount_google_error(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path_regex(r"/v1beta/models/.+:generateContentStream"))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string(r#"{"error":{"message":"API key not valid"}}"#),
        )
        .mount(server)
        .await;
}

// ===========================================================================
// Request body builders
// ===========================================================================

fn anthropic_request() -> Value {
    json!({
        "model": "claude-sonnet-4-20250514",
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": "hi"}],
    })
}

fn openai_legacy_request() -> Value {
    json!({
        "model": "gpt-4",
        "messages": [{"role": "user", "content": "hi"}],
        "stream": true,
    })
}

fn openai_responses_request() -> Value {
    json!({
        "model": "gpt-4o",
        "input": "hi",
        "stream": true,
    })
}

fn google_request() -> Value {
    json!({
        "model": "gemini-2.0-flash",
        "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
    })
}

// ===========================================================================
// Test 1: Consistent text-only response across all providers
// ===========================================================================

/// Given all four providers with mock SSE responses carrying a simple
/// "Hello world" text response, When each provider's stream is consumed,
/// Then all providers should produce at least 2 `TextDelta` events
/// (concatenating to "Hello world"), 1 `Done` event, and at least 1
/// `Usage` event — regardless of their different native SSE wire formats.
#[tokio::test]
async fn should_produce_consistent_text_only_response_across_all_providers_when_all_providers_stream_text()
 {
    // ---- Given: mock servers for each provider ----
    let anthropic_server = MockServer::start().await;
    let openai_legacy_server = MockServer::start().await;
    let openai_responses_server = MockServer::start().await;
    let google_server = MockServer::start().await;

    mount_anthropic_sse(&anthropic_server, &anthropic_text_sse()).await;
    mount_openai_legacy_sse(&openai_legacy_server, &openai_legacy_text_sse()).await;
    mount_openai_responses_sse(&openai_responses_server, &openai_responses_text_sse()).await;
    mount_google_sse(&google_server, &google_text_sse()).await;

    // ---- When: each provider sends a chat request and we collect events ----
    let anthropic_provider = AnthropicProvider::new("test-key".to_string(), format!("{}/v1", anthropic_server.uri()));
    let mut anthropic_stream = anthropic_provider
        .chat(anthropic_request())
        .await
        .expect("anthropic chat should succeed");
    let anthropic_events = collect_events(&mut anthropic_stream).await;

    let openai_legacy_provider =
        OpenAILegacyProvider::new("test-key".to_string(), format!("{}/v1", openai_legacy_server.uri()));
    let mut openai_legacy_stream = openai_legacy_provider
        .chat(openai_legacy_request())
        .await
        .expect("openai legacy chat should succeed");
    let openai_legacy_events = collect_events(&mut openai_legacy_stream).await;

    let openai_responses_provider =
        OpenAIResponsesProvider::new("test-key".to_string(), format!("{}/v1", openai_responses_server.uri()));
    let mut openai_responses_stream = openai_responses_provider
        .chat(openai_responses_request())
        .await
        .expect("openai responses chat should succeed");
    let openai_responses_events = collect_events(&mut openai_responses_stream).await;

    let google_provider = GoogleGenAIProvider::new("test-key".to_string(), format!("{}/v1beta", google_server.uri()));
    let mut google_stream = google_provider
        .chat(google_request())
        .await
        .expect("google chat should succeed");
    let google_events = collect_events(&mut google_stream).await;

    // ---- Then: all providers produce consistent event types ----
    let all_event_sets = [
        ("anthropic", &anthropic_events[..]),
        ("openai_legacy", &openai_legacy_events[..]),
        ("openai_responses", &openai_responses_events[..]),
        ("google", &google_events[..]),
    ];

    let expected_types = ["TextDelta", "TextDelta", "Done", "Usage"];

    for (_provider_name, events) in &all_event_sets {
        assert_contains_event_types(events, &expected_types);
    }

    // ---- Then: all providers produce the same text content ----
    for (provider_name, events) in &all_event_sets {
        let text = extract_text(events);
        assert_eq!(
            text, "Hello world",
            "{provider_name}: text content mismatch"
        );
    }

    // ---- Then: all providers produce at least one Done event ----
    for (provider_name, events) in &all_event_sets {
        assert!(
            has_done_event(events),
            "{provider_name}: expected at least one Done event"
        );
    }

    // ---- Then: all providers produce at least one Usage event with
    //       input_tokens > 0 and output_tokens > 0 (summed) ----
    for (provider_name, events) in &all_event_sets {
        let (input_tokens, output_tokens) = extract_total_usage(events);
        assert!(
            input_tokens > 0,
            "{provider_name}: expected input_tokens > 0, got {input_tokens}"
        );
        assert!(
            output_tokens > 0,
            "{provider_name}: expected output_tokens > 0, got {output_tokens}"
        );
    }
}

// ===========================================================================
// Test 2: Consistent tool call response across all providers
// ===========================================================================

/// Given all four providers with mock SSE responses carrying a tool call
/// (tool="get_weather", args={"city":"SF"}), When each provider's stream is
/// consumed, Then all providers should produce at least 1 `ToolCall` event
/// with the same tool name and args, and 1 `Done` event — regardless of
/// their different native SSE wire formats.
#[tokio::test]
async fn should_produce_consistent_tool_call_response_across_all_providers_when_all_providers_stream_tool_call()
 {
    // ---- Given: mock servers for each provider ----
    let anthropic_server = MockServer::start().await;
    let openai_legacy_server = MockServer::start().await;
    let openai_responses_server = MockServer::start().await;
    let google_server = MockServer::start().await;

    mount_anthropic_sse(&anthropic_server, &anthropic_tool_call_sse()).await;
    mount_openai_legacy_sse(&openai_legacy_server, &openai_legacy_tool_call_sse()).await;
    mount_openai_responses_sse(&openai_responses_server, &openai_responses_tool_call_sse()).await;
    mount_google_sse(&google_server, &google_tool_call_sse()).await;

    // ---- When: each provider sends a chat request and we collect events ----
    let anthropic_provider = AnthropicProvider::new("test-key".to_string(), format!("{}/v1", anthropic_server.uri()));
    let mut anthropic_stream = anthropic_provider
        .chat(anthropic_request())
        .await
        .expect("anthropic chat should succeed");
    let anthropic_events = collect_events(&mut anthropic_stream).await;

    let openai_legacy_provider =
        OpenAILegacyProvider::new("test-key".to_string(), format!("{}/v1", openai_legacy_server.uri()));
    let mut openai_legacy_stream = openai_legacy_provider
        .chat(openai_legacy_request())
        .await
        .expect("openai legacy chat should succeed");
    let openai_legacy_events = collect_events(&mut openai_legacy_stream).await;

    let openai_responses_provider =
        OpenAIResponsesProvider::new("test-key".to_string(), format!("{}/v1", openai_responses_server.uri()));
    let mut openai_responses_stream = openai_responses_provider
        .chat(openai_responses_request())
        .await
        .expect("openai responses chat should succeed");
    let openai_responses_events = collect_events(&mut openai_responses_stream).await;

    let google_provider = GoogleGenAIProvider::new("test-key".to_string(), format!("{}/v1beta", google_server.uri()));
    let mut google_stream = google_provider
        .chat(google_request())
        .await
        .expect("google chat should succeed");
    let google_events = collect_events(&mut google_stream).await;

    // ---- Then: all providers produce at least 1 ToolCall and 1 Done ----
    let all_event_sets = [
        ("anthropic", &anthropic_events[..]),
        ("openai_legacy", &openai_legacy_events[..]),
        ("openai_responses", &openai_responses_events[..]),
        ("google", &google_events[..]),
    ];

    let expected_types = ["ToolCall", "Done"];

    for (_provider_name, events) in &all_event_sets {
        assert_contains_event_types(events, &expected_types);
    }

    // ---- Then: all providers produce a ToolCall with the same tool name
    //       and args (call_id may differ — Google does not provide one) ----
    for (provider_name, events) in &all_event_sets {
        let tool_calls = extract_tool_calls(events);
        assert_eq!(
            tool_calls.len(),
            1,
            "{provider_name}: expected exactly 1 ToolCall, got {}",
            tool_calls.len()
        );
        assert_eq!(
            tool_calls[0].0, "get_weather",
            "{provider_name}: tool name mismatch"
        );
        assert_eq!(
            tool_calls[0].1["city"], "SF",
            "{provider_name}: tool args[\"city\"] mismatch"
        );
    }

    // ---- Then: all providers produce at least one Done event ----
    for (provider_name, events) in &all_event_sets {
        assert!(
            has_done_event(events),
            "{provider_name}: expected at least one Done event"
        );
    }
}

// ===========================================================================
// Test 3: Consistent error handling across all providers
// ===========================================================================

/// Given all four providers with mock servers returning 401 Unauthorized,
/// When each provider's `chat()` method is called, Then all providers should
/// return an `Err` — the error must be surfaced before any streaming begins.
#[tokio::test]
async fn should_produce_consistent_error_handling_across_all_providers_when_api_returns_error_status()
 {
    // ---- Given: mock servers returning 401 for each provider ----
    let anthropic_server = MockServer::start().await;
    let openai_legacy_server = MockServer::start().await;
    let openai_responses_server = MockServer::start().await;
    let google_server = MockServer::start().await;

    mount_anthropic_error(&anthropic_server).await;
    mount_openai_legacy_error(&openai_legacy_server).await;
    mount_openai_responses_error(&openai_responses_server).await;
    mount_google_error(&google_server).await;

    // ---- When / Then: each provider's chat() should return Err ----
    let anthropic_provider = AnthropicProvider::new("bad-key".to_string(), format!("{}/v1", anthropic_server.uri()));
    let anthropic_result = anthropic_provider.chat(anthropic_request()).await;
    assert!(
        anthropic_result.is_err(),
        "anthropic: expected Err for 401, got Ok"
    );

    let openai_legacy_provider =
        OpenAILegacyProvider::new("bad-key".to_string(), format!("{}/v1", openai_legacy_server.uri()));
    let openai_legacy_result = openai_legacy_provider.chat(openai_legacy_request()).await;
    assert!(
        openai_legacy_result.is_err(),
        "openai_legacy: expected Err for 401, got Ok"
    );

    let openai_responses_provider =
        OpenAIResponsesProvider::new("bad-key".to_string(), format!("{}/v1", openai_responses_server.uri()));
    let openai_responses_result = openai_responses_provider
        .chat(openai_responses_request())
        .await;
    assert!(
        openai_responses_result.is_err(),
        "openai_responses: expected Err for 401, got Ok"
    );

    let google_provider = GoogleGenAIProvider::new("bad-key".to_string(), format!("{}/v1beta", google_server.uri()));
    let google_result = google_provider.chat(google_request()).await;
    assert!(
        google_result.is_err(),
        "google: expected Err for 401, got Ok"
    );
}

// ===========================================================================
// Test 4: Consistent usage reporting across all providers
// ===========================================================================

/// Given all four providers with mock SSE responses that include token usage
/// information, When each provider's stream is consumed, Then all providers
/// should produce at least 1 `Usage` event with `input_tokens > 0` and
/// `output_tokens > 0` — regardless of their different native usage field
/// names and structures.
#[tokio::test]
async fn should_produce_consistent_usage_reporting_across_all_providers_when_response_includes_usage()
 {
    // ---- Given: mock servers with usage-carrying SSE responses ----
    //
    // We reuse the text-only SSE mocks, which include usage in their
    // respective formats:
    // - Anthropic: input_tokens in message_start, output_tokens in
    //   message_delta
    // - OpenAI Legacy: prompt_tokens + completion_tokens in the final chunk
    // - OpenAI Responses: input_tokens + output_tokens in response.completed
    // - Google: promptTokenCount + candidatesTokenCount in usageMetadata
    let anthropic_server = MockServer::start().await;
    let openai_legacy_server = MockServer::start().await;
    let openai_responses_server = MockServer::start().await;
    let google_server = MockServer::start().await;

    mount_anthropic_sse(&anthropic_server, &anthropic_text_sse()).await;
    mount_openai_legacy_sse(&openai_legacy_server, &openai_legacy_text_sse()).await;
    mount_openai_responses_sse(&openai_responses_server, &openai_responses_text_sse()).await;
    mount_google_sse(&google_server, &google_text_sse()).await;

    // ---- When: each provider sends a chat request and we collect events ----
    let anthropic_provider = AnthropicProvider::new("test-key".to_string(), format!("{}/v1", anthropic_server.uri()));
    let mut anthropic_stream = anthropic_provider
        .chat(anthropic_request())
        .await
        .expect("anthropic chat should succeed");
    let anthropic_events = collect_events(&mut anthropic_stream).await;

    let openai_legacy_provider =
        OpenAILegacyProvider::new("test-key".to_string(), format!("{}/v1", openai_legacy_server.uri()));
    let mut openai_legacy_stream = openai_legacy_provider
        .chat(openai_legacy_request())
        .await
        .expect("openai legacy chat should succeed");
    let openai_legacy_events = collect_events(&mut openai_legacy_stream).await;

    let openai_responses_provider =
        OpenAIResponsesProvider::new("test-key".to_string(), format!("{}/v1", openai_responses_server.uri()));
    let mut openai_responses_stream = openai_responses_provider
        .chat(openai_responses_request())
        .await
        .expect("openai responses chat should succeed");
    let openai_responses_events = collect_events(&mut openai_responses_stream).await;

    let google_provider = GoogleGenAIProvider::new("test-key".to_string(), format!("{}/v1beta", google_server.uri()));
    let mut google_stream = google_provider
        .chat(google_request())
        .await
        .expect("google chat should succeed");
    let google_events = collect_events(&mut google_stream).await;

    // ---- Then: all providers produce at least 1 Usage event with
    //       input_tokens > 0 and output_tokens > 0 ----
    let all_event_sets = [
        ("anthropic", &anthropic_events[..]),
        ("openai_legacy", &openai_legacy_events[..]),
        ("openai_responses", &openai_responses_events[..]),
        ("google", &google_events[..]),
    ];

    for (provider_name, events) in &all_event_sets {
        let (input_tokens, output_tokens) = extract_total_usage(events);
        assert!(
            input_tokens > 0,
            "{provider_name}: expected total input_tokens > 0, got {input_tokens}"
        );
        assert!(
            output_tokens > 0,
            "{provider_name}: expected total output_tokens > 0, got {output_tokens}"
        );
    }
}

}

mod tls_smoke {
//! Live TLS smoke test and dependency guard for the rustls-backed HTTP client.
//!
//! The workspace pins `reqwest` to `rustls-tls` (no `default-tls`), so no
//! OpenSSL is linked on any target. Two protections live here:
//!
//! 1. A lockfile purity guard: the resolved workspace `Cargo.lock` must not
//!    contain `openssl-sys`, so any change that reintroduces native-tls
//!    (e.g. re-enabling reqwest default features) fails a normal test run.
//! 2. A live TLS handshake test against a public endpoint, proving the
//!    rustls path works end to end. The wiremock suites exercise the HTTP
//!    stack over plain HTTP only.
//!
//! The live test is `#[ignore]`d by default: it requires network access.
//! Run locally with:
//!
//! ```sh
//! cargo test -p mirri-kosong --test tls_smoke -- --ignored
//! ```

#[test]
fn should_not_have_openssl_in_the_resolved_workspace_lockfile() {
    let lock_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.lock");
    let lock = std::fs::read_to_string(&lock_path)
        .expect("workspace Cargo.lock should be readable from mirri-kosong");

    assert!(
        !lock.contains("name = \"openssl-sys\""),
        "openssl-sys found in workspace Cargo.lock — the HTTP stack must stay on rustls \
         (check the reqwest features in the workspace Cargo.toml)"
    );
}

#[tokio::test]
#[ignore = "requires network access"]
async fn should_complete_real_tls_handshake_when_fetching_https_endpoint() {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .expect("client with rustls backend should build");

    let response = client
        .get("https://example.com/")
        .send()
        .await
        .expect("TLS handshake and HTTP round trip should succeed against example.com");

    assert!(
        response.status().is_success(),
        "expected success status, got {}",
        response.status()
    );
}

}

