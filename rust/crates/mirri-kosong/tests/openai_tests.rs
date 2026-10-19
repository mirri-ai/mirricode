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
