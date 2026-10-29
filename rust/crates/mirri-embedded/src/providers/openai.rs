//! [`OpenAILlm`] — a [`Llm`](mirri_core::Llm) adapter over the OpenAI Chat
//! Completions API (legacy streaming).
//!
//! The adapter owns the wire-format translation between the engine's
//! canonical [`Message`]/[`ToolDefinition`] shapes and OpenAI's request JSON;
//! [`OpenAILegacyProvider`](mirri_kosong::OpenAILegacyProvider) owns the
//! transport and SSE parsing.

use std::sync::Arc;

use mirri_core::error::EngineResult;
use mirri_core::llm::{Llm, LlmChatParams, LlmEventStream, ModelCapability, ProviderErrorInfo};
use mirri_core::message::{ContentPart, Message, Role, ToolCall};
use mirri_core::profile::ThinkingEffort;
use mirri_core::tool::ToolDefinition;
use mirri_kosong::OpenAILegacyProvider;
use serde_json::{Map, Value, json};

use super::convert::spawn_provider_chat;
use super::tool_result_media::{TOOL_RESULT_MEDIA_PROMPT, tool_result_text_content};

/// An [`Llm`] speaking the OpenAI Chat Completions wire format.
pub struct OpenAILlm {
    /// The underlying HTTP transport, shared with the streaming task.
    provider: Arc<OpenAILegacyProvider>,
    /// The model serving this adapter (e.g. `"gpt-4o"`).
    model: String,
    /// The system prompt, sent as the first `system` message.
    system_prompt: String,
    /// Reasoning-effort configuration; `None` omits `reasoning_effort`.
    thinking_effort: Option<ThinkingEffort>,
    /// Model capability flags.
    capability: ModelCapability,
    /// Dialect flag: emit `ContentPart::VideoUrl` parts as OpenAI-style
    /// `{ "type": "video_url", "video_url": { "url": … } }` content blocks
    /// (T7, F-P8-3). Off by default — the base OpenAI Chat Completions
    /// API has no video part, so the generic adapter keeps skipping them;
    /// dialects that do accept video references (Kimi's `ms://` files-API
    /// references) enable the emission.
    emit_video_url_parts: bool,
    /// The originating config provider id (O-5, ADR-009) — the attribution
    /// a surfaced auth failure carries so credential-status reporting can
    /// name the provider. `None` marks an unattributed adapter (direct
    /// constructions, test code).
    provider_id: Option<String>,
}

