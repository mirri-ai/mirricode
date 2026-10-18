//! Out-of-band LLM session-title generation (spec 09 / ADR-020).
//!
//! The title is produced by a side LLM call that never touches the main
//! turn loop: the caller passes the conversation prefix exactly as the
//! main loop serialized it (same system prompt lives in the adapter,
//! same tool list, same projected messages), and this module appends one
//! instruction message, performs a single chat, and returns the raw
//! response text for the caller to sanitize through the shared
//! rule-based path. Nothing here records into context memory or the
//! wire log.

use crate::llm::{Llm, LlmChatParams, LlmUsage};
use crate::message::Message;
use crate::tool::ToolDefinition;
use crate::{EngineError, EngineResult};

/// The instruction appended as the final user message of the side call.
pub const TITLE_GENERATION_INSTRUCTION: &str = "Based on the conversation above, write a short title (3-8 words) that captures the user's main intent in this session.\n\nRules:\n- Reply with the title text only — no quotes, no trailing punctuation, no explanation.\n- Use the same language as the user's first message.\n- Never include secrets, tokens, or credentials.";

/// Run the one-shot side title call. `messages` must already be the
/// media-adapted projection the main loop sent; the instruction message
/// is appended here. Returns the raw model text (the caller sanitizes
/// through `set_auto_title`) plus the usage buckets for cache
/// observability (rust AGENTS.md gate 5).
pub async fn generate_session_title(
    llm: &dyn Llm,
    mut messages: Vec<Message>,
    tools: Vec<ToolDefinition>,
) -> EngineResult<(String, LlmUsage)> {
    messages.push(Message::user(TITLE_GENERATION_INSTRUCTION));
    let mut params = LlmChatParams {
        messages,
        tools,
        // A fresh token: the settled turn's token may already be
        // cancelled, and this call must never be tied to the turn's
        // lifecycle.
        cancel: crate::cancel::CancelToken::new(),
        request_log_fields: Some(serde_json::json!({ "purpose": "session-title" })),
        cache_root_id: None,
    };
    if let Err(error) = llm.materialize_media(&mut params).await {
        tracing::warn!(
            %error,
            "title-call media materialization failed; sending inline media unchanged"
        );
    }
    let stream = llm.chat(params)?;
    let collected = crate::loop_engine::shared::collect_stream_events(stream, None).await;
    if let Some(message) = collected.stream_error {
        return Err(EngineError::ProviderError(message));
    }
    let usage = collected.usage.clone();
    let text = collected.text_parts.join("");
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(EngineError::ProviderError(
            "title call returned no text".to_string(),
        ));
    }
    Ok((trimmed.to_string(), usage))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{LlmEvent, LlmEventStream, ModelCapability, ProviderErrorInfo};
    use futures::stream;
    use std::sync::{Arc, Mutex};

    /// Minimal scripted mock mirroring the `MockLlm` pattern in
    /// `tests/compaction_tests.rs:171` — captures the chat params and
    /// replies with one text part.
    struct TitleMockLlm {
        captured: Mutex<Vec<LlmChatParams>>,
        events: Mutex<Vec<LlmEvent>>,
    }

    impl TitleMockLlm {
        fn replying(text: &str) -> Arc<Self> {
            Arc::new(Self {
                captured: Mutex::new(Vec::new()),
                events: Mutex::new(vec![
                    LlmEvent::TextPart {
                        text: text.to_string(),
                    },
                    LlmEvent::Done {
                        stop_reason: "end_turn".to_string(),
                    },
                ]),
            })
        }
        fn erroring(message: &str) -> Arc<Self> {
            Arc::new(Self {
                captured: Mutex::new(Vec::new()),
                events: Mutex::new(vec![LlmEvent::Error {
                    message: message.to_string(),
                }]),
            })
        }
    }

    impl Llm for TitleMockLlm {
        fn system_prompt(&self) -> &str {
            "system"
        }
        fn model_name(&self) -> &str {
            "mock-model"
        }
        fn capability(&self) -> &ModelCapability {
            static CAP: ModelCapability = ModelCapability {
                image_in: false,
                video_in: false,
                audio_in: false,
                dynamically_loaded_tools: false,
            };
            &CAP
        }
        fn is_retryable_error(&self, _error: &ProviderErrorInfo) -> bool {
            false
        }
        fn chat(&self, params: LlmChatParams) -> EngineResult<LlmEventStream> {
            self.captured.lock().unwrap().push(params.clone());
            let events = self.events.lock().unwrap().clone();
            Ok(Box::pin(stream::iter(events)))
        }
    }

    #[tokio::test]
    async fn should_return_the_model_title_and_append_the_instruction_as_the_last_message() {
        let llm = TitleMockLlm::replying("  Fix login timeout  ");
        let messages = vec![
            Message::user("my login always times out"),
            Message::assistant("let me look into it"),
        ];
        let tools = vec![ToolDefinition {
            name: "Bash".to_string(),
            description: "run".to_string(),
            parameters: serde_json::json!({"type": "object"}),
            deferred: false,
        }];

        let (title, _usage) = generate_session_title(llm.as_ref(), messages, tools)
            .await
            .unwrap();

        assert_eq!(title, "Fix login timeout");
        let captured = llm.captured.lock().unwrap();
        let params = captured.last().expect("one chat call");
        // The instruction rides as the final user message, after the
        // conversation prefix.
        let last = params.messages.last().expect("messages non-empty");
        assert_eq!(last.text_content(), TITLE_GENERATION_INSTRUCTION);
        assert_eq!(params.messages.len(), 3);
        assert_eq!(
            params.messages[0].text_content(),
            "my login always times out"
        );
        // The tools pass through untouched (prefix fidelity).
        assert_eq!(params.tools.len(), 1);
        assert_eq!(params.tools[0].name, "Bash");
    }

    #[tokio::test]
    async fn should_fail_when_the_response_carries_no_text() {
        let llm = Arc::new(TitleMockLlm {
            captured: Mutex::new(Vec::new()),
            events: Mutex::new(vec![LlmEvent::Done {
                stop_reason: "end_turn".to_string(),
            }]),
        });
        let result =
            generate_session_title(llm.as_ref(), vec![Message::user("hi")], Vec::new()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn should_surface_stream_errors() {
        let llm = TitleMockLlm::erroring("provider exploded");
        let result =
            generate_session_title(llm.as_ref(), vec![Message::user("hi")], Vec::new()).await;
        assert!(result.is_err());
    }
}
