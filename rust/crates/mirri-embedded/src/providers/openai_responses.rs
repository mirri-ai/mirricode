//! [`OpenAIResponsesLlm`] — a [`Llm`](mirri_core::Llm) adapter over the
//! OpenAI Responses API (`/v1/responses`).
//!
//! The adapter owns the wire-format translation between the engine's
//! canonical [`Message`]/[`ToolDefinition`] shapes and the Responses API's
//! `input` array of typed items (`message`, `function_call`,
//! `function_call_output`, `reasoning`).
//! [`OpenAIResponsesProvider`](mirri_kosong::OpenAIResponsesProvider) owns the
//! transport and SSE parsing; [`OpenAIResponsesDispatcher`] normalizes the
//! streamed SSE events into [`StreamEvent`]s that the shared
//! [`spawn_provider_chat`] pump forwards as [`LlmEvent`]s.

use std::sync::Arc;

use mirri_core::error::EngineResult;
use mirri_core::llm::{Llm, LlmChatParams, LlmEventStream, ModelCapability, ProviderErrorInfo};
use mirri_core::message::{ContentPart, Message, Role, ToolCall};
use mirri_core::profile::ThinkingEffort;
use mirri_core::tool::ToolDefinition;
use mirri_kosong::OpenAIResponsesProvider;
use serde_json::{Map, Value, json};

use super::convert::spawn_provider_chat;
use super::tool_result_media::{TOOL_RESULT_MEDIA_PROMPT, tool_result_text_content};

/// An [`Llm`] speaking the OpenAI Responses wire format.
///
/// The Responses API uses a flat `input` array of typed items rather than the
/// Chat Completions `messages` array. Key differences:
/// - System prompt → top-level `instructions` field (not an input item).
/// - Tool calls → `{ type: "function_call", call_id, name, arguments }`.
/// - Tool results → `{ type: "function_call_output", call_id, output }`.
/// - Think parts → `{ type: "reasoning", summary: [{ type: "summary_text", text }] }`.
/// - Tools → `{ type: "function", name, description, parameters, strict: false }`.
pub struct OpenAIResponsesLlm {
    /// The underlying HTTP transport, shared with the streaming task.
    provider: Arc<OpenAIResponsesProvider>,
    /// The model serving this adapter (e.g. `"o3"`).
    model: String,
    /// The system prompt, sent as the top-level `instructions` field.
    system_prompt: String,
    /// Reasoning-effort configuration; `None` / `None`-variant omits `reasoning`.
    thinking_effort: Option<ThinkingEffort>,
    /// Model capability flags.
    capability: ModelCapability,
}

impl OpenAIResponsesLlm {
    /// Construct an adapter for the given API key, base URL (no trailing
    /// slash), model, and system prompt.
    pub fn new(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        system_prompt: impl Into<String>,
        thinking_effort: Option<ThinkingEffort>,
    ) -> Self {
        Self::with_headers(
            api_key,
            base_url,
            model,
            system_prompt,
            thinking_effort,
            Vec::new(),
        )
    }

    /// Construct an adapter that additionally forwards `headers` (the
    /// provider config's `customHeaders`) on every transport request.
    pub fn with_headers(
        api_key: impl Into<String>,
        base_url: impl Into<String>,
        model: impl Into<String>,
        system_prompt: impl Into<String>,
        thinking_effort: Option<ThinkingEffort>,
        headers: Vec<(String, String)>,
    ) -> Self {
        Self {
            provider: Arc::new(
                OpenAIResponsesProvider::new(api_key.into(), base_url.into())
                    .with_extra_headers(headers),
            ),
            model: model.into(),
            system_prompt: system_prompt.into(),
            thinking_effort,
            capability: ModelCapability {
                image_in: false,
                video_in: false,
                audio_in: false,
                dynamically_loaded_tools: false,
            },
        }
    }

