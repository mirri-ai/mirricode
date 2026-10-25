//! [`GoogleLlm`] — a [`Llm`](mirri_core::Llm) adapter over the Google GenAI
//! (Gemini) `generateContentStream` API.
//!
//! The adapter owns the wire-format translation between the engine's
//! canonical [`Message`]/[`ToolDefinition`] shapes and Google's request JSON
//! (the TS reference: `packages/kosong/src/providers/google-genai.ts`);
//! [`GoogleGenAIProvider`](mirri_kosong::GoogleGenAIProvider) owns the
//! transport and SSE parsing.
//!
//! Mapping rules (Google's shape, not OpenAI's):
//! - The adapter's own system prompt becomes the top-level
//!   `systemInstruction`; historical `System` messages are preserved as
//!   `<system>`-wrapped user turns (Google's `Content.role` only accepts
//!   `user`/`model`).
//! - `Assistant` messages become `model` turns; tool calls become complete
//!   `functionCall` parts with the arguments parsed into a JSON object
//!   (Google sends and receives them pre-parsed, never as strings).
//! - `Tool` results become `functionResponse` parts under a single `user`
//!   turn, packed in the order of the preceding assistant's tool calls.
//! - Consecutive `user` turns are merged — Gemini requires strictly
//!   alternating user/model turns, and adjacent user content arises after
//!   compaction and when a user turn follows a tool result.

use std::sync::Arc;

use mirri_core::error::{EngineError, EngineResult};
use mirri_core::llm::{Llm, LlmChatParams, LlmEventStream, ModelCapability, ProviderErrorInfo};
use mirri_core::message::{ContentPart, Message, Role};
use mirri_core::profile::ThinkingEffort;
use mirri_core::tool::ToolDefinition;
use mirri_kosong::GoogleGenAIProvider;
use serde_json::{Map, Value, json};

use std::collections::HashMap;

use super::convert::spawn_provider_chat;

/// An [`Llm`] speaking the Google GenAI wire format.
pub struct GoogleLlm {
    /// The underlying HTTP transport, shared with the streaming task.
    provider: Arc<GoogleGenAIProvider>,
    /// The model serving this adapter (e.g. `"gemini-2.5-pro"`).
    model: String,
    /// The system prompt, sent as the top-level `systemInstruction`.
    system_prompt: String,
    /// Reasoning-effort configuration; `Some(Low..High)` requests thought
    /// streaming, `None`/`Some(None)` leaves the model's default.
    thinking_effort: Option<ThinkingEffort>,
    /// Model capability flags.
    capability: ModelCapability,
}

impl GoogleLlm {
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
                GoogleGenAIProvider::new(api_key.into(), base_url.into())
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

    /// Build the GenAI `generateContentStream` request body for `params`.
    ///
    /// The translation is fallible where the engine shape cannot be honestly
    /// represented (a tool-call arguments string that is not a JSON object —
    /// Google requires a pre-parsed args object).
    fn build_request(&self, params: &LlmChatParams) -> EngineResult<Value> {
        let mut request = Map::new();
        request.insert("model".to_string(), Value::String(self.model.clone()));

        if !self.system_prompt.is_empty() {
            request.insert(
                "systemInstruction".to_string(),
                json!({ "parts": [{ "text": self.system_prompt }] }),
            );
        }

        request.insert(
            "contents".to_string(),
            Value::Array(convert_messages(&params.messages)?),
        );

        let declarations = convert_tools_to_genai(&params.tools);
        if !declarations.is_empty() {
            request.insert(
                "tools".to_string(),
                json!([{ "functionDeclarations": declarations }]),
            );
        }

        // Reasoning-effort models: stream the thought parts alongside the
        // answer. No explicit thinkingBudget — the model's own default per
        // level applies, so the adapter stays honest about what it knows
        // (deviation D3 applies to model-declared custom efforts too).
        if matches!(
            &self.thinking_effort,
            Some(
                ThinkingEffort::Low
                    | ThinkingEffort::Medium
                    | ThinkingEffort::High
                    | ThinkingEffort::Custom(_)
            )
        ) {
            request.insert(
                "generationConfig".to_string(),
                json!({ "thinkingConfig": { "includeThoughts": true } }),
            );
        }

        Ok(Value::Object(request))
    }
}

