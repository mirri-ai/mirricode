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
