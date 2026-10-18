use anyhow::Result;
use serde_json::Value;

use crate::streaming::SseStream;

/// Join a base URL with an endpoint path.
///
/// The caller owns the full URL shape: `base` must already carry any
/// version segment the endpoint requires (e.g. `https://…/v1/` for the
/// OpenAI family, `https://…/v1beta` for Google, or a bare host for a
/// version-less API such as GitHub Copilot), and `path` is appended
/// verbatim after trimming trailing slashes from the base. No version
/// segment is ever auto-appended — a base missing its version segment is
/// a configuration error that surfaces as the provider's own 404.
///
/// Public since T7: the Kimi files client (`mirri-embedded`) reuses the
/// same join for the `files` endpoint.
pub fn join_url(base: &str, path: &str) -> String {
    format!("{}{}", base.trim_end_matches('/'), path)
}

/// Default connect timeout for provider HTTP calls — a stalled TCP/TLS
/// handshake must fail fast (healthy connects take well under a second).
pub(crate) const HTTP_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Default per-read idle timeout, applied to EVERY read operation on the
/// connection: a healthy SSE stream (chunk or keep-alive at least this
/// often) is never cut, but a provider that goes silent mid-stream fails
/// instead of hanging forever (reqwest's total `timeout` is unusable here —
/// it would kill legitimate long streams).
///
/// Deliberately shorter than the engine's turn stall watchdog
/// (`ENGINE_TURN_STALL_TIMEOUT`, 300 s default in `mirri-embedded`): with
/// read at 0.8x, a silently hung stream trips the provider-side read error
/// first — a retryable `LLM stream error` — while the watchdog stays a
/// pure engine-side backstop instead of masking the provider failure as
/// a cancellation. Healthy SSE keep-alives are second-scale, so the
/// shorter window is safe.
pub(crate) const HTTP_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(240);

/// The shared provider HTTP client (rustls via the workspace default
/// features, streaming-friendly timeouts).
pub fn http_client_with_timeouts(
    connect: std::time::Duration,
    read: std::time::Duration,
) -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .connect_timeout(connect)
        .read_timeout(read)
        .build()
}

#[cfg(test)]
mod join_url_tests {
    use super::join_url;

    #[test]
    fn should_append_the_path_verbatim_when_the_base_has_no_trailing_slash() {
        assert_eq!(
            join_url("https://api.moonshot.ai/v1", "/files"),
            "https://api.moonshot.ai/v1/files"
        );
    }

    #[test]
    fn should_trim_trailing_slashes_from_the_base_before_appending_the_path() {
        assert_eq!(
            join_url("https://api.moonshot.ai/v1/", "/files"),
            "https://api.moonshot.ai/v1/files"
        );
    }

    #[test]
    fn should_not_inject_a_version_segment_when_the_base_has_none() {
        // A provider whose endpoints carry no version prefix (e.g. GitHub
        // Copilot's /chat/completions) must keep the bare path — the engine
        // never guesses a version segment the user did not configure.
        assert_eq!(
            join_url("https://api.githubcopilot.com", "/chat/completions"),
            "https://api.githubcopilot.com/chat/completions"
        );
    }
}

/// Events emitted by a streaming LLM provider.
///
/// These are the logical, normalized events produced by parsing the provider's
/// raw SSE stream. A single underlying SSE event may produce zero, one, or
/// several `StreamEvent`s (for example, `message_delta` yields both a `Usage`
/// and a `Done`).
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// Stream started; the model that is serving the request is available.
    ///
    /// Note: this is currently folded into the first `Usage` event emitted from
    /// `message_start` for token accounting; kept for API completeness.
    #[allow(dead_code)]
    MessageStart { model: String },

    /// An incremental piece of assistant text.
    TextDelta { text: String },

    /// A complete tool call. Emitted once the corresponding content block is
    /// fully received (`content_block_stop`), with the tool's accumulated JSON
    /// arguments parsed into a `Value`.
    ToolCall {
        tool: String,
        args: Value,
        call_id: String,
    },

    /// Token usage accounting. `input_tokens` is non-zero on the first event
    /// (from `message_start`); `output_tokens` is non-zero on the final event
    /// (from `message_delta`). `cache_read_tokens` / `cache_creation_tokens`
    /// carry the provider's prompt-cache hit and write counts (Anthropic
    /// `cache_read_input_tokens` / `cache_creation_input_tokens`, OpenAI and
    /// Google `cached_tokens` / `cachedContentTokenCount`); `input_tokens`
    /// excludes them (the dispatchers subtract cached tokens so the three
    /// input-side buckets never double-count).
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cache_creation_tokens: u64,
    },

    /// Stream completed; carries the provider's stop reason.
    Done { stop_reason: String },

    /// Stream-level error surfaced while parsing the SSE response.
    Error { message: String },

    /// Incremental reasoning content (Anthropic thinking, OpenAI reasoning,
    /// Google thought). Emitted as the provider streams intermediate
    /// reasoning steps that precede the final answer.
    ThinkDelta { text: String },

    /// Incremental tool-call argument fragment — used by OpenAI-style
    /// streaming where tool call arguments arrive in index-addressed chunks.
    /// The next non-incremental event (a `ToolCallPart` at a different index,
    /// or a `Done`/`ToolCall`) flushes the pending tool call.
    ///
    /// `call_id`/`tool_name` carry the fragment's identity when the adapter
    /// already knows it (OpenAI sends the id and name in the first chunk of
    /// each tool call; the Responses API registers them on
    /// `output_item.added`). `None` on fragments that precede their identity
    /// header — consumers must not claim an identity for those. Once present,
    /// `call_id` is exactly the id the eventual block-level [`ToolCall`][0]
    /// will flush with.
    ///
    /// [0]: StreamEvent::ToolCall
    ToolCallPart {
        index: u64,
        arguments_part: String,
        call_id: Option<String>,
        tool_name: Option<String>,
    },
}

/// A streaming chat provider sends a chat request and returns a concrete
/// [`SseStream`] of [`StreamEvent`]s.
///
/// This trait deliberately returns a concrete `SseStream` (via RPITIT) rather
/// than `Box<dyn ...>`: `async fn` in traits yields unnamed future types,
/// which makes any trait containing them non-object-safe. Returning the
/// concrete stream type keeps the API ergonomic without `async-trait` and
/// without `dyn`.
pub trait ChatProvider: Send + Sync {
    /// Send a chat request and return a stream of events.
    ///
    /// `request` is a raw JSON value matching the provider's native API
    /// format; the provider is responsible for authentication headers,
    /// endpoint routing, and error handling for non-success HTTP statuses.
    fn chat(&self, request: Value) -> impl std::future::Future<Output = Result<SseStream>> + Send;
}