impl Llm for GoogleLlm {
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

/// Translate engine [`Message`]s into GenAI `contents` entries.
///
/// The walk mirrors the TS provider: an assistant turn carrying tool calls
/// registers its `id → name` pairs, consumes the consecutive tool-result
/// messages that follow, and packs them — ordered by the tool calls — into
/// one `user` turn of `functionResponse` parts. Consecutive user turns are
/// merged (Gemini requires strictly alternating roles).
fn convert_messages(messages: &[Message]) -> EngineResult<Vec<Value>> {
    let mut contents: Vec<Value> = Vec::new();
    let mut tool_name_by_id: HashMap<String, String> = HashMap::new();

    let mut i = 0;
    while let Some(message) = messages.get(i) {
        if message.partial {
            i += 1;
            continue;
        }
        match message.role {
            Role::System => {
                // Google's `Content.role` only accepts "user"/"model", so a
                // historical system message (session restore, cross-provider
                // migration) is preserved as a `<system>`-wrapped user turn —
                // the TS provider's behavior. The adapter's own system
                // prompt flows into `systemInstruction` separately.
                let text = message.text_content();
                if !text.is_empty() {
                    push_user_content(
                        &mut contents,
                        vec![json!({ "text": format!("<system>{text}</system>") })],
                    );
                }
                i += 1;
            }
            Role::Assistant => {
                if message.tool_calls.is_empty() {
                    let parts = content_parts(message);
                    if !parts.is_empty() {
                        contents.push(json!({ "role": "model", "parts": parts }));
                    }
                    i += 1;
                    continue;
                }

                // The model turn carries its tool calls as complete
                // `functionCall` parts (args parsed into a JSON object).
                let mut parts = content_parts(message);
                for call in &message.tool_calls {
                    let args: Value =
                        serde_json::from_str(&call.function.arguments).map_err(|_| {
                            EngineError::RequestInvalid(format!(
                                "tool call '{}' arguments are not valid JSON",
                                call.function.name
                            ))
                        })?;
                    if !args.is_object() {
                        return Err(EngineError::RequestInvalid(format!(
                            "tool call '{}' arguments must be a JSON object",
                            call.function.name
                        )));
                    }
                    parts.push(json!({
                        "functionCall": { "name": &call.function.name, "args": args }
                    }));
                }
                contents.push(json!({ "role": "model", "parts": parts }));

                // Register the ids and consume the consecutive tool results.
                for call in &message.tool_calls {
                    tool_name_by_id.insert(call.id.clone(), call.function.name.clone());
                }
                let mut j = i + 1;
                let mut tool_messages: Vec<&Message> = Vec::new();
                while let Some(next) = messages.get(j) {
                    if next.partial || !is_tool_result(next) {
                        break;
                    }
                    tool_messages.push(next);
                    j += 1;
                }
                if !tool_messages.is_empty() {
                    // Pack the results in the order of the tool calls they
                    // answer (Gemini expects a matching set of function
                    // responses for the preceding function calls).
                    let mut parts = Vec::new();
                    // Id-keyed pairing only works when the ids uniquely
                    // identify their calls. Replays of sessions recorded
                    // before the dispatcher synthesized ids carry empty or
                    // duplicated ids — an id map would collapse them,
                    // dropping results and misattributing the survivor. Fall
                    // back to positional pairing (arrival order) so every
                    // result reaches the wire.
                    let ids_unique = {
                        let mut seen = std::collections::HashSet::new();
                        message
                            .tool_calls
                            .iter()
                            .all(|c| !c.id.is_empty() && seen.insert(c.id.clone()))
                    };
                    if ids_unique && tool_messages.len() == message.tool_calls.len() {
                        let by_id: HashMap<&str, &Message> = tool_messages
                            .iter()
                            .map(|m| (m.tool_call_id.as_deref().unwrap_or(""), *m))
                            .collect();
                        for call in &message.tool_calls {
                            let Some(result) = by_id.get(call.id.as_str()) else {
                                continue;
                            };
                            parts.extend(tool_response_parts(result, &call.function.name));
                        }
                    } else {
                        for (call, result) in message.tool_calls.iter().zip(tool_messages.iter()) {
                            parts.extend(tool_response_parts(result, &call.function.name));
                        }
                    }
                    // Results beyond the calls (defensive; the engine pairs
                    // them normally) follow in arrival order — an unresolved
                    // name skips the part (an empty `name` would be an
                    // invalid GenAI payload).
                    for result in tool_messages.iter().skip(message.tool_calls.len()) {
                        let name = result
                            .tool_call_id
                            .as_deref()
                            .and_then(|id| tool_name_by_id.get(id))
                            .cloned()
                            .unwrap_or_default();
                        if name.is_empty() {
                            continue;
                        }
                        parts.extend(tool_response_parts(result, &name));
                    }
                    push_user_content(&mut contents, parts);
                    i = j;
                    continue;
                }
                i += 1;
            }
            Role::User if message.tool_call_id.is_some() => {
                // A tool result carried on a user message (the engine's
                // alternate spelling) — same functionResponse treatment. An
                // unresolvable name skips the part (an empty `name` would be
                // an invalid GenAI payload).
                let name = message
                    .tool_call_id
                    .as_deref()
                    .and_then(|id| tool_name_by_id.get(id))
                    .cloned()
                    .unwrap_or_default();
                if !name.is_empty() {
                    push_user_content(&mut contents, tool_response_parts(message, &name));
                }
                i += 1;
            }
            Role::User => {
                let parts = content_parts(message);
                if !parts.is_empty() {
                    push_user_content(&mut contents, parts);
                }
                i += 1;
            }
            Role::Tool => {
                // An orphan tool result (no preceding assistant tool call
                // registered its id) — the id must at least resolve a name;
                // otherwise the part is skipped (an empty `name` would be an
                // invalid GenAI payload).
                let name = message
                    .tool_call_id
                    .as_deref()
                    .and_then(|id| tool_name_by_id.get(id))
                    .cloned()
                    .unwrap_or_default();
                if !name.is_empty() {
                    push_user_content(&mut contents, tool_response_parts(message, &name));
                }
                i += 1;
            }
        }
    }
    Ok(contents)
}

/// Whether a message is a tool result (either spelling: `Tool` role, or a
/// user message carrying `tool_call_id`).
fn is_tool_result(message: &Message) -> bool {
    matches!(message.role, Role::Tool)
        || (message.role == Role::User && message.tool_call_id.is_some())
}

/// The content parts of a message: text parts plus media (images and other
/// data URLs → `inlineData`, remote URLs → `fileData`); think parts are
/// synthetic and skipped, exactly as the TS provider does.
fn content_parts(message: &Message) -> Vec<Value> {
    let mut parts = Vec::new();
    for part in super::convert::inline_file_attachments(&message.content) {
        match part {
            ContentPart::Text { text } if !text.is_empty() => {
                parts.push(json!({ "text": text }));
            }
            ContentPart::ImageUrl { image_url, .. } => {
                parts.push(convert_media_url(&image_url.url, "image/jpeg"));
            }
            ContentPart::AudioUrl { ref audio_url } => {
                parts.push(convert_media_url(audio_url, "audio/mpeg"));
            }
            ContentPart::VideoUrl { ref video_url } => {
                parts.push(convert_media_url(video_url, "video/mp4"));
            }
            // File attachments are inlined before conversion (see
            // `inline_file_attachments`); a leftover File part here means a
            // non-user message carried one — dropped, matching audio/video.
            ContentPart::File { .. } | ContentPart::Text { .. } | ContentPart::Think { .. } => { /* skipped */
            }
        }
    }
    parts
}

/// A tool result's `functionResponse` part plus any media parts the result
/// carries (multimodal tool outputs must not be silently dropped).
fn tool_response_parts(message: &Message, tool_name: &str) -> Vec<Value> {
    let mut parts = Vec::new();
    let mut output = String::new();
    for part in message.content.as_slice() {
        match part {
            ContentPart::Text { text } => output.push_str(text),
            ContentPart::ImageUrl { image_url, .. } => {
                parts.push(convert_media_url(&image_url.url, "image/jpeg"));
            }
            ContentPart::AudioUrl { audio_url } => {
                parts.push(convert_media_url(audio_url, "audio/mpeg"));
            }
            ContentPart::VideoUrl { video_url } => {
                parts.push(convert_media_url(video_url, "video/mp4"));
            }
            // No provider mapping for file attachments yet (P8).
            ContentPart::File { .. } | ContentPart::Think { .. } => { /* skipped */ }
        }
    }
    let mut response = Map::new();
    response.insert("output".to_string(), Value::String(output));
    let mut function_response = Map::new();
    function_response.insert("name".to_string(), Value::String(tool_name.to_string()));
    function_response.insert("response".to_string(), Value::Object(response));
    parts.insert(
        0,
        json!({ "functionResponse": Value::Object(function_response) }),
    );
    parts
}

/// Convert a media URL to a GenAI data part: `data:` URLs become
/// `inlineData` (parsed mime type), anything else `fileData` with the mime
/// guessed from the extension — the TS provider's conversion.
fn convert_media_url(url: &str, fallback_mime: &str) -> Value {
    if let Some(rest) = url.strip_prefix("data:")
        && let Some(comma) = rest.find(',')
    {
        let (meta, data) = rest.split_at(comma);
        let mime = meta.split(';').next().filter(|m| !m.is_empty());
        return json!({
            "inlineData": { "mimeType": mime.unwrap_or(fallback_mime), "data": &data[1..] }
        });
    }
    let mime = guess_mime_from_path(url).unwrap_or(fallback_mime);
    json!({ "fileData": { "fileUri": url, "mimeType": mime } })
}

/// Guess a mime type from a URL's path extension (the TS provider's list).
fn guess_mime_from_path(url: &str) -> Option<&'static str> {
    let path = url
        .split(&['?', '#'][..])
        .next()
        .unwrap_or("")
        .to_lowercase();
    let ext = path.rsplit('.').next()?;
    match ext {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "mp3" | "mpeg" => Some("audio/mpeg"),
        "wav" => Some("audio/wav"),
        "ogg" => Some("audio/ogg"),
        "mp4" => Some("video/mp4"),
        _ => None,
    }
}

