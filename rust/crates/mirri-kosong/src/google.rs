use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::Result;
use serde_json::Value;

use crate::provider::{
    ChatProvider, HTTP_CONNECT_TIMEOUT, HTTP_READ_TIMEOUT, StreamEvent, http_client_with_timeouts,
};
use crate::streaming::{SseDispatcher, SseStream};

/// Google GenAI (Gemini) streaming client.
///
/// Sends a raw JSON request body (matching the Google GenAI
/// `generateContentStream` schema) and returns an [`SseStream`] over the
/// response body on success. Non-2xx responses are converted into an
/// `anyhow::Error` before any streaming begins.
///
/// Unlike the Anthropic and OpenAI providers, Google passes the API key as a
/// URL query parameter (`?key=…`) rather than an `Authorization` header, and
/// the model name is embedded in the URL path (`/v1beta/models/{model}:…`)
/// rather than sent only in the request body. The model is extracted from the
/// request JSON's `model` field to construct the endpoint URL.
pub struct GoogleGenAIProvider {
    api_key: String,
    base_url: String,
    client: reqwest::Client,
    /// Extra headers from the provider config (`customHeaders`), applied to
    /// every request in addition to the content-type header.
    extra_headers: Vec<(String, String)>,
}

impl GoogleGenAIProvider {
    /// Construct a provider pointing at `base_url` (trailing slashes are
    /// tolerated) using `api_key` as the `key` query parameter.
    pub fn new(api_key: String, base_url: String) -> Self {
        Self {
            api_key,
            base_url,
            client: http_client_with_timeouts(HTTP_CONNECT_TIMEOUT, HTTP_READ_TIMEOUT)
                .unwrap_or_else(|error| panic!("static client config builds: {error}")),
            extra_headers: Vec::new(),
        }
    }

    /// Attach config-declared custom headers (e.g. a gateway-required
    /// `User-Agent`) applied to every request.
    pub fn with_extra_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.extra_headers = headers;
        self
    }

    /// Swap the HTTP client (tests inject short-timeout clients).
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    /// Build the `generateContentStream` endpoint for the given model.
    ///
    /// The URL does not include the `key` query parameter; that is appended
    /// in [`ChatProvider::chat`] so that [`stream_url`](Self::stream_url) can
    /// be tested independently of the API key.
    fn stream_url(&self, model: &str) -> String {
        crate::provider::join_url(
            &self.base_url,
            &format!("/models/{model}:generateContentStream"),
        )
    }
}

impl ChatProvider for GoogleGenAIProvider {
    async fn chat(&self, request: Value) -> Result<SseStream> {
        // Extract model from request body to build the URL — Google embeds
        // the model in the path, not just the body.
        let model = request
            .get("model")
            .and_then(|m| m.as_str())
            .unwrap_or("gemini-pro")
            .to_string();

        let url = format!("{}?key={}", self.stream_url(&model), self.api_key);

        let mut request_builder = self
            .client
            .post(&url)
            .header("content-type", "application/json");
        for (name, value) in &self.extra_headers {
            request_builder = request_builder.header(name, value);
        }
        let response = request_builder.json(&request).send().await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("Google GenAI API error {status}: {body}");
        }

        Ok(SseStream::new(response, Box::new(GoogleDispatcher)))
    }
}

// ---------------------------------------------------------------------------
// GoogleDispatcher — Google GenAI SSE dispatch
// ---------------------------------------------------------------------------

/// SSE dispatcher for Google GenAI streaming responses.
///
/// Each `data:` line is a complete `GenerateContentResponse` JSON object —
/// not an incremental delta. The response contains `candidates[]`, each with
/// a `content.parts[]` array that may carry text, thoughts, or function
/// calls.
///
/// Key differences from Anthropic/OpenAI dispatchers:
/// - **Tool calls are complete (non-incremental).** The `functionCall.args`
///   field is a pre-parsed JSON object (not a string), so the dispatcher
///   emits a [`StreamEvent::ToolCall`] directly — no accumulation needed.
/// - **`STOP` is the finish reason even when function calls are present.**
///   Google does not use a distinct `tool_calls` finish reason.
/// - **Usage metadata** (`usageMetadata`) may appear on any chunk but
///   typically arrives on the final one.
#[derive(Default)]
pub struct GoogleDispatcher;

impl SseDispatcher for GoogleDispatcher {
    fn dispatch(&mut self, data: &str) -> Vec<StreamEvent> {
        let json: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };

        let mut events = Vec::new();

        // Process candidates — each chunk may contain one or more.
        if let Some(candidates) = json.get("candidates").and_then(|c| c.as_array()) {
            for candidate in candidates {
                // Process content parts (text, thought, functionCall).
                if let Some(parts) = candidate
                    .get("content")
                    .and_then(|c| c.get("parts"))
                    .and_then(|p| p.as_array())
                {
                    for part in parts {
                        self.process_part(part, &mut events);
                    }
                }

                // Check finish reason — signals the candidate is complete.
                if let Some(finish_reason) = candidate.get("finishReason").and_then(|f| f.as_str())
                {
                    let stop_reason = match finish_reason {
                        "STOP" => "completed",
                        "MAX_TOKENS" => "truncated",
                        "SAFETY" => "filtered",
                        "RECITATION" => "filtered",
                        other => other,
                    };
                    events.push(StreamEvent::Done {
                        stop_reason: stop_reason.to_string(),
                    });
                }
            }
        }

        // Usage metadata — typically on the final chunk. `cachedContentTokenCount`
        // is subtracted from `promptTokenCount` (inclusive) and preserved in
        // the cache-read bucket.
        if let Some(usage) = json.get("usageMetadata") {
            let prompt_tokens = usage
                .get("promptTokenCount")
                .and_then(|t| t.as_u64())
                .unwrap_or(0);
            let cached_tokens = usage
                .get("cachedContentTokenCount")
                .and_then(|t| t.as_u64())
                .unwrap_or(0);
            let candidates_tokens = usage
                .get("candidatesTokenCount")
                .and_then(|t| t.as_u64())
                .unwrap_or(0);
            events.push(StreamEvent::Usage {
                input_tokens: prompt_tokens.saturating_sub(cached_tokens),
                output_tokens: candidates_tokens,
                cache_read_tokens: cached_tokens,
                cache_creation_tokens: 0,
            });
        }

        events
    }
}

impl GoogleDispatcher {
    /// Process a single content `part` and push any resulting events.
    ///
    /// A part may be:
    /// - A **thought** (`thought: true`) — its `text` is emitted as
    ///   [`StreamEvent::ThinkDelta`].
    /// - A **text** part — its `text` is emitted as
    ///   [`StreamEvent::TextDelta`] (non-empty only).
    /// - A **function call** — `functionCall.args` is already a parsed JSON
    ///   object, so a complete [`StreamEvent::ToolCall`] is emitted directly.
    ///
    /// When a part is marked as a thought, the text is emitted as
    /// `ThinkDelta` and *not* as `TextDelta`, even though the `text` field
    /// is present in both cases.
    fn process_part(&self, part: &Value, events: &mut Vec<StreamEvent>) {
        // Thought / reasoning content — the `thought` flag distinguishes
        // reasoning from normal text output.
        if part
            .get("thought")
            .and_then(|t| t.as_bool())
            .unwrap_or(false)
        {
            if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                events.push(StreamEvent::ThinkDelta {
                    text: text.to_string(),
                });
            }
            return;
        }

        // Normal text content.
        if let Some(text) = part.get("text").and_then(|t| t.as_str())
            && !text.is_empty()
        {
            events.push(StreamEvent::TextDelta {
                text: text.to_string(),
            });
        }

        // Function call — complete, non-incremental. Google sends the
        // already-parsed `args` object (not a JSON string), so we emit a
        // ToolCall directly without any accumulation.
        if let Some(func_call) = part.get("functionCall") {
            let name = func_call
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            let args = func_call
                .get("args")
                .cloned()
                // Degrade a missing or non-object `args` to an empty object:
                // tool-call arguments must be a JSON object on every
                // OpenAI/Anthropic/Google-compatible replay.
                .filter(|v| v.is_object())
                .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
            // Google does not provide a tool call id — synthesize a
            // unique one (monotonic per-process counter; unique within a
            // session, and replayed transcripts carry their stored id).
            let call_id = format!(
                "{name}_{}",
                NEXT_GOOGLE_CALL_ID.fetch_add(1, Ordering::Relaxed)
            );
            events.push(StreamEvent::ToolCall {
                tool: name,
                args,
                call_id,
            });
        }
    }
}

/// Monotonic counter for synthesized Google tool-call ids.
static NEXT_GOOGLE_CALL_ID: AtomicU64 = AtomicU64::new(1);
