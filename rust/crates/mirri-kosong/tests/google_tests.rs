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
