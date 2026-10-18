use std::collections::HashMap;

use anyhow::Result;
use serde_json::Value;

use crate::provider::{
    ChatProvider, HTTP_CONNECT_TIMEOUT, HTTP_READ_TIMEOUT, StreamEvent, http_client_with_timeouts,
};
use crate::streaming::{SseDispatcher, SseStream};

// ---------------------------------------------------------------------------
// OpenAIResponsesProvider — transport for the /v1/responses endpoint
// ---------------------------------------------------------------------------

/// OpenAI Responses API streaming client.
///
/// Sends a raw JSON request body (matching the OpenAI `/v1/responses`
/// schema) and returns an [`SseStream`] over the response body on success.
/// Non-2xx responses are converted into an `anyhow::Error` before any
/// streaming begins.
///
/// The provider only owns transport concerns: bearer-token authentication,
/// endpoint routing, and HTTP status validation. All SSE parsing lives in
/// [`SseStream`] paired with [`OpenAIResponsesDispatcher`].
pub struct OpenAIResponsesProvider {
    api_key: String,
    base_url: String,
    client: reqwest::Client,
    /// Extra headers from the provider config (`customHeaders`), applied to
    /// every request in addition to the auth and content-type headers.
    extra_headers: Vec<(String, String)>,
}

impl OpenAIResponsesProvider {
    /// Construct a provider pointing at `base_url` (trailing slashes are
    /// tolerated) using `api_key` for the `Authorization: Bearer` header.
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

    /// Full Responses endpoint URL.
    fn responses_url(&self) -> String {
        crate::provider::join_url(&self.base_url, "/responses")
    }
}

impl ChatProvider for OpenAIResponsesProvider {
    async fn chat(&self, request: Value) -> Result<SseStream> {
        let mut request_builder = self
            .client
            .post(self.responses_url())
            .header("authorization", format!("Bearer {}", self.api_key))
            .header("content-type", "application/json");
        for (name, value) in &self.extra_headers {
            request_builder = request_builder.header(name, value);
        }
        let response = request_builder.json(&request).send().await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("OpenAI Responses API error {status}: {body}");
        }

        Ok(SseStream::new(
            response,
            Box::new(OpenAIResponsesDispatcher::default()),
        ))
    }
}

// ---------------------------------------------------------------------------
// PendingResponseToolCall — accumulation state for a single Responses tool call
// ---------------------------------------------------------------------------

/// Accumulation state for one OpenAI Responses streaming tool call.
///
/// The Responses API routes tool-call deltas by `item_id` rather than a
/// numeric index. A `response.output_item.added` event declares the tool
/// name and `call_id`; subsequent `response.function_call_arguments.delta`
/// events carry argument fragments that must be concatenated. On
/// `response.completed`, the final `response.output` array carries the
/// authoritative `arguments` string, which is used to reconcile any
/// discrepancy between the streamed fragments and the final value before
/// emitting a [`StreamEvent::ToolCall`].
#[derive(Default)]
struct PendingResponseToolCall {
    /// Tool/function name, populated from `response.output_item.added`.
    tool_name: String,
    /// Provider-issued tool call id, populated from `response.output_item.added`.
    call_id: String,
    /// Concatenated `function_call_arguments.delta` fragments.
    accumulated_args: String,
}

// ---------------------------------------------------------------------------
// OpenAIResponsesDispatcher — OpenAI Responses API SSE dispatch
// ---------------------------------------------------------------------------

/// SSE dispatcher for OpenAI Responses API streaming responses.
///
/// The Responses API uses typed SSE events (`event: response.xxx`), but the
/// [`SseLineReader`] only surfaces the `data:` payload. Each `data:` JSON
/// object carries a `type` field that identifies the event, so routing is
/// performed on the JSON `type` rather than the SSE `event:` line.
///
/// Key event types handled:
/// - `response.output_text.delta` → [`StreamEvent::TextDelta`]
/// - `response.reasoning.delta` → [`StreamEvent::ThinkDelta`]
/// - `response.function_call_arguments.delta` → [`StreamEvent::ToolCallPart`]
/// - `response.output_item.added` → registers a pending tool call
/// - `response.completed` → reconciles tool calls, emits [`StreamEvent::ToolCall`]s,
///   [`StreamEvent::Done`], and [`StreamEvent::Usage`]
/// - `response.incomplete` → [`StreamEvent::Done`] with an incomplete/truncated
///   stop reason
#[derive(Default)]
pub struct OpenAIResponsesDispatcher {
    /// Pending tool calls keyed by the Responses API `item_id`.
    pending_tool_calls: HashMap<String, PendingResponseToolCall>,
}

impl SseDispatcher for OpenAIResponsesDispatcher {
    fn dispatch(&mut self, data: &str) -> Vec<StreamEvent> {
        let json: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };

        let event_type = json.get("type").and_then(|t| t.as_str()).unwrap_or("");