    /// Merge catalog-resolved media capabilities into the intrinsic set
    /// (multimodal spec US3.1: catalog resolution merges into the intrinsic
    /// set (OR); unresolved capabilities default to unsupported, ADR-003
    /// revision).
    pub fn with_media_capability(mut self, resolved: ModelCapability) -> Self {
        self.capability.image_in |= resolved.image_in;
        self.capability.video_in |= resolved.video_in;
        self.capability.audio_in |= resolved.audio_in;
        self
    }

    /// Build the OpenAI Responses API request body for `params`.
    ///
    /// The `input` array is built from the conversation history; the system
    /// prompt is placed in the top-level `instructions` field. Reasoning
    /// effort, when configured, is emitted as `{ reasoning: { effort, summary:
    /// "auto" }, include: ["reasoning.encrypted_content"] }`.
    fn build_request(&self, params: &LlmChatParams) -> EngineResult<Value> {
        let mut request = Map::new();
        request.insert("model".to_string(), Value::String(self.model.clone()));

        request.insert(
            "input".to_string(),
            Value::Array(self.convert_input(params)),
        );

        let tools = convert_tools_to_responses(&params.tools);
        if !tools.is_empty() {
            request.insert("tools".to_string(), Value::Array(tools));
        }

        request.insert("store".to_string(), Value::Bool(false));
        request.insert("stream".to_string(), Value::Bool(true));

        if !self.system_prompt.is_empty() {
            request.insert(
                "instructions".to_string(),
                Value::String(self.system_prompt.clone()),
            );
        }

        // o-series models accept a reasoning effort; the Responses API routes
        // it through the `reasoning` object rather than the Chat Completions
        // `reasoning_effort` string. Model-declared efforts (e.g. "max") pass
        // through verbatim — the kosong openai wire behavior.
        let reasoning_effort = match &self.thinking_effort {
            Some(ThinkingEffort::Low) => Some("low".to_string()),
            Some(ThinkingEffort::Medium) => Some("medium".to_string()),
            Some(ThinkingEffort::High) => Some("high".to_string()),
            Some(ThinkingEffort::Custom(effort)) => Some(effort.clone()),
            Some(ThinkingEffort::None) | None => None,
        };
        if let Some(level) = reasoning_effort {
            request.insert(
                "reasoning".to_string(),
                json!({ "effort": level, "summary": "auto" }),
            );
            request.insert(
                "include".to_string(),
                json!(["reasoning.encrypted_content"]),
            );
        }

        Ok(Value::Object(request))
    }