impl OpenAILlm {
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
                OpenAILegacyProvider::new(api_key.into(), base_url.into())
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
            emit_video_url_parts: false,
            provider_id: None,
        }
    }

    /// Set the originating config provider id — the attribution key the
    /// surfaced auth failure carries (O-5, ADR-009).
    pub fn with_provider_id(mut self, provider_id: impl Into<String>) -> Self {
        self.provider_id = Some(provider_id.into());
        self
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

    /// Enable video-part emission for dialects whose endpoint accepts
    /// `video_url` content blocks (e.g. Kimi's `ms://` files-API
    /// references), and mark the video input capability accordingly.
    /// Capability knowledge comes only from the catalog/declaration merge
    /// ([`Self::with_media_capability`]); this intrinsic flag just ORs into
    /// the modality flags.
    pub(crate) fn with_video_url_parts(mut self) -> Self {
        self.emit_video_url_parts = true;
        self.capability.video_in = true;
        self
    }

    /// Build the shared OpenAI Chat Completions request body for `params`,
    /// without any thinking-intent field.
    ///
    /// The message/tool translation and streaming fields are the OpenAI wire
    /// common denominator; dialect adapters (e.g. [`KimiLlm`](super::kimi::KimiLlm))
    /// overlay their own thinking parameter on this base.
    pub(crate) fn build_base_request(&self, params: &LlmChatParams) -> EngineResult<Value> {
        let mut request = Map::new();
        request.insert("model".to_string(), Value::String(self.model.clone()));

        request.insert(
            "messages".to_string(),
            Value::Array(self.convert_messages(params)),
        );

        let tools = convert_tools_to_openai(&params.tools);
        if !tools.is_empty() {
            request.insert("tools".to_string(), Value::Array(tools));
        }

        request.insert("stream".to_string(), Value::Bool(true));
        request.insert(
            "stream_options".to_string(),
            json!({ "include_usage": true }),
        );

        // Fork-chain cache routing: the cache root rides the request as the
        // TOP-LEVEL `prompt_cache_key` field — OpenAI Chat Completions and
        // Kimi both accept it there (TS parity: kosong flattens
        // `generationKwargs` into the body, openai-legacy.ts `_resolveRequestKwargs`
        // → `...kwargs`). Only minted sessions with a cache root carry it;
        // fresh/legacy sessions omit the field.
        if let Some(cache_root_id) = &params.cache_root_id {
            request.insert("prompt_cache_key".to_string(), json!(cache_root_id));
        }

        Ok(Value::Object(request))
    }

    /// The shared HTTP transport, for dialect adapters that build their own
    /// request body but reuse the same [`OpenAILegacyProvider`] connection
    /// semantics.
    pub(crate) fn provider(&self) -> &Arc<OpenAILegacyProvider> {
        &self.provider
    }

    /// Build the OpenAI Chat Completions request body for `params`.
    ///
    /// The translation is currently total; the `Result` return keeps the
    /// signature aligned with [`Llm::chat`] and leaves room for fallible
    /// conversions (a failure would surface as an engine provider error).
    fn build_request(&self, params: &LlmChatParams) -> EngineResult<Value> {
        let mut request = self.build_base_request(params)?;

        // o-series style models accept a reasoning effort; other models ignore
        // the field, so it is only sent when configured. Model-declared efforts
        // (e.g. "max") pass through verbatim — the kosong openai-legacy wire
        // behavior (`packages/kosong/src/providers/openai-legacy.ts:564-569`).
        let reasoning_effort = match &self.thinking_effort {
            Some(ThinkingEffort::Low) => Some("low".to_string()),
            Some(ThinkingEffort::Medium) => Some("medium".to_string()),
            Some(ThinkingEffort::High) => Some("high".to_string()),
            Some(ThinkingEffort::Custom(effort)) => Some(effort.clone()),
            Some(ThinkingEffort::None) | None => None,
        };
        if let Some(level) = reasoning_effort
            && let Some(map) = request.as_object_mut()
        {
            map.insert("reasoning_effort".to_string(), Value::String(level));
        }

        Ok(request)
    }

    /// Translate engine [`Message`]s into OpenAI `messages` entries.
    ///
    /// Mapping rules:
    /// - The adapter's own system prompt is prepended as the first `system`
    ///   message; inline `System` messages are kept in place.
    /// - `User` messages become a plain string when they carry a single text
    ///   part, or a content-block array otherwise.
    /// - `Assistant` messages carry `content` (joined text) and, when
    ///   present, `tool_calls` in OpenAI shape.
    /// - `Tool` messages (and `tool_call_id`-carrying `User` messages)
    ///   become `{ role: "tool", content, tool_call_id }`.
    /// - `partial` messages (interrupted mid-stream) are dropped — their
    ///   truncated content would corrupt the conversation history.
    fn convert_messages(&self, params: &LlmChatParams) -> Vec<Value> {
        let mut messages = Vec::new();
        // Tool-result images collected from tool messages; they cannot live
        // inside the tool message itself (many OpenAI-compatible servers
        // reject content arrays there) and are reattached as a follow-up
        // user message instead. Ports kosong `openai-legacy.ts`
        // `convertHistoryMessages`.
        let mut pending_tool_result_media: Vec<Value> = Vec::new();
        if !self.system_prompt.is_empty() {
            messages.push(json!({
                "role": "system",
                "content": &self.system_prompt,
            }));
        }
        for message in &params.messages {
            if message.partial {
                continue;
            }
            if message.tool_call_id.is_none() {
                flush_tool_result_media(&mut messages, &mut pending_tool_result_media);
            }
            match message.role {
                Role::System => {
                    messages.push(json!({
                        "role": "system",
                        "content": message.text_content(),
                    }));
                }
                Role::User => {
                    if let Some(tool_call_id) = &message.tool_call_id {
                        messages.push(openai_tool_message(tool_call_id, message));
                        pending_tool_result_media.extend(tool_result_image_blocks(message));
                    } else {
                        messages.push(openai_user_message(message, self.emit_video_url_parts));
                    }
                }
                Role::Assistant => {
                    let text = message.text_content();
                    if text.is_empty() && message.tool_calls.is_empty() {
                        continue;
                    }
                    let mut entry = Map::new();
                    entry.insert("role".to_string(), Value::String("assistant".to_string()));
                    entry.insert("content".to_string(), Value::String(text));
                    if !message.tool_calls.is_empty() {
                        entry.insert(
                            "tool_calls".to_string(),
                            Value::Array(
                                message
                                    .tool_calls
                                    .iter()
                                    .map(openai_tool_call_entry)
                                    .collect(),
                            ),
                        );
                    }
                    messages.push(Value::Object(entry));
                }
                Role::Tool => {
                    let Some(tool_call_id) = &message.tool_call_id else {
                        continue;
                    };
                    messages.push(openai_tool_message(tool_call_id, message));
                    pending_tool_result_media.extend(tool_result_image_blocks(message));
                }
            }
        }
        flush_tool_result_media(&mut messages, &mut pending_tool_result_media);
        messages
    }
}