        match event_type {
            "response.output_text.delta" => self.dispatch_output_text_delta(&json),
            "response.reasoning.delta" => self.dispatch_reasoning_delta(&json),
            "response.function_call_arguments.delta" => {
                self.dispatch_function_call_arguments_delta(&json)
            }
            "response.output_item.added" => self.dispatch_output_item_added(&json),
            "response.completed" => self.dispatch_completed(&json),
            "response.incomplete" => self.dispatch_incomplete(&json),
            _ => Vec::new(),
        }
    }
}

impl OpenAIResponsesDispatcher {
    /// `response.output_text.delta` — text content increment.
    fn dispatch_output_text_delta(&self, json: &Value) -> Vec<StreamEvent> {
        let text = json
            .get("delta")
            .and_then(|d| d.as_str())
            .unwrap_or("")
            .to_string();
        if text.is_empty() {
            Vec::new()
        } else {
            vec![StreamEvent::TextDelta { text }]
        }
    }

    /// `response.reasoning.delta` — reasoning content increment.
    ///
    /// Some models put the reasoning text in `delta`, others in `summary_text`;
    /// both are accepted.
    fn dispatch_reasoning_delta(&self, json: &Value) -> Vec<StreamEvent> {
        let text = json
            .get("delta")
            .and_then(|d| d.as_str())
            .or_else(|| json.get("summary_text").and_then(|s| s.as_str()))
            .unwrap_or("")
            .to_string();
        if text.is_empty() {
            Vec::new()
        } else {
            vec![StreamEvent::ThinkDelta { text }]
        }
    }

    /// `response.function_call_arguments.delta` — tool-call argument fragment.
    ///
    /// The fragment is accumulated against the pending tool call identified by
    /// `item_id`, and a [`StreamEvent::ToolCallPart`] is emitted immediately so
    /// downstream consumers can stream arguments incrementally.
    fn dispatch_function_call_arguments_delta(&mut self, json: &Value) -> Vec<StreamEvent> {
        let item_id = json
            .get("item_id")
            .and_then(|i| i.as_str())
            .unwrap_or("")
            .to_string();
        let delta = json
            .get("delta")
            .and_then(|d| d.as_str())
            .unwrap_or("")
            .to_string();
        if delta.is_empty() {
            return Vec::new();
        }
        if let Some(pending) = self.pending_tool_calls.get_mut(&item_id) {
            pending.accumulated_args.push_str(&delta);
        }
        // Responses API uses item_id routing, not a numeric index; use 0 as a
        // placeholder since the downstream consumer correlates parts by call
        // id (the item's call id, registered on `output_item.added` — the
        // same id the eventual block-level ToolCall flushes with; not the
        // item_id itself, which is only the routing key).
        let identity = self
            .pending_tool_calls
            .get(&item_id)
            .map(|pending| {
                (
                    (!pending.call_id.is_empty()).then(|| pending.call_id.clone()),
                    (!pending.tool_name.is_empty()).then(|| pending.tool_name.clone()),
                )
            })
            .unwrap_or((None, None));
        vec![StreamEvent::ToolCallPart {
            index: 0,
            arguments_part: delta,
            call_id: identity.0,
            tool_name: identity.1,
        }]
    }

    /// `response.output_item.added` — registers a new tool call when the item
    /// is a `function_call`.
    fn dispatch_output_item_added(&mut self, json: &Value) -> Vec<StreamEvent> {
        let item = json.get("item").unwrap_or(&Value::Null);
        let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if item_type != "function_call" {
            return Vec::new();
        }
        let item_id = item
            .get("id")
            .and_then(|i| i.as_str())
            .unwrap_or("")
            .to_string();
        let name = item
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string();
        let call_id = item
            .get("call_id")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .to_string();
        let pending = PendingResponseToolCall {
            tool_name: name,
            call_id,
            accumulated_args: String::new(),
        };
        self.pending_tool_calls.insert(item_id, pending);
        Vec::new()
    }