/// Push a `user` content, merging into the previous one when it is also a
/// user turn (Gemini requires strictly alternating roles).
///
/// Unlike the TS provider's merge (which keeps a tool-result-only turn
/// separate from a plain user turn), this merges any adjacent user turns —
/// only reachable with a malformed transcript (orphaned tool results), and
/// the merged output at least stays role-alternating.
fn push_user_content(contents: &mut Vec<Value>, parts: Vec<Value>) {
    if parts.is_empty() {
        return;
    }
    if let Some(last) = contents.last_mut()
        && last.get("role").and_then(Value::as_str) == Some("user")
        && let Some(last_parts) = last.get_mut("parts").and_then(Value::as_array_mut)
    {
        last_parts.extend(parts);
        return;
    }
    contents.push(json!({ "role": "user", "parts": parts }));
}

/// Translate [`ToolDefinition`]s into GenAI function declarations, skipping
/// deferred (progressive-disclosure) tools.
fn convert_tools_to_genai(tools: &[ToolDefinition]) -> Vec<Value> {
    tools
        .iter()
        .filter(|tool| !tool.deferred)
        .map(|tool| {
            json!({
                "name": &tool.name,
                "description": &tool.description,
                "parameters": &tool.parameters,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;
    use mirri_core::llm::ProviderErrorInfo;
    use mirri_core::message::{ImageUrlData, ToolCall, ToolCallFunction};
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

    fn test_llm(thinking_effort: Option<ThinkingEffort>) -> GoogleLlm {
        GoogleLlm::new(
            "test-api-key",
            "https://generativelanguage.googleapis.com/v1beta",
            "gemini-test",
            "You are a test assistant.",
            thinking_effort,
        )
    }

    #[test]
    fn should_build_genai_request_when_converting_messages_and_tools() {
        // Given a Google-backed LLM and a full conversation including an
        // inline system message, a multi-part user message, a tool-using
        // assistant turn, its tool result, and one deferred and one active
        // tool
        let llm = test_llm(None);
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
                                url: "data:image/png;base64,aGk=".to_string(),
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

        // When the GenAI request is built
        let request = llm.build_request(&params).expect("request builds");

        // Then the top-level fields carry the model and the system
        // instruction (the adapter's own prompt)
        assert_eq!(request["model"], "gemini-test");
        assert_eq!(
            request["systemInstruction"],
            json!({ "parts": [{ "text": "You are a test assistant." }] })
        );
        assert!(request.get("generationConfig").is_none());

        // And the historical system message is a <system>-wrapped user turn,
        // with the following user text merged into the same turn
        let contents = request["contents"].as_array().expect("contents array");
        assert_eq!(contents.len(), 3, "system+user merged, model, tool-result");
        assert_eq!(
            contents[0]["parts"],
            json!([
                { "text": "<system>Be brief.</system>" },
                { "text": "hello" },
                { "text": "describe" },
                { "inlineData": { "mimeType": "image/png", "data": "aGk=" } },
            ])
        );

        // And the assistant turn is a model turn with the tool call as a
        // complete functionCall part (args parsed into an object)
        let model = &contents[1];
        assert_eq!(model["role"], "model");
        assert_eq!(model["parts"][0]["text"], "Let me read it.");
        assert_eq!(
            model["parts"][1]["functionCall"],
            json!({ "name": "Read", "args": { "path": "/tmp" } })
        );

        // And the tool result is a single user turn with the
        // functionResponse part carrying the resolved tool name
        let tool_turn = &contents[2];
        assert_eq!(tool_turn["role"], "user");
        assert_eq!(tool_turn["parts"][0]["functionResponse"]["name"], "Read");
        assert_eq!(
            tool_turn["parts"][0]["functionResponse"]["response"]["output"],
            "file contents"
        );

        // And only the non-deferred tool is advertised
        let tools = request["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["functionDeclarations"][0]["name"], "Read");
        assert_eq!(
            tools[0]["functionDeclarations"][0]["parameters"],
            json!({"type": "object"})
        );
    }

    #[test]
    fn should_request_thought_streaming_when_thinking_effort_is_set() {
        // Given a Google-backed LLM with High reasoning effort
        let llm = test_llm(Some(ThinkingEffort::High));

        // When the request is built
        let request = llm
            .build_request(&chat_params_with(vec![Message::user("hi")], vec![]))
            .expect("request builds");

        // Then the generation config requests thought streaming
        assert_eq!(
            request["generationConfig"],
            json!({ "thinkingConfig": { "includeThoughts": true } })
        );
    }

    #[test]
    fn should_request_thought_streaming_without_a_budget_when_effort_is_model_declared() {
        // Given a Google-backed LLM with a model-declared custom effort
        // (e.g. "max" — a level the canonical enum does not spell)
        let llm = test_llm(Some(ThinkingEffort::Custom("max".to_string())));

        // When the request is built
        let request = llm
            .build_request(&chat_params_with(vec![Message::user("hi")], vec![]))
            .expect("request builds");

        // A model-declared custom effort is a reasoning level like the others:
        // includeThoughts streams the thought parts, and (deviation D3) no
        // thinkingBudget is invented.
        assert_eq!(
            request["generationConfig"]["thinkingConfig"]["includeThoughts"],
            serde_json::json!(true)
        );
        assert!(
            request["generationConfig"]["thinkingConfig"]
                .get("thinkingBudget")
                .is_none()
        );
    }

    #[test]
    fn should_reject_tool_call_arguments_that_are_not_json_objects() {
        // Given a Google-backed LLM and a history whose assistant tool call
        // carries non-object arguments
        let llm = test_llm(None);
        let params = chat_params_with(
            vec![Message {
                role: Role::Assistant,
                name: None,
                content: Vec::new(),
                tool_calls: vec![ToolCall {
                    id: "call_1".to_string(),
                    call_type: "function".to_string(),
                    function: ToolCallFunction {
                        name: "Read".to_string(),
                        arguments: "not json".to_string(),
                    },
                }],
                tool_call_id: None,
                partial: false,
            }],
            vec![],
        );

        // When the request is built
        let error = llm.build_request(&params).expect_err("request rejected");

        // Then the failure names the offending tool call
        assert!(
            error.to_string().contains("Read"),
            "error mentions the tool: {error}"
        );
    }

    #[test]
    fn should_pack_tool_results_in_tool_call_order_when_multiple_calls_match() {
        // Given a Google-backed LLM and a multi-tool assistant turn whose
        // results arrive in reversed order
        let llm = test_llm(None);
        let params = chat_params_with(
            vec![
                Message {
                    role: Role::Assistant,
                    name: None,
                    content: Vec::new(),
                    tool_calls: vec![
                        ToolCall {
                            id: "call_1".to_string(),
                            call_type: "function".to_string(),
                            function: ToolCallFunction {
                                name: "Read".to_string(),
                                arguments: "{}".to_string(),
                            },
                        },
                        ToolCall {
                            id: "call_2".to_string(),
                            call_type: "function".to_string(),
                            function: ToolCallFunction {
                                name: "Grep".to_string(),
                                arguments: "{}".to_string(),
                            },
                        },
                    ],
                    tool_call_id: None,
                    partial: false,
                },
                Message::tool_result("call_2", "grep output"),
                Message::tool_result("call_1", "read output"),
            ],
            vec![],
        );

        // When the request is built
        let request = llm.build_request(&params).expect("request builds");

        // Then the single user tool-result turn carries the function
        // responses in tool-call order (Read before Grep)
        let contents = request["contents"].as_array().expect("contents array");
        let tool_turn = contents.last().expect("tool result turn");
        assert_eq!(tool_turn["role"], "user");
        let names: Vec<&str> = tool_turn["parts"]
            .as_array()
            .expect("parts")
            .iter()
            .map(|p| p["functionResponse"]["name"].as_str().expect("name"))
            .collect();
        assert_eq!(names, ["Read", "Grep"]);
    }

    #[test]
    fn should_pair_tool_results_positionally_when_call_ids_are_empty() {
        // Given a Google-backed LLM and a multi-tool assistant turn whose
        // calls and results all carry Google's empty call id (the shape the
        // provider emitted before id synthesis; still reachable from
        // replayed transcripts)
        let llm = test_llm(None);
        let params = chat_params_with(
            vec![
                Message {
                    role: Role::Assistant,
                    name: None,
                    content: Vec::new(),
                    tool_calls: vec![
                        ToolCall {
                            id: String::new(),
                            call_type: "function".to_string(),
                            function: ToolCallFunction {
                                name: "Read".to_string(),
                                arguments: "{}".to_string(),
                            },
                        },
                        ToolCall {
                            id: String::new(),
                            call_type: "function".to_string(),
                            function: ToolCallFunction {
                                name: "Grep".to_string(),
                                arguments: "{}".to_string(),
                            },
                        },
                    ],
                    tool_call_id: None,
                    partial: false,
                },
                Message::tool_result("", "read output"),
                Message::tool_result("", "grep output"),
            ],
            vec![],
        );

        // When the request is built
        let request = llm.build_request(&params).expect("request builds");

        // Then each call receives its own result, in arrival order (no
        // dropped output, no duplicated output)
        let tool_turn = request["contents"]
            .as_array()
            .expect("contents array")
            .last()
            .expect("tool result turn");
        let names: Vec<&str> = tool_turn["parts"]
            .as_array()
            .expect("parts")
            .iter()
            .map(|p| p["functionResponse"]["name"].as_str().expect("name"))
            .collect();
        assert_eq!(names, ["Read", "Grep"]);
        let outputs: Vec<&str> = tool_turn["parts"]
            .as_array()
            .expect("parts")
            .iter()
            .map(|p| {
                p["functionResponse"]["response"]["output"]
                    .as_str()
                    .expect("output")
            })
            .collect();
        assert_eq!(outputs, ["read output", "grep output"]);
    }

    #[tokio::test]
    async fn should_stream_text_thought_tool_call_and_usage_when_google_returns_sse_response() {
        // Given a mock Google GenAI endpoint streaming a thought part, a
        // text part, a function call, the STOP finish reason, and usage
        // metadata (the realistic final-chunk ordering)
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1beta/models/gemini-test:generateContentStream"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(
                        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Thinking...\",\"thought\":true}]}}]}\n\n\
                         data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hello\"}]}}]}\n\n\
                         data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"Read\",\"args\":{\"path\":\"/tmp\"}}}]}}]}\n\n\
                         data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\" world\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":12,\"cachedContentTokenCount\":2,\"candidatesTokenCount\":7}}\n\n",
                    ),
            )
            .mount(&server)
            .await;

        // And a Google-backed LLM pointed at the mock
        let llm = GoogleLlm::new(
            "test-api-key",
            format!("{}/v1beta", server.uri()),
            "gemini-test",
            "",
            None,
        );

        // When the chat stream is consumed to EOF
        let mut stream = llm
            .chat(chat_params_with(vec![Message::user("hi")], vec![]))
            .expect("chat starts");
        let events = drain(&mut stream).await;

        // Then thought and text deltas stream, the accumulated parts flush
        // at the tool call and again at Done (text first, think second — the
        // shared pump's block-level ordering), the complete tool call
        // precedes the mapped stop reason, and the usage accounts for
        // cached tokens
        let tags: Vec<String> = events.iter().map(event_tag).collect();
        // The tool call carries a synthesized id (a process-global counter —
        // the exact number is not deterministic under parallel tests), so its
        // tag is asserted by prefix.
        assert_eq!(
            tags[..4],
            [
                "think_delta:Thinking...",
                "text_delta:Hello",
                "text_part:Hello",
                "think_part:Thinking...",
            ]
        );
        assert!(
            tags[4].starts_with("tool_call:Read:{\"path\":\"/tmp\"}:Read_"),
            "got: {}",
            tags[4]
        );
        assert_eq!(
            tags[5..],
            [
                "text_delta: world",
                "text_part: world",
                "done:completed",
                "usage:10:7:2:0",
            ]
        );
    }

    #[tokio::test]
    async fn should_emit_error_event_when_google_provider_returns_error_status() {
        // Given a mock Google GenAI endpoint answering 401
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1beta/models/gemini-test:generateContentStream"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_string(r#"{"error":{"message":"Invalid API key"}}"#),
            )
            .mount(&server)
            .await;

        // And a Google-backed LLM pointed at the mock
        let llm = GoogleLlm::new(
            "bad-key",
            format!("{}/v1beta", server.uri()),
            "gemini-test",
            "",
            None,
        );

        // When the chat stream is consumed to EOF
        let mut stream = llm
            .chat(chat_params_with(vec![Message::user("hi")], vec![]))
            .expect("chat starts");
        let events = drain(&mut stream).await;

        // Then exactly one error event carrying the status is emitted
        assert!(
            matches!(&events[..], [LlmEvent::Error { message }] if message.contains("401")),
            "events: {events:?}"
        );
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
    fn should_classify_network_and_stream_errors_as_retryable_when_no_status_is_present() {
        let llm = GoogleLlm::new("k", "https://example.test", "gemini-x", "sys", None);
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
        let llm = GoogleLlm::new("k", "https://example.test", "gemini-x", "sys", None);
        assert!(llm.is_retryable_error(&ProviderErrorInfo::from_message(
            "Google GenAI API error 503 Service Unavailable: x"
        )));
        assert!(llm.is_retryable_error(&ProviderErrorInfo::from_message(
            "429 Too Many Requests: slow down"
        )));
    }
}