impl Llm for OpenAILlm {
    fn provider_id(&self) -> Option<&str> {
        self.provider_id.as_deref()
    }

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

/// A single OpenAI tool call entry: `{ id, type: "function", function: {
/// name, arguments } }` — the arguments stay a raw JSON string (OpenAI's
/// wire format).
fn openai_tool_call_entry(call: &ToolCall) -> Value {
    json!({
        "id": &call.id,
        "type": "function",
        "function": {
            "name": &call.function.name,
            "arguments": &call.function.arguments,
        },
    })
}

/// Image parts of a tool result, in the OpenAI content-block shape — the
/// media that gets reattached as a follow-up user message.
fn tool_result_image_blocks(message: &Message) -> Vec<Value> {
    message
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::ImageUrl { image_url, detail } => {
                let mut image_url_entry = Map::new();
                image_url_entry.insert("url".to_string(), Value::String(image_url.url.clone()));
                if let Some(detail) = detail.clone().or_else(|| image_url.detail.clone()) {
                    image_url_entry.insert("detail".to_string(), Value::String(detail));
                }
                Some(json!({
                    "type": "image_url",
                    "image_url": Value::Object(image_url_entry),
                }))
            }
            _ => None,
        })
        .collect()
}

/// Push the pending tool-result media as a follow-up user message and clear
/// the pending list (ports kosong `openai-legacy.ts`
/// `appendToolResultMediaMessage`).
fn flush_tool_result_media(messages: &mut Vec<Value>, pending: &mut Vec<Value>) {
    if pending.is_empty() {
        return;
    }
    let mut content = vec![json!({ "type": "text", "text": TOOL_RESULT_MEDIA_PROMPT })];
    content.append(pending);
    messages.push(json!({ "role": "user", "content": content }));
}

/// `{ role: "tool", content, tool_call_id }` — the OpenAI shape for a tool
/// execution result. Used for both `Tool` messages and `tool_call_id`-
/// carrying `User` messages.
///
/// The message carries text only: many OpenAI-compatible servers reject
/// content arrays on `role:"tool"` messages, so images are reattached by
/// the caller as a follow-up user message (see `convert_messages`), and
/// audio/video get explicit omission placeholders instead of vanishing.
fn openai_tool_message(tool_call_id: &str, message: &Message) -> Value {
    json!({
        "role": "tool",
        "content": tool_result_text_content(message),
        "tool_call_id": tool_call_id,
    })
}