    /// Translate engine [`Message`]s into the Responses API `input` array of
    /// typed items.
    ///
    /// Mapping rules:
    /// - The adapter's own system prompt is sent as `instructions` (not here);
    ///   inline `System` messages become `{ type: "message", role: "developer" }`.
    /// - `User` messages become `message` items with `input_text`/`input_image`
    ///   content blocks.
    /// - `Assistant` messages become `message` items with `output_text` blocks
    ///   for text content, plus separate `function_call` items for tool calls.
    /// - `Tool` messages become `function_call_output` items.
    /// - `Think` parts become `reasoning` items with `summary_text` entries.
    /// - `partial` messages are dropped — their truncated content would
    ///   corrupt the conversation history.
    fn convert_input(&self, params: &LlmChatParams) -> Vec<Value> {
        let mut items = Vec::new();
        // Tool-result images collected from tool results; `function_call_output`
        // items carry text only, so the images are reattached as a follow-up
        // user message item (mirrors the Chat Completions dialect's reattach
        // strategy).
        let mut pending_tool_result_media: Vec<Value> = Vec::new();
        for message in &params.messages {
            if message.partial {
                continue;
            }
            if message.tool_call_id.is_none() {
                flush_tool_result_media(&mut items, &mut pending_tool_result_media);
            }
            match message.role {
                Role::System => {
                    // The Responses API routes system/developer instructions
                    // through the `developer` role for o-series models. Non-
                    // reasoning models accept `system` — model-aware routing
                    // is deferred until the model catalog is available.
                    items.push(responses_developer_message(message));
                }
                Role::User => {
                    if let Some(tool_call_id) = &message.tool_call_id {
                        // A user message carrying a tool_call_id is treated as
                        // a tool result (mirrors the Chat Completions adapter).
                        items.push(responses_function_call_output(
                            tool_call_id,
                            tool_result_text_content(message),
                        ));
                        pending_tool_result_media.extend(tool_result_image_items(message));
                    } else {
                        items.push(responses_user_message(message));
                    }
                }
                Role::Assistant => {
                    // Process content parts in order, interleaving reasoning
                    // items (from Think parts) and message items (from Text
                    // parts), mirroring the TS adapter's flush-on-think logic.
                    let mut pending_text = String::new();
                    for part in &message.content {
                        match part {
                            ContentPart::Think { text: think_text } => {
                                if !pending_text.is_empty() {
                                    items.push(responses_assistant_message(&pending_text));
                                    pending_text.clear();
                                }
                                if !think_text.is_empty() {
                                    items.push(responses_reasoning_item(think_text));
                                }
                            }
                            ContentPart::Text { text } => {
                                pending_text.push_str(text);
                            }
                            _ => { /* not representable; skipped */ }
                        }
                    }
                    if !pending_text.is_empty() {
                        items.push(responses_assistant_message(&pending_text));
                    }
                    // Tool calls become function_call items.
                    for call in &message.tool_calls {
                        items.push(responses_function_call(call));
                    }
                }
                Role::Tool => {
                    let Some(tool_call_id) = &message.tool_call_id else {
                        continue;
                    };
                    items.push(responses_function_call_output(
                        tool_call_id,
                        tool_result_text_content(message),
                    ));
                    pending_tool_result_media.extend(tool_result_image_items(message));
                }
            }
        }
        flush_tool_result_media(&mut items, &mut pending_tool_result_media);
        items
    }
}

impl Llm for OpenAIResponsesLlm {
    fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    fn model_name(&self) -> &str {
        &self.model
    }

    fn capability(&self) -> &ModelCapability {
        &self.capability
    }

    fn is_retryable_error(&self, error: &ProviderErrorInfo) -> bool {
        // Network/transport failures, throttles, and 5xx statuses are
        // transient; auth and quota exhaustion are not. The shared verdict
        // also recovers the HTTP status from the error text (transport
        // errors carry none).
        error.is_transient_provider_error()
    }

    fn chat(&self, params: LlmChatParams) -> EngineResult<LlmEventStream> {
        let request = self.build_request(&params)?;
        Ok(spawn_provider_chat(
            Arc::clone(&self.provider),
            request,
            params.cancel,
        ))
    }
}

// --- item constructors ----------------------------------------------------

/// `{ type: "message", role: "developer", content: [{ type: "input_text",
/// text }] }` — the Responses API shape for system/developer messages.
fn responses_developer_message(message: &Message) -> Value {
    json!({
        "type": "message",
        "role": "developer",
        "content": [{ "type": "input_text", "text": message.text_content() }],
    })
}

/// `{ type: "message", role: "user", content: [...] }` — user message with
/// `input_text`/`input_image` content blocks.
fn responses_user_message(message: &Message) -> Value {
    let parts = super::convert::inline_file_attachments(&message.content);
    if let [ContentPart::Text { text }] = parts.as_slice() {
        return json!({
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": text }],
        });
    }

    let mut blocks = Vec::new();
    for part in parts {
        match part {
            ContentPart::Text { text } => {
                if !text.is_empty() {
                    blocks.push(json!({ "type": "input_text", "text": text }));
                }
            }
            ContentPart::ImageUrl { image_url, detail } => {
                let d = detail
                    .clone()
                    .or_else(|| image_url.detail.clone())
                    .unwrap_or_else(|| "auto".to_string());
                blocks.push(json!({
                    "type": "input_image",
                    "detail": d,
                    "image_url": &image_url.url,
                }));
            }
            ContentPart::Think { .. }
            | ContentPart::AudioUrl { .. }
            | ContentPart::VideoUrl { .. }
            | ContentPart::File { .. } => { /* not representable; skipped */ }
        }
    }
    if blocks.is_empty() {
        blocks.push(json!({
            "type": "input_text",
            "text": "[content not representable in the OpenAI Responses API]",
        }));
    }
    json!({ "type": "message", "role": "user", "content": blocks })
}

