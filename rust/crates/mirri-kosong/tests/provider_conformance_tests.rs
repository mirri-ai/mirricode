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