/// An OpenAI user message: a plain string when the content is a single text
/// part, otherwise a content-block array (`text`/`image_url`, plus
/// `video_url` when the dialect emits video parts — T7, F-P8-3).
/// Unsupported parts are skipped; a placeholder text block guards the empty
/// case.
fn openai_user_message(message: &Message, emit_video_url_parts: bool) -> Value {
    let parts = super::convert::inline_file_attachments(&message.content);
    if let [ContentPart::Text { text }] = parts.as_slice() {
        return json!({ "role": "user", "content": text });
    }

    let mut blocks = Vec::new();
    for part in parts {
        match part {
            ContentPart::Text { text } => blocks.push(json!({ "type": "text", "text": text })),
            ContentPart::ImageUrl { image_url, detail } => {
                let mut image_url_entry = Map::new();
                image_url_entry.insert("url".to_string(), Value::String(image_url.url.clone()));
                if let Some(detail) = detail.clone().or_else(|| image_url.detail.clone()) {
                    image_url_entry.insert("detail".to_string(), Value::String(detail));
                }
                blocks.push(json!({
                    "type": "image_url",
                    "image_url": Value::Object(image_url_entry),
                }));
            }
            // Dialects that accept video references (Kimi's `ms://`
            // files-API ids) get the OpenAI-style nested wire block, the
            // shape the TS `convertContentPart` emits; the plain OpenAI
            // dialect keeps skipping them (no video part on that API).
            ContentPart::VideoUrl { video_url } if emit_video_url_parts => {
                blocks.push(json!({
                    "type": "video_url",
                    "video_url": { "url": video_url },
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
            "type": "text",
            "text": "[content not representable in the OpenAI Chat Completions API]",
        }));
    }
    json!({ "role": "user", "content": blocks })
}

/// Translate [`ToolDefinition`]s into OpenAI function-tool entries, skipping
/// deferred (progressive-disclosure) tools.
fn convert_tools_to_openai(tools: &[ToolDefinition]) -> Vec<Value> {
    tools
        .iter()
        .filter(|tool| !tool.deferred)
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": &tool.name,
                    "description": &tool.description,
                    "parameters": &tool.parameters,
                },
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use futures::StreamExt;
    use mirri_core::LlmEvent;
    use mirri_core::cancel::CancelToken;
    use mirri_core::llm::{LlmChatParams, ProviderErrorInfo};
    use mirri_core::loop_engine::{LiveEvent, LiveEventCallback, chat_with_retry};
    use mirri_core::message::{ImageUrlData, ToolCallFunction};
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

    fn test_llm() -> OpenAILlm {
        OpenAILlm::new(
            "test-api-key",
            "https://api.openai.com/v1",
            "gpt-test",
            "You are a test assistant.",
            None,
        )
    }

    #[test]
    fn should_build_openai_request_when_converting_messages_and_tools() {
        // Given an OpenAI-backed LLM and a full conversation including a
        // inline system message, a multi-part user message, a tool-using
        // assistant message, a tool result, and one deferred and one active
        // tool
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
                    content: vec![ContentPart::Text {
                        text: "Let me read it.".to_string(),
                    }],
                    tool_calls: vec![ToolCall {
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

        // When the OpenAI request is built
        let request = llm.build_request(&params).expect("request builds");

        // Then the top-level fields carry the model, streaming, and usage
        // accounting options
        assert_eq!(request["model"], "gpt-test");
        assert_eq!(request["stream"], true);
        assert_eq!(request["stream_options"], json!({"include_usage": true}));
        assert!(
            request.get("reasoning_effort").is_none(),
            "no reasoning effort without effort"
        );

        // And the adapter's system prompt is the first message, with the
        // inline system message kept in place
        let messages = request["messages"].as_array().expect("messages array");
        assert_eq!(
            messages[0],
            json!({"role": "system", "content": "You are a test assistant."})
        );
        assert_eq!(
            messages[1],
            json!({"role": "system", "content": "Be brief."})
        );

        // And a single-text user message uses the string content form
        assert_eq!(messages[2], json!({"role": "user", "content": "hello"}));

        // And a multi-part user message uses content blocks with image_url
        let multi = &messages[3];
        assert_eq!(multi["role"], "user");
        assert_eq!(multi["content"][0]["type"], "text");
        assert_eq!(multi["content"][1]["type"], "image_url");
        assert_eq!(
            multi["content"][1]["image_url"]["url"],
            "https://example.com/cat.png"
        );
        assert_eq!(multi["content"][1]["image_url"]["detail"], "low");

        // And the assistant message carries content plus OpenAI-shaped
        // tool_calls with the arguments kept as a raw JSON string
        let assistant = &messages[4];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["content"], "Let me read it.");
        assert_eq!(
            assistant["tool_calls"][0],
            json!({
                "id": "call_1",
                "type": "function",
                "function": {"name": "Read", "arguments": r#"{"path":"/tmp"}"#},
            })
        );

        // And the tool result carries the tool_call_id
        assert_eq!(
            messages[5],
            json!({"role": "tool", "content": "file contents", "tool_call_id": "call_1"})
        );

        // And only the non-deferred tool is advertised
        let tools = request["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["function"]["name"], "Read");
        assert_eq!(
            tools[0]["function"]["parameters"],
            json!({"type": "object"})
        );
    }

    #[test]
    fn should_inject_cache_root_id_as_prompt_cache_key_when_params_carry_cache_root() {
        // Given an OpenAI-backed LLM and params whose cache root id (the
        // fork-chain prompt-cache lineage) is set
        let llm = test_llm();
        let mut params = chat_params_with(vec![Message::user("hi")], Vec::new());
        params.cache_root_id = Some("session-root".to_string());

        // When the request is built
        let request = llm.build_request(&params).expect("request builds");

        // Then the cache root rides the TOP-LEVEL prompt_cache_key field —
        // the Chat Completions wire field, flattened from generationKwargs
        // the same way TS kosong does (openai-legacy.ts `...kwargs`)
        assert_eq!(
            request["prompt_cache_key"],
            json!("session-root"),
            "the cache root id must ride the top-level prompt_cache_key, got: {request}"
        );
    }

    #[test]
    fn should_leave_prompt_cache_key_absent_when_params_carry_no_cache_root() {
        // Given an OpenAI-backed LLM and params without a cache root id
        let llm = test_llm();
        let params = chat_params_with(vec![Message::user("hi")], Vec::new());

        // When the request is built
        let request = llm.build_request(&params).expect("request builds");

        // Then no prompt_cache_key is emitted (a fresh/legacy session must
        // not start routing cache keys; strict providers 400 on unknowns)
        assert!(
            request.get("prompt_cache_key").is_none(),
            "no prompt_cache_key must be emitted without a cache root, got: {request}"
        );
    }

    #[test]
    fn should_skip_video_parts_when_the_video_dialect_flag_is_off() {
        // Given a plain OpenAI adapter (no video dialect) and a user message
        // carrying a video reference
        let llm = test_llm();
        let params = chat_params_with(
            vec![Message {
                role: Role::User,
                name: None,
                content: vec![
                    ContentPart::Text {
                        text: "watch".to_string(),
                    },
                    ContentPart::VideoUrl {
                        video_url: "ms://file-abc123".to_string(),
                    },
                ],
                tool_calls: Vec::new(),
                tool_call_id: None,
                partial: false,
            }],
            Vec::new(),
        );

        // When the request is built
        let request = llm.build_request(&params).expect("request builds");

        // Then the video part is skipped (the base OpenAI Chat Completions
        // API has no video part) and the text block survives
        let content = &request["messages"][1]["content"];
        assert!(content.is_array(), "multi-part stays a content array");
        assert_eq!(content.as_array().unwrap().len(), 1, "video part dropped");
        assert_eq!(content[0], json!({"type": "text", "text": "watch"}));
        assert!(
            !llm.capability().video_in,
            "base dialect has no video input"
        );
    }

    #[test]
    fn should_set_reasoning_effort_when_thinking_effort_is_high() {
        // Given an OpenAI-backed LLM with High reasoning effort
        let llm = OpenAILlm::new(
            "test-api-key",
            "https://api.openai.com/v1",
            "gpt-test",
            "",
            Some(ThinkingEffort::High),
        );

        // When the request is built
        let request = llm
            .build_request(&chat_params_with(vec![Message::user("hi")], vec![]))
            .expect("request builds");

        // Then the reasoning_effort field carries the lowercase level
        assert_eq!(request["reasoning_effort"], "high");
    }

    #[test]
    fn should_send_model_declared_efforts_verbatim_as_reasoning_effort() {
        // Given an OpenAI-backed LLM with a model-declared custom effort
        // (e.g. "max" — a level the canonical enum does not spell)
        let llm = OpenAILlm::new(
            "test-api-key",
            "https://api.openai.com/v1",
            "gpt-test",
            "",
            Some(ThinkingEffort::Custom("max".to_string())),
        );

        // When the request is built
        let request = llm
            .build_request(&chat_params_with(vec![Message::user("hi")], vec![]))
            .expect("request builds");

        // Then the effort rides reasoning_effort verbatim
        assert_eq!(request["reasoning_effort"], serde_json::json!("max"));
    }

    #[test]
    fn should_omit_reasoning_effort_when_thinking_effort_is_none() {
        // Given an OpenAI-backed LLM with an explicit None effort
        let llm = OpenAILlm::new(
            "test-api-key",
            "https://api.openai.com/v1",
            "gpt-test",
            "",
            Some(ThinkingEffort::None),
        );

        // When the request is built
        let request = llm
            .build_request(&chat_params_with(vec![Message::user("hi")], vec![]))
            .expect("request builds");

        // Then no reasoning_effort field is sent
        assert!(request.get("reasoning_effort").is_none());
    }

    #[tokio::test]
    async fn should_stream_text_and_trailing_usage_when_openai_returns_sse_response() {
        // Given a mock OpenAI endpoint streaming text chunks, a finish_reason
        // chunk, and a trailing usage chunk (the realistic
        // stream_options.include_usage ordering)
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(
                        "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n\
                         data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world\"},\"finish_reason\":null}]}\n\n\
                         data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                         data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":7,\"prompt_tokens_details\":{\"cached_tokens\":2}}}\n\n\
                         data: [DONE]\n\n",
                    ),
            )
            .mount(&server)
            .await;

        // And an OpenAI-backed LLM pointed at the mock
        let llm = OpenAILlm::new(
            "test-api-key",
            format!("{}/v1", server.uri()),
            "gpt-test",
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
        // trailing usage chunk (after Done) still reaches the consumer
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
    async fn should_emit_tool_call_parts_and_tool_call_when_openai_streams_tool_use() {
        // Given a mock OpenAI endpoint streaming a fragmented tool call
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(
                        "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"type\":\"function\",\"function\":{\"name\":\"Read\",\"arguments\":\"\"}}]},\"finish_reason\":null}]}\n\n\
                         data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\":\\\"\"}}]},\"finish_reason\":null}]}\n\n\
                         data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"/tmp\\\"}\"}}]},\"finish_reason\":null}]}\n\n\
                         data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
                         data: [DONE]\n\n",
                    ),
            )
            .mount(&server)
            .await;

        // And an OpenAI-backed LLM pointed at the mock
        let llm = OpenAILlm::new(
            "test-api-key",
            format!("{}/v1", server.uri()),
            "gpt-test",
            "",
            None,
        );

        // When the chat stream is consumed to EOF
        let mut stream = llm
            .chat(chat_params_with(vec![Message::user("read /tmp")], vec![]))
            .expect("chat starts");
        let events = drain(&mut stream).await;

        // Then argument fragments are surfaced as ToolCallPart events and the
        // assembled call precedes the Done with the tool_calls stop reason
        let tags: Vec<String> = events.iter().map(event_tag).collect();
        assert_eq!(
            tags[..2],
            ["tool_call_part:0:{\"path\":\"", "tool_call_part:0:/tmp\"}"]
        );
        assert!(matches!(
            &events[2],
            LlmEvent::ToolCall {
                tool,
                args,
                call_id,
            } if tool == "Read" && args["path"] == "/tmp" && call_id == "call_1"
        ));
        assert_eq!(tags[3], "done:tool_calls");
    }

    #[tokio::test]
    async fn should_emit_error_event_when_openai_provider_returns_error_status() {
        // Given a mock OpenAI endpoint answering 401
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_string(r#"{"error":{"message":"Invalid API key"}}"#),
            )
            .mount(&server)
            .await;

        // And an OpenAI-backed LLM pointed at the mock
        let llm = OpenAILlm::new(
            "bad-key",
            format!("{}/v1", server.uri()),
            "gpt-test",
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

    /// Render an event into a compact, comparable tag. `LlmEvent` does not
    /// implement `PartialEq`, so exact-sequence assertions compare tags.
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
            } => {
                format!("tool_call:{tool}:{args}:{call_id}")
            }
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
    fn should_inline_text_file_attachments_as_enveloped_text_when_user_message_carries_a_file() {
        // Given a user message with a text attachment alongside its prompt
        let mut message = Message::user("What is in the file?");
        message.content.push(ContentPart::File {
            file_id: None,
            data: Some("data:text/plain;base64,aGVsbG8gYXR0YWNobWVudA==".to_string()),
            name: Some("notes.txt".to_string()),
            media_type: Some("text/plain".to_string()),
            size: Some(15),
        });
        let llm = OpenAILlm::new(
            "test-api-key",
            "https://api.openai.com/v1",
            "gpt-test",
            "",
            None,
        );

        // When the request is built
        let request = llm
            .build_request(&chat_params_with(vec![message], vec![]))
            .expect("request builds");

        // Then the attachment travels as an enveloped text block the model
        // can read, not a dropped file part
        let blocks = request["messages"][0]["content"]
            .as_array()
            .expect("blocks");
        let text: String = blocks
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("<attachment name=\"notes.txt\" media-type=\"text/plain\">\nhello attachment\n</attachment>"),
            "got: {text}"
        );
    }

    #[test]
    fn should_placeholder_binary_file_attachments_when_the_media_type_is_not_textual() {
        // Given a user message with a binary attachment
        let mut message = Message::user("Describe the image.");
        message.content.push(ContentPart::File {
            file_id: None,
            data: Some("data:image/png;base64,iVBORw0KGgo=".to_string()),
            name: Some("chart.png".to_string()),
            media_type: Some("image/png".to_string()),
            size: Some(9),
        });
        let llm = OpenAILlm::new(
            "test-api-key",
            "https://api.openai.com/v1",
            "gpt-test",
            "",
            None,
        );

        // When the request is built
        let request = llm
            .build_request(&chat_params_with(vec![message], vec![]))
            .expect("request builds");

        // Then the binary attachment is a descriptor placeholder, so the
        // model at least knows it exists
        let blocks = request["messages"][0]["content"]
            .as_array()
            .expect("blocks");
        let text: String = blocks
            .iter()
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains(
                "[attachment name=\"chart.png\": binary content (image/png, 9 bytes) not inlined]"
            ),
            "got: {text}"
        );
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

        // When the OpenAI request is built
        let request = llm.build_request(&params).expect("request builds");
        let messages = request["messages"].as_array().expect("messages array");

        // Then the tool message keeps only its text content
        assert_eq!(
            messages[2],
            json!({
                "role": "tool",
                "content": "<image path=\"/tmp/cat.png\">\n</image>",
                "tool_call_id": "call_1",
            })
        );

        // And the image is reattached as a follow-up user message
        assert_eq!(
            messages[3],
            json!({
                "role": "user",
                "content": [
                    { "type": "text", "text": "Attached media from tool result:" },
                    {
                        "type": "image_url",
                        "image_url": { "url": "data:image/png;base64,iVBORw0KGgo=" }
                    },
                ],
            })
        );

        // And the following user message stays in place after the reattach
        assert_eq!(messages[4], json!({ "role": "user", "content": "thanks" }));
    }

    #[test]
    fn should_note_omitted_audio_and_video_when_tool_result_carries_them() {
        // Given a tool result carrying text, audio, and video parts
        let llm = test_llm();
        let params = chat_params_with(
            vec![Message {
                role: Role::Tool,
                name: None,
                content: vec![
                    ContentPart::Text {
                        text: "done".to_string(),
                    },
                    ContentPart::AudioUrl {
                        audio_url: "data:audio/mpeg;base64,AAA".to_string(),
                    },
                    ContentPart::VideoUrl {
                        video_url: "kimi-file://vid".to_string(),
                    },
                ],
                tool_calls: Vec::new(),
                tool_call_id: Some("call_9".to_string()),
                partial: false,
            }],
            vec![],
        );

        // When the OpenAI request is built
        let request = llm.build_request(&params).expect("request builds");
        let messages = request["messages"].as_array().expect("messages array");

        // Then the tool message text carries the omission placeholders and no
        // reattached media message follows (audio/video cannot be reattached
        // on the Chat Completions API)
        assert_eq!(
            messages[1],
            json!({
                "role": "tool",
                "content": "done\n(audio omitted: not supported by this provider)\n(video omitted: not supported by this provider)",
                "tool_call_id": "call_9",
            })
        );
        assert_eq!(messages.len(), 2, "no reattach message for non-image media");
    }

    #[test]
    fn should_classify_network_and_stream_errors_as_retryable_when_no_status_is_present() {
        let llm = OpenAILlm::new("k", "https://example.test", "m", "sys", None);
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
        let llm = OpenAILlm::new("k", "https://example.test", "m", "sys", None);
        assert!(llm.is_retryable_error(&ProviderErrorInfo::from_message(
            "OpenAI API error 502 Bad Gateway: upstream"
        )));
        assert!(llm.is_retryable_error(&ProviderErrorInfo::from_message(
            "429 Too Many Requests: slow down"
        )));
    }

    #[test]
    fn should_not_classify_auth_failures_as_retryable() {
        let llm = OpenAILlm::new("k", "https://example.test", "m", "sys", None);
        assert!(!llm.is_retryable_error(&ProviderErrorInfo::from_message(
            "OpenAI API error 401 Unauthorized: bad key"
        )));
        assert!(!llm.is_retryable_error(&ProviderErrorInfo::from_message(
            "403 Forbidden: invalid api key"
        )));
    }

    /// A minimal SSE body that yields one text part and a completed stop.
    const RETRY_SSE_BODY: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n\
data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n";

    #[tokio::test]
    async fn should_retry_and_recover_when_provider_returns_500_then_200() {
        // Given a provider that fails once with a 500 then streams normally.
        // wiremock serves requests by the order mocks were mounted: the
        // one-shot 500 is mounted first so it answers exactly the first
        // request (`up_to_n_times(1)`), then falls through to the mounted
        // 200 success mock for the retry.
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(500).set_body_string("internal"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(RETRY_SSE_BODY.to_string()),
            )
            .mount(&server)
            .await;

        let llm = OpenAILlm::new("k", format!("{}/v1", server.uri()), "m", "sys", None);
        let retrying = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&retrying);
        let callback: LiveEventCallback = Arc::new(move |event| {
            if matches!(event, LiveEvent::StepRetrying { .. }) {
                counter.fetch_add(1, Ordering::SeqCst);
            }
        });

        // When a chat is attempted with a retry budget
        let result = chat_with_retry(
            &llm,
            LlmChatParams {
                messages: Vec::new(),
                tools: Vec::new(),
                cancel: CancelToken::new(),
                request_log_fields: None,
                cache_root_id: None,
            },
            3,
            Some(&callback),
            "turn-retry-test",
            1,
            None,
        )
        .await;

        // Then the 500 is retried exactly once and the turn recovers
        assert!(result.is_ok(), "chat must recover after a transient 500");
        assert_eq!(retrying.load(Ordering::SeqCst), 1);
        assert_eq!(result.unwrap().response.text, "hi");
    }

    #[tokio::test]
    async fn should_retry_connection_refused_and_surface_connection_error_when_exhausted() {
        // Given a provider endpoint that refuses connections outright
        // (TCP port 1 on loopback rejects near-instantly)
        let llm = OpenAILlm::new("k", "http://127.0.0.1:1", "m", "sys", None);
        let retrying = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&retrying);
        let callback: LiveEventCallback = Arc::new(move |event| {
            if matches!(event, LiveEvent::StepRetrying { .. }) {
                counter.fetch_add(1, Ordering::SeqCst);
            }
        });

        // When a chat is attempted with a budget of two attempts
        let result = chat_with_retry(
            &llm,
            LlmChatParams {
                messages: Vec::new(),
                tools: Vec::new(),
                cancel: CancelToken::new(),
                request_log_fields: None,
                cache_root_id: None,
            },
            2,
            Some(&callback),
            "turn-refused-test",
            1,
            None,
        )
        .await;

        // Then the transport failure was retried (once) and the surfaced
        // error carries the connection-error domain code
        assert!(result.is_err());
        assert_eq!(retrying.load(Ordering::SeqCst), 1);
        match result.unwrap_err() {
            mirri_core::error::EngineError::ProviderConnectionError(_) => {}
            other => panic!("expected ProviderConnectionError, got {other:?}"),
        }
    }
}