    /// `response.completed` — terminal event carrying the final output, stop
    /// reason, and usage.
    ///
    /// For each tool call in `response.output`:
    /// 1. Reconcile streamed arguments against the authoritative final
    ///    `arguments`; if the streamed accumulation is a prefix of the final
    ///    value, emit the missing suffix as a [`StreamEvent::ToolCallPart`].
    /// 2. Emit the complete [`StreamEvent::ToolCall`].
    ///
    /// Then emit [`StreamEvent::Done`] with a normalized stop reason and, if
    /// present, [`StreamEvent::Usage`].
    fn dispatch_completed(&mut self, json: &Value) -> Vec<StreamEvent> {
        let mut events = Vec::new();

        let response = json.get("response").unwrap_or(&Value::Null);

        // Reconcile and emit tool calls from the authoritative output array.
        if let Some(output) = response.get("output").and_then(|o| o.as_array()) {
            for item in output {
                let item_type = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
                if item_type != "function_call" {
                    continue;
                }
                let item_id = item
                    .get("id")
                    .and_then(|i| i.as_str())
                    .unwrap_or("")
                    .to_string();
                let full_args = item.get("arguments").and_then(|a| a.as_str()).unwrap_or("");

                // Argument reconciliation: if the streamed accumulation is a
                // strict prefix of the final arguments, emit the missing suffix.
                if let Some(pending) = self.pending_tool_calls.get(&item_id) {
                    if !full_args.starts_with(&pending.accumulated_args) {
                        // The accumulated args do not form a prefix of the final
                        // value; skip reconciliation to avoid emitting garbage.
                    } else if pending.accumulated_args.len() < full_args.len() {
                        let suffix = &full_args[pending.accumulated_args.len()..];
                        if !suffix.is_empty() {
                            events.push(StreamEvent::ToolCallPart {
                                index: 0,
                                arguments_part: suffix.to_string(),
                                call_id: (!pending.call_id.is_empty())
                                    .then(|| pending.call_id.clone()),
                                tool_name: (!pending.tool_name.is_empty())
                                    .then(|| pending.tool_name.clone()),
                            });
                        }
                    }
                }

                // Emit the complete ToolCall using the authoritative arguments.
                // Malformed or non-object arguments degrade to an empty
                // object: `null`/non-object values are rejected by
                // OpenAI-compatible servers when the call is replayed.
                let args: Value = serde_json::from_str::<Value>(full_args)
                    .ok()
                    .filter(|v| v.is_object())
                    .unwrap_or(Value::Object(serde_json::Map::new()));

                let (tool_name, call_id) =
                    if let Some(pending) = self.pending_tool_calls.remove(&item_id) {
                        (pending.tool_name, pending.call_id)
                    } else {
                        (
                            item.get("name")
                                .and_then(|n| n.as_str())
                                .unwrap_or("")
                                .to_string(),
                            item.get("call_id")
                                .and_then(|c| c.as_str())
                                .unwrap_or("")
                                .to_string(),
                        )
                    };

                events.push(StreamEvent::ToolCall {
                    tool: tool_name,
                    args,
                    call_id,
                });
            }
        }

        // Flush any remaining pending tool calls that did not appear in the
        // final output (defensive — should not normally happen).
        let remaining: Vec<String> = self.pending_tool_calls.keys().cloned().collect();
        for item_id in remaining {
            if let Some(pending) = self.pending_tool_calls.remove(&item_id) {
                let args: Value = serde_json::from_str::<Value>(&pending.accumulated_args)
                    .ok()
                    .filter(|v| v.is_object())
                    .unwrap_or(Value::Object(serde_json::Map::new()));
                events.push(StreamEvent::ToolCall {
                    tool: pending.tool_name,
                    args,
                    call_id: pending.call_id,
                });
            }
        }

        // Stop reason.
        let status = response
            .get("status")
            .and_then(|s| s.as_str())
            .unwrap_or("completed");
        let stop_reason = match status {
            "completed" => "completed",
            "incomplete" => {
                let reason = response
                    .get("incomplete_details")
                    .and_then(|d| d.get("reason"))
                    .and_then(|r| r.as_str())
                    .unwrap_or("");
                match reason {
                    "max_output_tokens" => "truncated",
                    _ => "incomplete",
                }
            }
            other => other,
        };
        events.push(StreamEvent::Done {
            stop_reason: stop_reason.to_string(),
        });

        // Usage.
        if let Some(usage) = response.get("usage") {
            let input_tokens = usage
                .get("input_tokens")
                .and_then(|t| t.as_u64())
                .unwrap_or(0);
            let cached_tokens = usage
                .get("input_tokens_details")
                .and_then(|d| d.get("cached_tokens"))
                .and_then(|t| t.as_u64())
                .unwrap_or(0);
            let output_tokens = usage
                .get("output_tokens")
                .and_then(|t| t.as_u64())
                .unwrap_or(0);
            events.push(StreamEvent::Usage {
                input_tokens: input_tokens.saturating_sub(cached_tokens),
                output_tokens,
                cache_read_tokens: cached_tokens,
                cache_creation_tokens: 0,
            });
        }

        events
    }

    /// `response.incomplete` — terminal event signalling the response did not
    /// complete normally.
    fn dispatch_incomplete(&self, json: &Value) -> Vec<StreamEvent> {
        let response = json.get("response").unwrap_or(&Value::Null);
        let reason = response
            .get("incomplete_details")
            .and_then(|d| d.get("reason"))
            .and_then(|r| r.as_str())
            .unwrap_or("");
        let stop_reason = match reason {
            "max_output_tokens" => "truncated",
            _ => "incomplete",
        };
        vec![StreamEvent::Done {
            stop_reason: stop_reason.to_string(),
        }]
    }
}
