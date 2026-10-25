//! [`KimiLlm`] — a [`Llm`](mirri_core::Llm) adapter over the Kimi (Moonshot)
//! API, an OpenAI Chat Completions wire dialect.
//!
//! Kimi speaks the OpenAI wire format except for the thinking intent: instead
//! of OpenAI's `reasoning_effort` string, Kimi expects a top-level `thinking`
//! object (`{ "type": "disabled" }` / `{ "type": "enabled", "effort": ... }`).
//! This adapter therefore delegates all shared OpenAI wire translation
//! (messages, tools, streaming fields) to an inner [`OpenAILlm`] constructed
//! with no thinking effort, and overlays the Kimi thinking dialect on the
//! request; the HTTP transport (`mirri_kosong::OpenAILegacyProvider`) is
//! shared as-is.
//!
//! Mirrors the TypeScript registration in
//! `packages/agent-core-v2/src/kosong/provider/providers/kimi/kimi.contrib.ts`
//! (`(kimi, openai)` over the OpenAI base protocol), scoped to what the rust
//! engine expresses today: the thinking dialect, video input, and the
//! endpoint defaults (`MIRRI_API_KEY` / `MIRRI_BASE_URL`, default
//! `https://api.mirricode.com/coding/v1`, joined with `/chat/completions`).
//! Video file input follows the TS `kimi-files.ts` semantics: the caller
//! uploads through [`KimiFiles`](super::kimi_files::KimiFiles) (the Kimi
//! files API, `POST /v1/files` with `purpose=video`) and places the returned
//! `ms://<id>` reference in the message; this adapter emits it as the
//! nested `video_url` content block the Kimi endpoint expects. Vendor
//! specific behaviors with no rust engine counterpart yet
//! (`max_completion_tokens` ceiling takeover, `$`-prefixed builtin tools,
//! prompt cache keys) are deliberately absent; the quota-exhausted 429
//! classification is NOT a gap — it landed in the shared provider-error
//! layer (`mirri-core/src/llm.rs`, P8b-1).

use std::sync::Arc;

use mirri_core::error::EngineResult;
use mirri_core::llm::{Llm, LlmChatParams, LlmEventStream, ModelCapability, ProviderErrorInfo};
use mirri_core::profile::ThinkingEffort;
use serde_json::{Value, json};

use super::convert::spawn_provider_chat;
use super::openai::OpenAILlm;

/// An [`Llm`] speaking the Kimi (Moonshot) wire dialect over the OpenAI
/// Chat Completions transport.
pub struct KimiLlm {
    /// The OpenAI-dialect conversion and transport engine this adapter
    /// delegates to. Constructed with no thinking effort — the Kimi
    /// dialect owns the thinking field.
    base: OpenAILlm,
    /// Thinking-intent configuration; `None` omits the `thinking` object.
    thinking_effort: Option<ThinkingEffort>,
}

impl KimiLlm {
    /// Construct an adapter for the given API key, base URL (no trailing
    /// slash, carrying the version segment e.g. `/v1`; `/chat/completions`
    /// is joined onto it), model, and system prompt.
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
            // Kimi accepts `video_url` content blocks (the `ms://` files-API
            // references — T7, F-P8-3), so the OpenAI base conversion emits
            // them instead of skipping the parts.
            base: OpenAILlm::with_headers(api_key, base_url, model, system_prompt, None, headers)
                .with_video_url_parts(),
            thinking_effort,
        }
    }

    /// Merge catalog-resolved media capabilities into the intrinsic set
    /// (multimodal spec US3.1: catalog resolution merges into the intrinsic
    /// set (OR); unresolved capabilities default to unsupported, ADR-003
    /// revision). Delegates to the inner OpenAI base, which owns the
    /// capability view this adapter reports.
    pub fn with_media_capability(self, resolved: ModelCapability) -> Self {
        Self {
            base: self.base.with_media_capability(resolved),
            thinking_effort: self.thinking_effort,
        }
    }

    /// Build the Kimi Chat Completions request body for `params`: the shared
    /// OpenAI base request plus the Kimi `thinking` object.
    fn build_request(&self, params: &LlmChatParams) -> EngineResult<Value> {
        let mut request = self.base.build_base_request(params)?;
        match &self.thinking_effort {
            Some(ThinkingEffort::None) => {
                insert_thinking(&mut request, json!({ "type": "disabled" }));
            }
            Some(ThinkingEffort::Low) => {
                insert_thinking(&mut request, json!({ "type": "enabled", "effort": "low" }));
            }
            Some(ThinkingEffort::Medium) => {
                insert_thinking(
                    &mut request,
                    json!({ "type": "enabled", "effort": "medium" }),
                );
            }
            Some(ThinkingEffort::High) => {
                insert_thinking(&mut request, json!({ "type": "enabled", "effort": "high" }));
            }
            Some(ThinkingEffort::Custom(effort)) => {
                // Model-declared efforts ride the Kimi thinking object verbatim —
                // the same enabled+effort shape the canonical levels use.
                insert_thinking(&mut request, json!({ "type": "enabled", "effort": effort }));
            }
            None => {}
        }
        Ok(request)
    }
}

