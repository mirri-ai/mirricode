use std::collections::HashMap;

use anyhow::Result;
use serde_json::Value;

use crate::provider::{
    ChatProvider, HTTP_CONNECT_TIMEOUT, HTTP_READ_TIMEOUT, StreamEvent, http_client_with_timeouts,
};
use crate::streaming::{SseDispatcher, SseStream};

/// OpenAI Chat Completions API streaming client.
///
/// Sends a raw JSON request body (matching the OpenAI `/v1/chat/completions`
/// schema, including `model`, `messages`, and `stream: true`) and returns
/// an [`SseStream`] over the response body on success. Non-2xx responses are
/// converted into an `anyhow::Error` before any streaming begins.
///
/// The provider only owns transport concerns: bearer-token authentication,
/// endpoint routing, and HTTP status validation. All SSE parsing lives in
/// [`SseStream`] paired with [`OpenAILegacyDispatcher`].
pub struct OpenAILegacyProvider {
    api_key: String,
    base_url: String,
    client: reqwest::Client,
    /// Extra headers from the provider config (`customHeaders`, e.g. a
    /// gateway-required `User-Agent`), applied to every request in addition
    /// to the auth and content-type headers.
    extra_headers: Vec<(String, String)>,
}

impl OpenAILegacyProvider {
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

    /// Attach config-declared custom headers (e.g. `User-Agent`) that
    /// gateways may require; a missing allowlisted header can make an
    /// endpoint hang or drop the request instead of answering.
    pub fn with_extra_headers(mut self, headers: Vec<(String, String)>) -> Self {
        self.extra_headers = headers;
        self
    }

    /// Swap the HTTP client (tests inject short-timeout clients).
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    /// Full Chat Completions endpoint URL.
    fn completions_url(&self) -> String {
        crate::provider::join_url(&self.base_url, "/chat/completions")
    }
}

impl ChatProvider for OpenAILegacyProvider {
    async fn chat(&self, request: Value) -> Result<SseStream> {
        let mut request_builder = self
            .client
            .post(self.completions_url())
            .header("authorization", format!("Bearer {}", self.api_key))
            .header("content-type", "application/json");
        for (name, value) in &self.extra_headers {
            request_builder = request_builder.header(name, value);
        }
        let response = request_builder.json(&request).send().await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            anyhow::bail!("OpenAI API error {status}: {body}");
        }

        Ok(SseStream::new(
            response,
            Box::new(OpenAILegacyDispatcher::default()),
        ))
    }
}

// ---------------------------------------------------------------------------
// PendingToolCall — accumulation state for a single streaming tool call
// ---------------------------------------------------------------------------

/// Accumulation state for one OpenAI streaming tool call.
///
/// OpenAI streams tool calls across multiple chunks addressed by `index`:
/// the first chunk carries `id` and `function.name`, subsequent chunks carry
/// `function.arguments` fragments that must be concatenated, and a new
/// `index` (or `finish_reason: "tool_calls"`) signals that the call is
/// complete. This struct holds the in-progress state until the call can be
/// flushed into a [`StreamEvent::ToolCall`].
#[derive(Default)]
struct PendingToolCall {
    /// Tool/function name, populated from the first chunk for this `index`.
    tool_name: String,
    /// Provider-issued tool call id, populated from the first chunk.
    call_id: String,
    /// Concatenated `function.arguments` fragments, parsed into JSON on flush.
    accumulated_args: String,
}

// ---------------------------------------------------------------------------
// OpenAILegacyDispatcher — OpenAI Chat Completions SSE dispatch
// ---------------------------------------------------------------------------

