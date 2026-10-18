//! mirri-kosong: LLM provider abstraction with Anthropic SSE streaming.
//!
//! PoC-1 scope is limited to verifying that SSE streaming correctly parses
//! `text_delta` and tool-call (`input_json_delta`) events from the Anthropic
//! Messages API.

pub mod anthropic;
pub mod google;
pub mod openai;
pub mod openai_responses;
pub mod provider;
pub mod streaming;

pub use anthropic::AnthropicProvider;
pub use google::GoogleGenAIProvider;
pub use openai::OpenAILegacyProvider;
pub use openai_responses::OpenAIResponsesProvider;
pub use provider::{ChatProvider, StreamEvent, http_client_with_timeouts, join_url};
pub use streaming::SseStream;
