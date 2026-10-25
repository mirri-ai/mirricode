//! Real LLM provider adapters — bridge `mirri-kosong`'s HTTP transports to
//! [`mirri_core::Llm`].
//!
//! `mirri-kosong` owns provider transport and SSE parsing
//! ([`ChatProvider`](mirri_kosong::ChatProvider)/[`SseStream`](mirri_kosong::SseStream));
//! the adapters in this module own the wire-format translation between the
//! engine's canonical [`Message`](mirri_core::Message)/[`ToolDefinition`]
//! shapes and each provider's native request JSON, plus the conversion from
//! normalized [`StreamEvent`](mirri_kosong::StreamEvent)s to
//! [`LlmEvent`](mirri_core::LlmEvent)s.
//!
//! [`ProviderLlmFactory`] wires the adapters into the embedded engine
//! ([`crate::LlmFactory`]) from environment variables.

pub mod anthropic;
pub mod anthropic_files;
pub mod config_factory;
mod convert;
pub mod google;
pub mod kimi;
pub mod kimi_files;
pub mod media_ref;
mod merge_user_messages;
pub mod openai;
pub mod openai_responses;
mod tool_result_media;
pub mod web;

pub use anthropic::AnthropicLlm;
pub use anthropic_files::{AnthropicFiles, UploadedFile};
pub use config_factory::{ConfigAwareLlmFactory, ConfigResolvingLlm};
pub use google::GoogleLlm;
pub use kimi::KimiLlm;
pub use kimi_files::{KimiFiles, UploadedVideo, VideoUploadInput};
pub use media_ref::{FILEREF_PREFIX, format_fileref, is_fileref, parse_fileref};
pub use openai::OpenAILlm;
pub use openai_responses::OpenAIResponsesLlm;
pub use web::{
    LocalFetchUrlProvider, ManagedFetchConfig, ManagedFetchUrlProvider, ManagedSearchConfig,
    ManagedWebSearchProvider,
};

use std::path::PathBuf;
use std::sync::Arc;

use mirri_core::{AgentScopeConfig, Llm, ProviderType};
use mirri_persistence::provider_file_cache::ProviderFileCache;

/// An [`LlmFactory`](crate::LlmFactory) building real provider adapters from
/// the environment:
///
/// - [`ProviderType::Anthropic`] — `ANTHROPIC_API_KEY` (required),
///   `ANTHROPIC_BASE_URL` (default `https://api.anthropic.com/v1`)
/// - [`ProviderType::OpenAi`] — `OPENAI_API_KEY` (required),
///   `OPENAI_BASE_URL` (default `https://api.openai.com/v1`)
/// - [`ProviderType::OpenAiResponses`] — same env vars as `OpenAi`
/// - [`ProviderType::Google`] — `GOOGLE_API_KEY` (required),
///   `GOOGLE_GEMINI_BASE_URL` (default
///   `https://generativelanguage.googleapis.com/v1beta`)
/// - [`ProviderType::Kimi`] — `MIRRI_API_KEY` (required),
///   `MIRRI_BASE_URL` (default `https://api.mirricode.com/coding/v1`)
/// - [`ProviderType::Custom`] — unsupported (no custom adapter exists yet)
///
/// The agent's `model_id`, `system_prompt`, and `thinking_effort` are passed
/// through to the constructed adapter.
#[derive(Debug, Clone, Default)]
pub struct ProviderLlmFactory {
    /// The root of the provider-side media file cache
    /// (`<mirri home>/provider-files/`, T11a). When present, the Anthropic
    /// adapter is built with the Files API materialization channel; `None`
    /// (the default) keeps every adapter fully inline.
    pub media_cache_root: Option<PathBuf>,
}