/// `{ type: "message", role: "assistant", content: [{ type: "output_text",
/// text, annotations: [] }] }` — assistant text output.
fn responses_assistant_message(text: &str) -> Value {
    json!({
        "type": "message",
        "role": "assistant",
        "content": [{ "type": "output_text", "text": text, "annotations": [] }],
    })
}

/// `{ type: "function_call", call_id, name, arguments }` — a tool call
/// issued by the assistant. `arguments` stays a raw JSON string.
fn responses_function_call(call: &ToolCall) -> Value {
    json!({
        "type": "function_call",
        "call_id": &call.id,
        "name": &call.function.name,
        "arguments": &call.function.arguments,
    })
}

/// `{ type: "function_call_output", call_id, output }` — a tool execution
/// result.
fn responses_function_call_output(call_id: &str, output: String) -> Value {
    json!({
        "type": "function_call_output",
        "call_id": call_id,
        "output": output,
    })
}

/// Image parts of a tool result, in the Responses API content-block shape —
/// the media that gets reattached as a follow-up user message item.
fn tool_result_image_items(message: &Message) -> Vec<Value> {
    message
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::ImageUrl { image_url, detail } => {
                let d = detail
                    .clone()
                    .or_else(|| image_url.detail.clone())
                    .unwrap_or_else(|| "auto".to_string());
                Some(json!({
                    "type": "input_image",
                    "detail": d,
                    "image_url": &image_url.url,
                }))
            }
            _ => None,
        })
        .collect()
}

/// Push the pending tool-result media as a follow-up user message item and
/// clear the pending list (mirrors the Chat Completions dialect's reattach
/// strategy; `function_call_output` items carry text only).
fn flush_tool_result_media(items: &mut Vec<Value>, pending: &mut Vec<Value>) {
    if pending.is_empty() {
        return;
    }
    let mut content = vec![json!({ "type": "input_text", "text": TOOL_RESULT_MEDIA_PROMPT })];
    content.append(pending);
    items.push(json!({ "type": "message", "role": "user", "content": content }));
}

/// `{ type: "reasoning", summary: [{ type: "summary_text", text }] }` — a
/// reasoning/thinking block. `encrypted_content` is omitted because the Rust
/// `ContentPart::Think` variant does not carry an encrypted payload.
fn responses_reasoning_item(text: &str) -> Value {
    json!({
        "type": "reasoning",
        "summary": [{ "type": "summary_text", "text": text }],
    })
}