/// Insert the Kimi `thinking` object as a top-level request field.
fn insert_thinking(request: &mut Value, thinking: Value) {
    if let Value::Object(map) = request {
        map.insert("thinking".to_string(), thinking);
    }
}

impl Llm for KimiLlm {
    fn system_prompt(&self) -> &str {
        self.base.system_prompt()
    }

    fn model_name(&self) -> &str {
        self.base.model_name()
    }

    fn capability(&self) -> &ModelCapability {
        self.base.capability()
    }

    fn is_retryable_error(&self, error: &ProviderErrorInfo) -> bool {
        self.base.is_retryable_error(error)
    }

    fn chat(&self, params: LlmChatParams) -> EngineResult<LlmEventStream> {
        let request = self.build_request(&params)?;
        Ok(spawn_provider_chat(
            Arc::clone(self.base.provider()),
            request,
            params.cancel,
        ))
    }
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;
    use mirri_core::message::{ContentPart, Message};
    use mirri_core::{CancelToken, LlmEvent};
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::*};

    use super::super::kimi_files::{KimiFiles, VideoUploadInput};
    use super::*;

    fn chat_params_with(messages: Vec<Message>) -> LlmChatParams {
        LlmChatParams {
            messages,
            tools: Vec::new(),
            cancel: CancelToken::new(),
            request_log_fields: None,
            cache_root_id: None,
        }
    }

    fn kimi_llm(thinking_effort: Option<ThinkingEffort>) -> KimiLlm {
        KimiLlm::new(
            "test-api-key",
            "https://api.mirricode.com/coding/v1",
            "kimi-k2-test",
            "You are a test assistant.",
            thinking_effort,
        )
    }

    #[test]
    fn should_encode_enabled_thinking_with_effort_when_thinking_effort_is_set() {
        // Given a Kimi adapter with High thinking effort
        let llm = kimi_llm(Some(ThinkingEffort::High));

        // When the request is built
        let request = llm
            .build_request(&chat_params_with(vec![Message::user("hi")]))
            .expect("request builds");

        // Then the thinking intent travels as the Kimi top-level object
        assert_eq!(
            request["thinking"],
            json!({ "type": "enabled", "effort": "high" })
        );
        // And OpenAI's reasoning_effort is never emitted
        assert!(
            request.get("reasoning_effort").is_none(),
            "kimi must not send reasoning_effort"
        );
    }

    #[test]
    fn should_inject_cache_root_id_as_prompt_cache_key_when_params_carry_cache_root() {
        // Given a Kimi adapter (OpenAI dialect) and params whose cache root
        // id (the fork-chain prompt-cache lineage) is set
        let llm = kimi_llm(None);
        let mut params = chat_params_with(vec![Message::user("hi")]);
        params.cache_root_id = Some("session-root".to_string());

        // When the request is built
        let request = llm.build_request(&params).expect("request builds");

        // Then the cache root rides the TOP-LEVEL prompt_cache_key — the
        // same wire the shared OpenAI base emits (TS kosong flattens
        // generationKwargs into the body)
        assert_eq!(
            request["prompt_cache_key"],
            serde_json::json!("session-root"),
            "the cache root id must ride the top-level prompt_cache_key, got: {request}"
        );
    }

    #[test]
    fn should_send_model_declared_efforts_verbatim_in_the_thinking_object() {
        // Given a Kimi adapter with a model-declared custom effort
        // (e.g. "max" — a level the canonical enum does not spell)
        let llm = kimi_llm(Some(ThinkingEffort::Custom("max".to_string())));

        // When the request is built
        let request = llm
            .build_request(&chat_params_with(vec![Message::user("hi")]))
            .expect("request builds");

        // Then the effort rides the Kimi thinking object verbatim
        assert_eq!(
            request["thinking"],
            serde_json::json!({ "type": "enabled", "effort": "max" })
        );
        assert!(
            request.get("reasoning_effort").is_none(),
            "kimi must not send reasoning_effort"
        );
    }

    #[test]
    fn should_encode_disabled_thinking_when_thinking_effort_is_none() {
        // Given a Kimi adapter with an explicit None (off) effort
        let llm = kimi_llm(Some(ThinkingEffort::None));

        // When the request is built
        let request = llm
            .build_request(&chat_params_with(vec![Message::user("hi")]))
            .expect("request builds");

        // Then the thinking object is explicitly disabled
        assert_eq!(request["thinking"], json!({ "type": "disabled" }));
        assert!(request.get("reasoning_effort").is_none());
    }

    #[test]
    fn should_encode_medium_effort_when_thinking_effort_is_medium() {
        // Given a Kimi adapter with the default Medium effort
        let llm = kimi_llm(Some(ThinkingEffort::Medium));

        // When the request is built
        let request = llm
            .build_request(&chat_params_with(vec![Message::user("hi")]))
            .expect("request builds");

        // Then the thinking object enables reasoning with the medium effort
        assert_eq!(
            request["thinking"],
            json!({ "type": "enabled", "effort": "medium" })
        );
        assert!(request.get("reasoning_effort").is_none());
    }

    #[test]
    fn should_omit_thinking_when_thinking_effort_is_unconfigured() {
        // Given a Kimi adapter without thinking configuration
        let llm = kimi_llm(None);

        // When the request is built
        let request = llm
            .build_request(&chat_params_with(vec![Message::user("hi")]))
            .expect("request builds");

        // Then no thinking field is sent
        assert!(
            request.get("thinking").is_none(),
            "unconfigured thinking must be omitted"
        );
        assert!(request.get("reasoning_effort").is_none());
    }

    #[test]
    fn should_reuse_openai_base_fields_when_building_kimi_request() {
        // Given a Kimi adapter with a system prompt and thinking effort
        let llm = kimi_llm(Some(ThinkingEffort::Low));

        // When the request is built
        let request = llm
            .build_request(&chat_params_with(vec![Message::user("hi")]))
            .expect("request builds");

        // Then the shared OpenAI wire fields are present and unchanged
        assert_eq!(request["model"], "kimi-k2-test");
        assert_eq!(request["stream"], true);
        assert_eq!(request["stream_options"], json!({ "include_usage": true }));
        assert_eq!(
            request["messages"][0],
            json!({"role": "system", "content": "You are a test assistant."})
        );
        assert_eq!(
            request["messages"][1],
            json!({"role": "user", "content": "hi"})
        );
        assert_eq!(
            request["thinking"],
            json!({ "type": "enabled", "effort": "low" })
        );
    }

    // -- T7 (F-P8-3): video input ------------------------------------------

    #[test]
    fn should_emit_the_nested_video_url_block_when_a_message_carries_a_video_reference() {
        // Given a Kimi adapter and a user message referencing an uploaded
        // video by its ms:// files-API URL
        let llm = kimi_llm(None);
        let message = Message {
            role: mirri_core::message::Role::User,
            name: None,
            content: vec![
                ContentPart::Text {
                    text: "describe this clip".to_string(),
                },
                ContentPart::VideoUrl {
                    video_url: "ms://file-abc123".to_string(),
                },
            ],
            tool_calls: Vec::new(),
            tool_call_id: None,
            partial: false,
        };

        // When the request is built
        let request = llm
            .build_request(&chat_params_with(vec![message]))
            .expect("request builds");

        // Then the video reference travels as the nested video_url content
        // block (the TS convertContentPart shape the Kimi endpoint expects)
        let user_content = &request["messages"][1]["content"];
        assert_eq!(
            user_content[0],
            json!({"type": "text", "text": "describe this clip"})
        );
        assert_eq!(
            user_content[1],
            json!({
                "type": "video_url",
                "video_url": { "url": "ms://file-abc123" }
            })
        );
        // And the adapter declares the video input capability
        assert!(llm.capability().video_in, "kimi must accept video input");
    }

    /// The upload-then-chat composition: a video uploaded through
    /// [`KimiFiles`](super::kimi_files::KimiFiles) (the Kimi files API) is
    /// referenced in the chat request by its `ms://` id — the TS
    /// `kimi-files.ts` semantics, over mock transport (a real end-to-end
    /// against the Moonshot backend needs real credentials and stays a
    /// manual verification).
    #[tokio::test]
    async fn should_reference_an_uploaded_video_in_the_chat_request() {
        // Given a mock Kimi backend serving both the files endpoint and the
        // chat completions endpoint
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/files"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "id": "file-video-1", "object": "file" })),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(
                        "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":\"stop\"}]}\n\n\
                         data: [DONE]\n\n",
                    ),
            )
            .mount(&server)
            .await;

        // When a video is uploaded and the returned reference is sent in a
        // chat message
        let files = KimiFiles::new("test-api-key", format!("{}/v1", server.uri()));
        let uploaded = files
            .upload_video(VideoUploadInput::Bytes {
                data: b"\x00\x00\x00\x18ftypmp42fake".as_slice(),
                mime_type: "video/mp4",
                filename: Some("clip.mp4"),
            })
            .await
            .expect("upload succeeds");
        assert_eq!(uploaded.url, "ms://file-video-1");

        let llm = KimiLlm::new(
            "test-api-key",
            format!("{}/v1", server.uri()),
            "kimi-k2-test",
            "",
            Some(ThinkingEffort::Low),
        );
        let mut stream = llm
            .chat(chat_params_with(vec![Message {
                role: mirri_core::message::Role::User,
                name: None,
                content: vec![ContentPart::VideoUrl {
                    video_url: uploaded.url.clone(),
                }],
                tool_calls: Vec::new(),
                tool_call_id: None,
                partial: false,
            }]))
            .expect("chat starts");
        while let Some(event) = stream.next().await {
            if matches!(event, LlmEvent::Done { .. }) {
                break;
            }
        }

        // Then the chat request carried the files-API reference in the
        // nested video_url block (and only the chat request shape — the
        // upload itself went to the files endpoint)
        let requests = server.received_requests().await.expect("requests");
        let chat_request = requests
            .iter()
            .find(|request| request.url.path() == "/v1/chat/completions")
            .expect("chat request recorded");
        let body: serde_json::Value =
            serde_json::from_slice(&chat_request.body).expect("chat body is JSON");
        // (empty system prompt — the user message is the first entry)
        assert_eq!(
            body["messages"][0]["content"][0],
            json!({
                "type": "video_url",
                "video_url": { "url": "ms://file-video-1" }
            })
        );
    }

    #[tokio::test]
    async fn should_stream_text_and_trailing_usage_when_kimi_returns_sse_response() {
        // Given a mock Kimi endpoint streaming text chunks, a finish_reason
        // chunk, and a trailing usage chunk (the OpenAI wire shape Kimi
        // speaks)
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
                         data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":7}}\n\n\
                         data: [DONE]\n\n",
                    ),
            )
            .mount(&server)
            .await;

        // When the Kimi adapter chats with thinking enabled
        let llm = KimiLlm::new(
            "test-api-key",
            format!("{}/v1", server.uri()),
            "kimi-k2-test",
            "",
            Some(ThinkingEffort::High),
        );
        let mut stream = llm
            .chat(chat_params_with(vec![Message::user("hi")]))
            .expect("chat starts");
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }

        // Then the stream yields the streamed text, the aggregated usage,
        // and a done event
        let text: String = events
            .iter()
            .filter_map(|event| match event {
                LlmEvent::TextDelta { text } => Some(text.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello world");
        assert!(
            events
                .iter()
                .any(|event| matches!(event, LlmEvent::Usage { .. }))
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, LlmEvent::Done { .. }))
        );

        // And the single request carried the Kimi thinking dialect, not
        // OpenAI's reasoning_effort
        let requests = server.received_requests().await.expect("received requests");
        assert_eq!(requests.len(), 1);
        let body: serde_json::Value =
            serde_json::from_slice(&requests[0].body).expect("request body is JSON");
        assert_eq!(
            body["thinking"],
            json!({ "type": "enabled", "effort": "high" })
        );
        assert!(body.get("reasoning_effort").is_none());
    }

    // -- quota-429 billing short-circuit (T5, P8b-1) ----------------------

    #[tokio::test]
    async fn should_emit_a_quota_exhausted_classifiable_error_when_kimi_answers_a_billing_429() {
        // Given a mock Kimi endpoint answering the Moonshot quota body
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(429).set_body_json(serde_json::json!({
                    "error": {
                        "type": "exceeded_current_quota_error",
                        "message": "You exceeded your current token quota, please check your account balance"
                    }
                })),
            )
            .mount(&server)
            .await;

        // When a chat is streamed through the Kimi adapter
        let llm = KimiLlm::new(
            "test-api-key",
            format!("{}/v1", server.uri()),
            "kimi-k2-test",
            "",
            None,
        );
        let mut stream = llm
            .chat(chat_params_with(vec![Message {
                role: mirri_core::message::Role::User,
                name: None,
                content: vec![ContentPart::Text {
                    text: "hi".to_string(),
                }],
                tool_calls: Vec::new(),
                tool_call_id: None,
                partial: false,
            }]))
            .expect("chat starts");
        let mut error_message: Option<String> = None;
        while let Some(event) = stream.next().await {
            if let LlmEvent::Error { message } = event {
                error_message = Some(message);
                break;
            }
        }

        // Then the surfaced error text classifies as quota exhaustion
        // (the engine's classify_provider_error turns this terminal)
        let message = error_message.expect("an error event");
        let info = mirri_core::llm::ProviderErrorInfo::from_message(&message);
        assert!(info.is_quota_exhausted(), "message: {message}");
        assert_eq!(info.provider_domain_code(), "provider.quota_exhausted");
    }
}