impl crate::LlmFactory for ProviderLlmFactory {
    fn create_llm(&self, config: &AgentScopeConfig) -> Result<Arc<dyn Llm>, String> {
        match config.provider_type {
            ProviderType::Anthropic => {
                let api_key = std::env::var("ANTHROPIC_API_KEY")
                    .map_err(|_| "ANTHROPIC_API_KEY not set".to_string())?;
                let base_url = std::env::var("ANTHROPIC_BASE_URL")
                    .unwrap_or_else(|_| "https://api.anthropic.com/v1".to_string());
                let mut llm = AnthropicLlm::new(
                    api_key.clone(),
                    base_url.clone(),
                    config.model_id.clone(),
                    config.system_prompt.clone().unwrap_or_default(),
                    config.thinking_effort.clone(),
                );
                if let Some(root) = &self.media_cache_root {
                    llm = llm.with_media_files(
                        AnthropicFiles::new(api_key, base_url),
                        Arc::new(ProviderFileCache::new(root.clone())),
                    );
                }
                Ok(Arc::new(llm))
            }
            ProviderType::OpenAi => {
                let api_key = std::env::var("OPENAI_API_KEY")
                    .map_err(|_| "OPENAI_API_KEY not set".to_string())?;
                let base_url = std::env::var("OPENAI_BASE_URL")
                    .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
                Ok(Arc::new(OpenAILlm::new(
                    api_key,
                    base_url,
                    config.model_id.clone(),
                    config.system_prompt.clone().unwrap_or_default(),
                    config.thinking_effort.clone(),
                )))
            }
            ProviderType::OpenAiResponses => {
                let api_key = std::env::var("OPENAI_API_KEY")
                    .map_err(|_| "OPENAI_API_KEY not set".to_string())?;
                let base_url = std::env::var("OPENAI_BASE_URL")
                    .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
                Ok(Arc::new(OpenAIResponsesLlm::new(
                    api_key,
                    base_url,
                    config.model_id.clone(),
                    config.system_prompt.clone().unwrap_or_default(),
                    config.thinking_effort.clone(),
                )))
            }
            ProviderType::Google => {
                let api_key = std::env::var("GOOGLE_API_KEY")
                    .map_err(|_| "GOOGLE_API_KEY not set".to_string())?;
                let base_url = std::env::var("GOOGLE_GEMINI_BASE_URL").unwrap_or_else(|_| {
                    "https://generativelanguage.googleapis.com/v1beta".to_string()
                });
                Ok(Arc::new(GoogleLlm::new(
                    api_key,
                    base_url,
                    config.model_id.clone(),
                    config.system_prompt.clone().unwrap_or_default(),
                    config.thinking_effort.clone(),
                )))
            }
            ProviderType::Kimi => {
                let api_key = std::env::var("MIRRI_API_KEY")
                    .map_err(|_| "MIRRI_API_KEY not set".to_string())?;
                let base_url = std::env::var("MIRRI_BASE_URL")
                    .unwrap_or_else(|_| "https://api.mirricode.com/coding/v1".to_string());
                Ok(Arc::new(KimiLlm::new(
                    api_key,
                    base_url,
                    config.model_id.clone(),
                    config.system_prompt.clone().unwrap_or_default(),
                    config.thinking_effort.clone(),
                )))
            }
            ProviderType::Custom => {
                Err("Custom provider type not supported by ProviderLlmFactory".to_string())
            }
        }
    }
}