/// Translate [`ToolDefinition`]s into Responses API function-tool entries,
/// skipping deferred (progressive-disclosure) tools.
///
/// Unlike the Chat Completions format, the Responses API uses a flat shape:
/// `{ type: "function", name, description, parameters, strict: false }` — no
/// nested `function.` wrapper.
fn convert_tools_to_responses(tools: &[ToolDefinition]) -> Vec<Value> {
    tools
        .iter()
        .filter(|tool| !tool.deferred)
        .map(|tool| {
            json!({
                "type": "function",
                "name": &tool.name,
                "description": &tool.description,
                "parameters": &tool.parameters,
                "strict": false,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;
    use mirri_core::llm::ProviderErrorInfo;
    use mirri_core::message::{ImageUrlData, ToolCallFunction};
    use mirri_core::{CancelToken, LlmEvent};
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::*};

    use super::*;

    fn chat_params_with(messages: Vec<Message>, tools: Vec<ToolDefinition>) -> LlmChatParams {
        LlmChatParams {
            messages,
            tools,
            cancel: CancelToken::new(),
            request_log_fields: None,
            cache_root_id: None,
        }
    }

    fn test_llm() -> OpenAIResponsesLlm {
        OpenAIResponsesLlm::new(
            "test-api-key",
            "https://api.openai.com/v1",
            "o3-test",
            "You are a test assistant.",
            None,
        )
    }

    #[test]
    fn should_build_responses_request_when_converting_messages_and_tools() {
        // Given a Responses-backed LLM and a full conversation including an
        // inline system message, a multi-part user message, a tool-using
        // assistant message with a think part, a tool result, and one
        // deferred and one active tool
        let llm = test_llm();
        let params = chat_params_with(
            vec![
                Message::system("Be brief."),
                Message::user("hello"),
                Message {
                    role: Role::User,
                    name: None,
                    content: vec![
                        ContentPart::Text {
                            text: "describe".to_string(),
                        },
                        ContentPart::ImageUrl {
                            image_url: ImageUrlData {
                                url: "https://example.com/cat.png".to_string(),
                                detail: Some("low".to_string()),
                                id: None,
                            },
                            detail: None,
                        },
                    ],
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                    partial: false,
                },
                Message {
                    role: Role::Assistant,
                    name: None,
                    content: vec![
                        ContentPart::Think {
                            text: "Let me think.".to_string(),
                        },
                        ContentPart::Text {
                            text: "Let me read it.".to_string(),
                        },
                    ],
                    tool_calls: vec![mirri_core::message::ToolCall {
                        id: "call_1".to_string(),
                        call_type: "function".to_string(),
                        function: ToolCallFunction {
                            name: "Read".to_string(),
                            arguments: r#"{"path":"/tmp"}"#.to_string(),
                        },
                    }],
                    tool_call_id: None,
                    partial: false,
                },
                Message::tool_result("call_1", "file contents"),
            ],
            vec![
                ToolDefinition {
                    name: "Read".to_string(),
                    description: "Read a file".to_string(),
                    parameters: json!({"type": "object"}),
                    deferred: false,
                },
                ToolDefinition {
                    name: "Secret".to_string(),
                    description: "Deferred tool".to_string(),
                    parameters: json!({}),
                    deferred: true,
                },
            ],
        );

        // When the Responses request is built
        let request = llm.build_request(&params).expect("request builds");

        // Then the top-level fields carry the model, store=false, streaming,
        // and the system prompt as instructions
        assert_eq!(request["model"], "o3-test");
        assert_eq!(request["store"], false);
        assert_eq!(request["stream"], true);
        assert_eq!(request["instructions"], "You are a test assistant.");
        assert!(
            request.get("reasoning").is_none(),
            "no reasoning without effort"
        );

        // And the input array carries typed items in conversation order
        let input = request["input"].as_array().expect("input array");
        assert_eq!(
            input[0],
            json!({
                "type": "message",
                "role": "developer",
                "content": [{ "type": "input_text", "text": "Be brief." }]
            })
        );

        // And a single-text user message uses input_text
        assert_eq!(
            input[1],
            json!({
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": "hello" }]
            })
        );

        // And a multi-part user message uses input_text + input_image
        let multi = &input[2];
        assert_eq!(multi["role"], "user");
        assert_eq!(multi["content"][0]["type"], "input_text");
        assert_eq!(multi["content"][1]["type"], "input_image");
        assert_eq!(
            multi["content"][1]["image_url"],
            "https://example.com/cat.png"
        );
        assert_eq!(multi["content"][1]["detail"], "low");

        // And the assistant message carries a reasoning item for the think
        // part, an output_text message, and a function_call item
        assert_eq!(
            input[3],
            json!({
                "type": "reasoning",
                "summary": [{ "type": "summary_text", "text": "Let me think." }]
            })
        );
        assert_eq!(
            input[4],
            json!({
                "type": "message",
                "role": "assistant",
                "content": [{
                    "type": "output_text",
                    "text": "Let me read it.",
                    "annotations": []
                }]
            })
        );
        assert_eq!(
            input[5],
            json!({
                "type": "function_call",
                "call_id": "call_1",
                "name": "Read",
                "arguments": r#"{"path":"/tmp"}"#
            })
        );

        // And the tool result is a function_call_output
        assert_eq!(
            input[6],
            json!({
                "type": "function_call_output",
                "call_id": "call_1",
                "output": "file contents"
            })
        );

        // And only the non-deferred tool is advertised with strict=false
        let tools = request["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["name"], "Read");
        assert_eq!(tools[0]["parameters"], json!({"type": "object"}));
        assert_eq!(tools[0]["strict"], false);
    }

    #[test]
    fn should_set_reasoning_field_when_thinking_effort_is_high() {
        // Given a Responses-backed LLM with High reasoning effort
        let llm = OpenAIResponsesLlm::new(
            "test-api-key",
            "https://api.openai.com/v1",
            "o3-test",
            "",
            Some(ThinkingEffort::High),
        );

        // When the request is built
        let request = llm
            .build_request(&chat_params_with(vec![Message::user("hi")], vec![]))
            .expect("request builds");

        // Then the reasoning field carries the effort and summary=auto
        assert_eq!(
            request["reasoning"],
            json!({ "effort": "high", "summary": "auto" })
        );
        assert_eq!(request["include"], json!(["reasoning.encrypted_content"]));
    }

    #[test]
    fn should_send_model_declared_efforts_verbatim_in_the_reasoning_object() {
        // Given a Responses-backed LLM with a model-declared custom effort
        // (e.g. "max" — a level the canonical enum does not spell)
        let llm = OpenAIResponsesLlm::new(
            "test-api-key",
            "https://api.openai.com/v1",
            "o3-test",
            "",
            Some(ThinkingEffort::Custom("max".to_string())),
        );

        // When the request is built
        let request = llm
            .build_request(&chat_params_with(vec![Message::user("hi")], vec![]))
            .expect("request builds");

        // Then the effort rides reasoning.effort verbatim
        assert_eq!(request["reasoning"]["effort"], serde_json::json!("max"));
    }

    #[test]
    fn should_omit_reasoning_field_when_thinking_effort_is_none() {
        // Given a Responses-backed LLM with an explicit None effort
        let llm = OpenAIResponsesLlm::new(
            "test-api-key",
            "https://api.openai.com/v1",
            "o3-test",
            "",
            Some(ThinkingEffort::None),
        );

        // When the request is built
        let request = llm
            .build_request(&chat_params_with(vec![Message::user("hi")], vec![]))
            .expect("request builds");

        // Then no reasoning or include fields are sent
        assert!(request.get("reasoning").is_none());
        assert!(request.get("include").is_none());
    }

    #[test]
    fn should_drop_partial_messages_when_converting_input() {
        // Given a Responses-backed LLM and a conversation with a partial
        // assistant message
        let llm = test_llm();
        let params = chat_params_with(
            vec![
                Message::user("hi"),
                Message {
                    role: Role::Assistant,
                    name: None,
                    content: vec![ContentPart::Text {
                        text: "partial".to_string(),
                    }],
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                    partial: true,
                },
            ],
            vec![],
        );

        // When the request is built
        let request = llm.build_request(&params).expect("request builds");

        // Then only the non-partial user message appears in the input array
        let input = request["input"].as_array().expect("input array");
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["role"], "user");
    }

    #[test]
    fn should_reattach_tool_result_images_when_converting_tool_messages() {
        // Given a conversation whose tool result carries text and an image part
        let llm = test_llm();
        let params = chat_params_with(
            vec![
                Message::user("describe the image"),
                Message {
                    role: Role::Tool,
                    name: None,
                    content: vec![
                        ContentPart::Text {
                            text: "<image path=\"/tmp/cat.png\">".to_string(),
                        },
                        ContentPart::ImageUrl {
                            image_url: ImageUrlData {
                                url: "data:image/png;base64,iVBORw0KGgo=".to_string(),
                                detail: None,
                                id: None,
                            },
                            detail: None,
                        },
                        ContentPart::Text {
                            text: "</image>".to_string(),
                        },
                    ],
                    tool_calls: Vec::new(),
                    tool_call_id: Some("call_1".to_string()),
                    partial: false,
                },
                Message::user("thanks"),
            ],
            vec![],
        );

        // When the Responses request is built
        let request = llm.build_request(&params).expect("request builds");
        let items = request["input"].as_array().expect("input array");
        assert_eq!(
            items.len(),
            4,
            "user, function_call_output, reattach, thanks"
        );

        // Then the leading user message is unchanged
        assert_eq!(
            items[0],
            json!({
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": "describe the image" }],
            })
        );

        // And the function_call_output item keeps only its text content
        assert_eq!(
            items[1],
            json!({
                "type": "function_call_output",
                "call_id": "call_1",
                "output": "<image path=\"/tmp/cat.png\">\n</image>",
            })
        );

        // And the image is reattached as a follow-up user message item
        assert_eq!(
            items[2],
            json!({
                "type": "message",
                "role": "user",
                "content": [
                    { "type": "input_text", "text": "Attached media from tool result:" },
                    {
                        "type": "input_image",
                        "detail": "auto",
                        "image_url": "data:image/png;base64,iVBORw0KGgo="
                    },
                ],
            })
        );

        // And the following user message stays in place after the reattach
        assert_eq!(
            items[3],
            json!({
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": "thanks" }],
            })
        );
    }

    #[tokio::test]
    async fn should_stream_text_and_trailing_usage_when_responses_api_returns_sse() {
        // Given a mock OpenAI Responses endpoint streaming text deltas,
        // a completed event, and usage
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(
                        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hello\"}\n\n\
                         data: {\"type\":\"response.output_text.delta\",\"delta\":\" world\"}\n\n\
                         data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":12,\"output_tokens\":7,\"input_tokens_details\":{\"cached_tokens\":2}}}}\n\n",
                    ),
            )
            .mount(&server)
            .await;

        // And a Responses-backed LLM pointed at the mock
        let llm = OpenAIResponsesLlm::new(
            "test-api-key",
            format!("{}/v1", server.uri()),
            "o3-test",
            "",
            None,
        );

        // When the chat stream is consumed to EOF
        let mut stream = llm
            .chat(chat_params_with(vec![Message::user("hi")], vec![]))
            .expect("chat starts");
        let events = drain(&mut stream).await;

        // Then deltas are forwarded, the accumulated text is flushed as a
        // block-level part, the Done carries the mapped stop reason, and the
        // usage from the completed event is forwarded
        let tags: Vec<String> = events.iter().map(event_tag).collect();
        assert_eq!(
            tags,
            [
                "text_delta:Hello",
                "text_delta: world",
                "text_part:Hello world",
                "done:completed",
                "usage:10:7:2:0",
            ]
        );
    }

    #[tokio::test]
    async fn should_emit_tool_call_parts_and_tool_call_when_responses_streams_tool_use() {
        // Given a mock OpenAI Responses endpoint streaming a tool call
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(
                        "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"item_1\",\"call_id\":\"call_1\",\"name\":\"Read\",\"arguments\":\"\"}}\n\n\
                         data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"item_1\",\"delta\":\"{\\\"path\\\":\\\"\"}\n\n\
                         data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"item_1\",\"delta\":\"/tmp\\\"}\"}\n\n\
                         data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"function_call\",\"id\":\"item_1\",\"call_id\":\"call_1\",\"name\":\"Read\",\"arguments\":\"{\\\"path\\\":\\\"/tmp\\\"}\"}],\"usage\":null}}\n\n",
                    ),
            )
            .mount(&server)
            .await;

        // And a Responses-backed LLM pointed at the mock
        let llm = OpenAIResponsesLlm::new(
            "test-api-key",
            format!("{}/v1", server.uri()),
            "o3-test",
            "",
            None,
        );

        // When the chat stream is consumed to EOF
        let mut stream = llm
            .chat(chat_params_with(vec![Message::user("read /tmp")], vec![]))
            .expect("chat starts");
        let events = drain(&mut stream).await;

        // Then argument fragments are surfaced as ToolCallPart events and the
        // assembled call precedes the Done with the completed stop reason
        let tags: Vec<String> = events.iter().map(event_tag).collect();
        assert!(tags[..2].contains(&"tool_call_part:0:{\"path\":\"".to_string()));
        assert!(tags[..2].contains(&"tool_call_part:0:/tmp\"}".to_string()));
        assert!(matches!(
            &events[2],
            LlmEvent::ToolCall {
                tool,
                args,
                call_id,
            } if tool == "Read" && args["path"] == "/tmp" && call_id == "call_1"
        ));
        assert_eq!(tags[3], "done:completed");
    }

    #[tokio::test]
    async fn should_emit_error_event_when_responses_provider_returns_error_status() {
        // Given a mock OpenAI Responses endpoint answering 401
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/responses"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_string(r#"{"error":{"message":"Invalid API key"}}"#),
            )
            .mount(&server)
            .await;

        // And a Responses-backed LLM pointed at the mock
        let llm = OpenAIResponsesLlm::new(
            "bad-key",
            format!("{}/v1", server.uri()),
            "o3-test",
            "",
            None,
        );

        // When the chat stream is consumed to EOF
        let mut stream = llm
            .chat(chat_params_with(vec![Message::user("hi")], vec![]))
            .expect("chat starts");
        let events = drain(&mut stream).await;

        // Then exactly one error event carrying the status is emitted
        assert!(matches!(&events[..], [LlmEvent::Error { message }] if message.contains("401")));
    }

    // --- helpers ----------------------------------------------------------

    /// Drain the stream to EOF, returning all events in order.
    async fn drain(stream: &mut LlmEventStream) -> Vec<LlmEvent> {
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        events
    }

    /// Render an event into a compact, comparable tag.
    fn event_tag(event: &LlmEvent) -> String {
        match event {
            LlmEvent::TextDelta { text } => format!("text_delta:{text}"),
            LlmEvent::ThinkDelta { text } => format!("think_delta:{text}"),
            LlmEvent::ToolCallPart {
                index,
                arguments_part,
                ..
            } => format!("tool_call_part:{index}:{arguments_part}"),
            LlmEvent::ToolCall {
                tool,
                args,
                call_id,
            } => format!("tool_call:{tool}:{args}:{call_id}"),
            LlmEvent::TextPart { text } => format!("text_part:{text}"),
            LlmEvent::ThinkPart { text } => format!("think_part:{text}"),
            LlmEvent::Usage {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_creation_tokens,
            } => format!(
                "usage:{input_tokens}:{output_tokens}:{cache_read_tokens}:{cache_creation_tokens}"
            ),
            LlmEvent::Done { stop_reason } => format!("done:{stop_reason}"),
            LlmEvent::Error { message } => format!("error:{message}"),
        }
    }

    #[test]
    fn should_classify_network_and_stream_errors_as_retryable_when_no_status_is_present() {
        let llm = OpenAIResponsesLlm::new("k", "https://example.test", "gpt-x", "sys", None);
        for message in [
            "error sending request: error trying to connect: tcp connect error: Connection refused",
            "error reading SSE stream: error reading a body from connection: connection reset by peer",
            "operation timed out",
        ] {
            assert!(
                llm.is_retryable_error(&ProviderErrorInfo::from_message(message)),
                "'{message}' must be retryable"
            );
        }
    }

    #[test]
    fn should_classify_server_errors_and_throttles_as_retryable_from_text() {
        let llm = OpenAIResponsesLlm::new("k", "https://example.test", "gpt-x", "sys", None);
        assert!(llm.is_retryable_error(&ProviderErrorInfo::from_message(
            "OpenAI Responses API error 503 Service Unavailable: x"
        )));
        assert!(llm.is_retryable_error(&ProviderErrorInfo::from_message(
            "429 Too Many Requests: slow down"
        )));
    }
}