/// SSE dispatcher for OpenAI Chat Completions streaming responses.
///
/// OpenAI streams chunks as `data: {json}\n\n`, terminated by `data: [DONE]`.
/// Each chunk has a `choices[0].delta` object that may contain:
/// - `content` (text delta)
/// - `reasoning_content` (reasoning delta, e.g. o1 models)
/// - `tool_calls` (array of tool call deltas, indexed by `index`)
///
/// A `finish_reason` on the choice signals completion. When
/// `stream_options.include_usage` is set, the final chunk (after the
/// `finish_reason` chunk) carries a top-level `usage` object.
#[derive(Default)]
pub struct OpenAILegacyDispatcher {
    /// Pending tool calls being accumulated, keyed by OpenAI's `index` field.
    pending_tool_calls: HashMap<u64, PendingToolCall>,
}

impl SseDispatcher for OpenAILegacyDispatcher {
    fn dispatch(&mut self, data: &str) -> Vec<StreamEvent> {
        // Handle [DONE] — stream terminator. Flush any tool calls that were
        // not closed by a finish_reason (defensive; should not normally happen).
        if data == "[DONE]" {
            return self.flush_all_pending();
        }

        let json: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };

        let mut events = Vec::new();

        // Extract the first choice (OpenAI returns one choice per chunk for
        // non-n streaming, which is the default).
        let choice = match json
            .get("choices")
            .and_then(|c| c.as_array())
            .and_then(|c| c.first())
        {
            Some(c) => c,
            None => {
                // No choices — this is the usage-only final chunk. Process
                // usage if present and return.
                if let Some(usage) = json.get("usage") {
                    events.push(self.extract_usage(usage));
                }
                return events;
            }
        };

        // Process delta — may carry content, reasoning_content, and tool_calls.
        if let Some(delta) = choice.get("delta") {
            // Text content
            if let Some(content) = delta.get("content").and_then(|c| c.as_str())
                && !content.is_empty()
            {
                events.push(StreamEvent::TextDelta {
                    text: content.to_string(),
                });
            }

            // Reasoning content (if model supports, e.g. o1)
            if let Some(reasoning) = delta.get("reasoning_content").and_then(|r| r.as_str())
                && !reasoning.is_empty()
            {
                events.push(StreamEvent::ThinkDelta {
                    text: reasoning.to_string(),
                });
            }

            // Tool calls
            if let Some(tool_calls) = delta.get("tool_calls").and_then(|t| t.as_array()) {
                for tc in tool_calls {
                    self.process_tool_call_delta(tc, &mut events);
                }
            }
        }

        // Check finish_reason — signals that the choice is complete.
        if let Some(finish_reason) = choice.get("finish_reason").and_then(|f| f.as_str()) {
            // Flush all pending tool calls before Done.
            events.extend(self.flush_all_pending());

            // Map OpenAI finish reasons to normalized stop reasons.
            let stop_reason = match finish_reason {
                "stop" => "completed",
                "tool_calls" => "tool_calls",
                "length" => "truncated",
                "content_filter" => "filtered",
                other => other,
            };
            events.push(StreamEvent::Done {
                stop_reason: stop_reason.to_string(),
            });
        }

        // Check usage (may appear in the final chunk with
        // stream_options.include_usage, either alongside choices or alone).
        if let Some(usage) = json.get("usage") {
            events.push(self.extract_usage(usage));
        }

        events
    }
}

impl OpenAILegacyDispatcher {
    /// Process a single `tool_calls[i]` delta object from a choice's delta.
    ///
    /// OpenAI sends tool calls indexed by `index`. The first chunk for a
    /// given index carries `id` and `function.name`; subsequent chunks for
    /// the same index carry `function.arguments` fragments. When a new index
    /// appears, any previously pending tool call at a different index is
    /// considered complete and flushed.
    fn process_tool_call_delta(&mut self, tc: &Value, events: &mut Vec<StreamEvent>) {
        let index = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0);

        // OpenAI sends tool calls sequentially by index: a new index means
        // the previous one is complete. Flush any pending calls at a
        // different index before starting the new one.
        if !self.pending_tool_calls.contains_key(&index) {
            let indices_to_flush: Vec<u64> = self
                .pending_tool_calls
                .keys()
                .filter(|&&k| k != index)
                .copied()
                .collect();
            for idx in indices_to_flush {
                if let Some(event) = self.flush_pending(idx) {
                    events.push(event);
                }
            }
        }