/// Serializes tests that mutate process environment variables, which are
/// process-global and would otherwise race between parallel tests.
/// `pub(crate)` so the config_factory tests and the embedded_server tests
/// (which also mutate env vars) share the same lock.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use futures::StreamExt;
    use mirri_core::llm::{LlmChatParams, LlmEventStream};
    use mirri_core::message::Message;
    use mirri_core::{AgentScopeConfig, CancelToken, LlmEvent, ProviderType, ThinkingEffort};
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::*};

    use super::*;
    use crate::LlmFactory;

    use super::ENV_LOCK;

    fn anthropic_config() -> AgentScopeConfig {
        AgentScopeConfig {
            provider_type: ProviderType::Anthropic,
            model_id: "claude-test".to_string(),
            system_prompt: Some("You are a test assistant.".to_string()),
            thinking_effort: Some(ThinkingEffort::Low),
            ..Default::default()
        }
    }

    #[test]
    fn should_create_anthropic_llm_when_provider_type_is_anthropic_and_api_key_is_set() {
        // Given the Anthropic environment variables and an Anthropic config
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("ANTHROPIC_API_KEY", "test-key") };
        unsafe { std::env::remove_var("ANTHROPIC_BASE_URL") };

        // When an LLM is created
        let llm = ProviderLlmFactory::default()
            .create_llm(&anthropic_config())
            .expect("llm is created");

        // Then it carries the configured model, prompt, and the
        // constructed-default all-false capability — the env-based factory
        // performs no capability resolution (the config factory is the
        // production assembly point that merges resolved capabilities)
        assert_eq!(llm.model_name(), "claude-test");
        assert_eq!(llm.system_prompt(), "You are a test assistant.");
        assert!(!llm.capability().image_in);
        assert!(!llm.capability().video_in);
        assert!(!llm.capability().audio_in);
        assert!(!llm.capability().dynamically_loaded_tools);
    }

    #[test]
    fn should_create_openai_llm_when_provider_type_is_openai_and_api_key_is_set() {
        // Given the OpenAI environment variables and an OpenAI config
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("OPENAI_API_KEY", "test-key") };
        unsafe { std::env::set_var("OPENAI_BASE_URL", "https://proxy.example.com") };
        let config = AgentScopeConfig {
            provider_type: ProviderType::OpenAi,
            model_id: "gpt-test".to_string(),
            system_prompt: None,
            ..Default::default()
        };

        // When an LLM is created
        let llm = ProviderLlmFactory::default()
            .create_llm(&config)
            .expect("llm is created");

        // Then it carries the configured model and an empty default prompt
        assert_eq!(llm.model_name(), "gpt-test");
        assert_eq!(llm.system_prompt(), "");
    }

    #[test]
    fn should_fail_when_anthropic_api_key_is_missing() {
        // Given no ANTHROPIC_API_KEY and an Anthropic config
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::remove_var("ANTHROPIC_API_KEY") };

        // When an LLM is created
        let err = ProviderLlmFactory::default()
            .create_llm(&anthropic_config())
            .err()
            .expect("creation fails without an API key");

        // Then the failure names the missing variable
        assert!(err.contains("ANTHROPIC_API_KEY"), "got: {err}");
    }

    #[test]
    fn should_create_google_llm_when_provider_type_is_google_and_api_key_is_set() {
        // Given the Google environment variables and a Google config
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("GOOGLE_API_KEY", "test-key") };
        unsafe { std::env::remove_var("GOOGLE_GEMINI_BASE_URL") };
        let config = AgentScopeConfig {
            provider_type: ProviderType::Google,
            model_id: "gemini-test".to_string(),
            system_prompt: None,
            ..Default::default()
        };

        // When an LLM is created
        let llm = ProviderLlmFactory::default()
            .create_llm(&config)
            .expect("llm is created");

        // Then it carries the configured model and an empty default prompt
        assert_eq!(llm.model_name(), "gemini-test");
        assert_eq!(llm.system_prompt(), "");
    }

    #[test]
    fn should_fail_when_google_api_key_is_missing() {
        // Given no GOOGLE_API_KEY and a Google config
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::remove_var("GOOGLE_API_KEY") };
        let config = AgentScopeConfig {
            provider_type: ProviderType::Google,
            model_id: "gemini-test".to_string(),
            system_prompt: None,
            ..Default::default()
        };

        // When an LLM is created
        let err = ProviderLlmFactory::default()
            .create_llm(&config)
            .err()
            .expect("creation fails without an API key");

        // Then the failure names the missing variable
        assert!(err.contains("GOOGLE_API_KEY"), "got: {err}");
    }

    #[test]
    fn should_fail_for_custom_provider_type_when_factory_does_not_support_it() {
        // Given a Custom provider config
        let config = AgentScopeConfig {
            provider_type: ProviderType::Custom,
            ..Default::default()
        };

        // When an LLM is created
        let err = ProviderLlmFactory::default()
            .create_llm(&config)
            .err()
            .expect("custom providers are unsupported");

        // Then the failure explains the limitation
        assert!(
            err.contains("Custom provider type not supported"),
            "got: {err}"
        );
    }

    #[test]
    fn should_create_kimi_llm_when_provider_type_is_kimi_and_mirri_api_key_is_set() {
        // Given the Kimi environment variables and a Kimi config
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("MIRRI_API_KEY", "test-key") };
        unsafe { std::env::remove_var("MIRRI_BASE_URL") };

        // When an LLM is created
        let config = AgentScopeConfig {
            provider_type: ProviderType::Kimi,
            model_id: "kimi-k2-test".to_string(),
            system_prompt: None,
            ..Default::default()
        };
        let llm = ProviderLlmFactory::default()
            .create_llm(&config)
            .expect("llm is created");

        // Then it carries the configured model and prompt, and the Kimi
        // video input capability (the files-API reference channel) stays
        // declared while every other modality stays false — the env-based
        // factory performs no capability resolution and "kimi-k2-test"
        // matches no catalog marker, so unresolved modalities default to
        // unsupported (ADR-003 revision; a resolved/declared capability is
        // what merges the flags in, via the config-aware factory)
        assert_eq!(llm.model_name(), "kimi-k2-test");
        assert_eq!(llm.system_prompt(), "");
        assert!(!llm.capability().image_in);
        assert!(llm.capability().video_in, "kimi accepts video input (T7)");
    }

    #[test]
    fn should_fail_when_mirri_api_key_is_missing() {
        // Given no MIRRI_API_KEY and a Kimi config
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::remove_var("MIRRI_API_KEY") };
        let factory = ProviderLlmFactory::default();

        // When an LLM is created
        let config = AgentScopeConfig {
            provider_type: ProviderType::Kimi,
            ..Default::default()
        };
        let err = factory
            .create_llm(&config)
            .err()
            .expect("creation fails without an API key");

        // Then the failure names the missing variable
        assert!(err.contains("MIRRI_API_KEY"), "got: {err}");
    }

    #[tokio::test]
    async fn should_send_requests_to_mirri_base_url_when_mirri_base_url_env_var_is_set() {
        // Given a mock OpenAI-shaped endpoint
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(
                        "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n\
                         data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                         data: [DONE]\n\n",
                    ),
            )
            .mount(&server)
            .await;

        // And the environment routes Kimi traffic to that endpoint
        let config = AgentScopeConfig {
            provider_type: ProviderType::Kimi,
            model_id: "kimi-k2-test".to_string(),
            system_prompt: None,
            ..Default::default()
        };
        let llm = {
            let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            unsafe { std::env::set_var("MIRRI_API_KEY", "test-key") };
            unsafe { std::env::set_var("MIRRI_BASE_URL", format!("{}/v1", server.uri())) };
            ProviderLlmFactory::default()
                .create_llm(&config)
                .expect("llm is created")
        };

        // When the created LLM chats
        let mut stream = llm
            .chat(LlmChatParams {
                messages: vec![Message::user("hi")],
                tools: Vec::new(),
                cancel: CancelToken::new(),
                request_log_fields: None,
                cache_root_id: None,
            })
            .expect("chat starts");
        drain(&mut stream).await;

        // Then the request reached the configured base URL, proving
        // MIRRI_BASE_URL flows from the factory into the HTTP transport
        let requests = server.received_requests().await.expect("received requests");
        assert_eq!(requests.len(), 1, "exactly one request reaches the mock");
        assert_eq!(requests[0].url.path(), "/v1/chat/completions");
    }

    #[tokio::test]
    async fn should_send_requests_to_configured_base_url_when_openai_base_url_env_var_is_set() {
        // Given a mock OpenAI endpoint
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(
                        "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n\
                         data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                         data: [DONE]\n\n",
                    ),
            )
            .mount(&server)
            .await;

        // And the environment routes OpenAI traffic to that endpoint
        let config = AgentScopeConfig {
            provider_type: ProviderType::OpenAi,
            model_id: "gpt-test".to_string(),
            system_prompt: None,
            ..Default::default()
        };
        let llm = {
            let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            unsafe { std::env::set_var("OPENAI_API_KEY", "test-key") };
            unsafe { std::env::set_var("OPENAI_BASE_URL", format!("{}/v1", server.uri())) };
            ProviderLlmFactory::default()
                .create_llm(&config)
                .expect("llm is created")
        };

        // When the created LLM chats
        let mut stream = llm
            .chat(LlmChatParams {
                messages: vec![Message::user("hi")],
                tools: Vec::new(),
                cancel: CancelToken::new(),
                request_log_fields: None,
                cache_root_id: None,
            })
            .expect("chat starts");
        drain(&mut stream).await;

        // Then the request reached the configured base URL, proving
        // OPENAI_BASE_URL flows from the factory into the HTTP transport
        let requests = server.received_requests().await.expect("received requests");
        assert_eq!(requests.len(), 1, "exactly one request reaches the mock");
        assert_eq!(requests[0].url.path(), "/v1/chat/completions");
    }

    #[tokio::test]
    async fn should_send_requests_to_configured_base_url_when_anthropic_base_url_env_var_is_set() {
        // Given a mock Anthropic endpoint
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(
                        "event: message_start\n\
                         data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-sonnet-4-20250514\",\"usage\":{\"input_tokens\":10}}}\n\n\
                         event: content_block_start\n\
                         data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
                         event: content_block_delta\n\
                         data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hello\"}}\n\n\
                         event: content_block_stop\n\
                         data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
                         event: message_delta\n\
                         data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":10}}\n\n\
                         event: message_stop\n\
                         data: {\"type\":\"message_stop\"}\n\n",
                    ),
            )
            .mount(&server)
            .await;

        // And the environment routes Anthropic traffic to that endpoint
        let config = AgentScopeConfig {
            provider_type: ProviderType::Anthropic,
            model_id: "claude-test".to_string(),
            system_prompt: None,
            ..Default::default()
        };
        let llm = {
            let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            unsafe { std::env::set_var("ANTHROPIC_API_KEY", "test-key") };
            unsafe { std::env::set_var("ANTHROPIC_BASE_URL", format!("{}/v1", server.uri())) };
            ProviderLlmFactory::default()
                .create_llm(&config)
                .expect("llm is created")
        };

        // When the created LLM chats
        let mut stream = llm
            .chat(LlmChatParams {
                messages: vec![Message::user("hi")],
                tools: Vec::new(),
                cancel: CancelToken::new(),
                request_log_fields: None,
                cache_root_id: None,
            })
            .expect("chat starts");
        drain(&mut stream).await;

        // Then the request reached the configured base URL, proving
        // ANTHROPIC_BASE_URL flows from the factory into the HTTP transport
        let requests = server.received_requests().await.expect("received requests");
        assert_eq!(requests.len(), 1, "exactly one request reaches the mock");
        assert_eq!(requests[0].url.path(), "/v1/messages");
    }

    // --- helpers ----------------------------------------------------------

    /// Drain a chat stream to EOF so the spawned HTTP request completes.
    async fn drain(stream: &mut LlmEventStream) -> Vec<LlmEvent> {
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        events
    }
}