        // Extract or create the pending tool call at this index.
        let pending = self.pending_tool_calls.entry(index).or_default();

        // Tool call id and function name arrive in the first chunk for this
        // index.
        if let Some(id) = tc.get("id").and_then(|i| i.as_str()) {
            pending.call_id = id.to_string();
        }
        if let Some(func) = tc.get("function") {
            if let Some(name) = func.get("name").and_then(|n| n.as_str()) {
                pending.tool_name = name.to_string();
            }
            if let Some(args) = func.get("arguments").and_then(|a| a.as_str())
                && !args.is_empty()
            {
                pending.accumulated_args.push_str(args);
                events.push(StreamEvent::ToolCallPart {
                    index,
                    arguments_part: args.to_string(),
                    call_id: (!pending.call_id.is_empty()).then(|| pending.call_id.clone()),
                    tool_name: (!pending.tool_name.is_empty()).then(|| pending.tool_name.clone()),
                });
            }
        }
    }

    /// Extract a [`StreamEvent::Usage`] from an OpenAI `usage` object.
    ///
    /// Subtracts `prompt_tokens_details.cached_tokens` from `prompt_tokens`
    /// so that `input_tokens` reflects only non-cached input tokens, matching
    /// the accounting semantics of the Anthropic dispatcher. The subtracted
    /// cached tokens are preserved in `cache_read_tokens` so cache hits stay
    /// observable (OpenAI and DeepSeek both report inclusive
    /// `prompt_tokens`).
    fn extract_usage(&self, usage: &Value) -> StreamEvent {
        let prompt_tokens = usage
            .get("prompt_tokens")
            .and_then(|t| t.as_u64())
            .unwrap_or(0);
        let cached_tokens = usage
            .get("prompt_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(|t| t.as_u64())
            .unwrap_or(0);
        let completion_tokens = usage
            .get("completion_tokens")
            .and_then(|t| t.as_u64())
            .unwrap_or(0);
        StreamEvent::Usage {
            input_tokens: prompt_tokens.saturating_sub(cached_tokens),
            output_tokens: completion_tokens,
            cache_read_tokens: cached_tokens,
            cache_creation_tokens: 0,
        }
    }

    /// Flush a specific pending tool call into a complete [`StreamEvent::ToolCall`].
    ///
    /// Returns `None` if there was no pending call at `index`, or if the
    /// pending call never received a tool name (malformed stream).
    fn flush_pending(&mut self, index: u64) -> Option<StreamEvent> {
        let pending = self.pending_tool_calls.remove(&index)?;
        if pending.tool_name.is_empty() {
            return None;
        }
        let args: Value = serde_json::from_str::<Value>(&pending.accumulated_args)
            .ok()
            .filter(|v| v.is_object())
            // Degrade malformed or non-object arguments to an empty object:
            // `null`/non-object values are rejected by OpenAI-compatible
            // servers when the call is replayed in later turns.
            .unwrap_or(Value::Object(serde_json::Map::new()));
        Some(StreamEvent::ToolCall {
            tool: pending.tool_name,
            args,
            call_id: pending.call_id,
        })
    }

    /// Flush all pending tool calls into [`StreamEvent::ToolCall`]s.
    ///
    /// Used on `finish_reason` and `[DONE]` to ensure no tool call is left
    /// un-emitted. Iterates over a snapshot of indices because flushing
    /// mutates the map.
    fn flush_all_pending(&mut self) -> Vec<StreamEvent> {
        let indices: Vec<u64> = self.pending_tool_calls.keys().copied().collect();
        indices
            .into_iter()
            .filter_map(|idx| self.flush_pending(idx))
            .collect()
    }
}
