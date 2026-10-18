//! [`AnthropicLlm`] — a [`Llm`](mirri_core::Llm) adapter over the Anthropic
//! Messages API.
//!
//! The adapter owns the wire-format translation between the engine's
//! canonical [`Message`]/[`ToolDefinition`] shapes and Anthropic's request
//! JSON; [`AnthropicProvider`](mirri_kosong::AnthropicProvider) owns the
//! transport and SSE parsing.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use base64::Engine as _;
use mirri_core::error::EngineResult;
use mirri_core::llm::{Llm, LlmChatParams, LlmEventStream, ModelCapability, ProviderErrorInfo};
use mirri_core::message::{ContentPart, ImageUrlData, Message, Role, ToolCall};
use mirri_core::profile::ThinkingEffort;
use mirri_core::tool::ToolDefinition;
use mirri_kosong::AnthropicProvider;
use mirri_persistence::provider_file_cache::{CachedProviderFile, ProviderFileCache};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use super::anthropic_files::AnthropicFiles;
use super::convert::spawn_provider_chat;
use super::media_ref::{format_fileref, parse_fileref};
use super::merge_user_messages::merge_consecutive_user_messages;
use super::tool_result_media::{OMITTED_AUDIO_PLACEHOLDER, OMITTED_VIDEO_PLACEHOLDER};

/// Default `max_tokens` for requests without extended thinking.
const DEFAULT_MAX_TOKENS: u64 = 8192;

/// The provider tag the materializer stamps into `fileref:` references
/// (see [`super::media_ref`]).
const FILEREF_PROVIDER: &str = "anthropic";

/// Map a canonical [`ThinkingEffort`] to an Anthropic
/// extended-thinking `budget_tokens` value. `Custom` efforts carry no
/// budget — the adaptive `output_config.effort` passthrough below carries
/// them instead.
fn thinking_budget_tokens(effort: &ThinkingEffort) -> Option<u64> {
    match effort {
        ThinkingEffort::None => Some(0),
        ThinkingEffort::Low => Some(1024),
        ThinkingEffort::Medium => Some(4096),
        ThinkingEffort::High => Some(32000),
        ThinkingEffort::Custom(_) => None,
    }
}

/// An [`Llm`] speaking the Anthropic Messages API wire format.
pub struct AnthropicLlm {
    /// The underlying HTTP transport, shared with the streaming task.
    provider: Arc<AnthropicProvider>,
    /// The media file cache namespace for this adapter's endpoint *and*
    /// credential — precomputed in the constructor where the api key is
    /// available (`ProviderFileCache::namespace`, F-T11a-1: two api keys
    /// against one shared gateway never share cache entries, since a file
    /// id uploaded under one account is invisible to the other).
    media_ns: String,
    /// The model serving this adapter (e.g. `"claude-sonnet-4-20250514"`).
    model: String,
    /// The system prompt, sent in the top-level `system` field.
    system_prompt: String,
    /// Extended-thinking configuration; `None` omits the `thinking` field.
    thinking_effort: Option<ThinkingEffort>,
    /// Model capability flags.
    capability: ModelCapability,
    /// The Files API upload channel for the media materializer (T11a).
    /// `None` keeps the adapter fully inline (the default).
    files: Option<AnthropicFiles>,
    /// The content-addressed provider file cache shared across adapters
    /// rooted at the same Mirri home.
    media_cache: Option<Arc<ProviderFileCache>>,
}

impl AnthropicLlm {
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
        let api_key = api_key.into();
        let base_url = base_url.into();
        let media_ns = ProviderFileCache::namespace(FILEREF_PROVIDER, &base_url, &api_key);
        Self {
            provider: Arc::new(
                AnthropicProvider::new(api_key, base_url).with_extra_headers(headers),
            ),
            media_ns,
            model: model.into(),
            system_prompt: system_prompt.into(),
            thinking_effort,
            capability: ModelCapability {
                image_in: false,
                video_in: false,
                audio_in: false,
                dynamically_loaded_tools: false,
            },
            files: None,
            media_cache: None,
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

    /// Attach the Files API upload channel and the media file cache,
    /// enabling the media materializer (T11a): inline `data:` image URIs in
    /// the transient chat params are rewritten onto `fileref:anthropic;<id>`
    /// references (uploaded once, deduplicated by content hash through
    /// `media_cache`). Without this builder the adapter stays fully inline.
    pub fn with_media_files(
        mut self,
        files: AnthropicFiles,
        media_cache: Arc<ProviderFileCache>,
    ) -> Self {
        self.files = Some(files);
        self.media_cache = Some(media_cache);
        self
    }

    /// Build the Anthropic Messages API request body for `params`.
    ///
    /// The translation is currently total; the `Result` return keeps the
    /// signature aligned with [`Llm::chat`] and leaves room for fallible
    /// conversions (a failure would surface as an engine provider error).
    fn build_request(&self, params: &LlmChatParams) -> EngineResult<Value> {
        let mut request = Map::new();
        request.insert("model".to_string(), Value::String(self.model.clone()));

        // Extended thinking: the budget is the per-effort constant, and
        // `max_tokens` must exceed the budget (Anthropic rejects
        // `budget_tokens >= max_tokens`), so the default 8192 becomes the
        // visible-response headroom on top of the budget.
        match &self.thinking_effort {
            // Model-declared effort (e.g. "max"/"xhigh") on adaptive models:
            // enabled thinking without a token budget, the effort riding the
            // `output_config.effort` field verbatim — the kosong budget-less
            // passthrough (`packages/kosong/src/providers/anthropic.ts:1208-1217`).
            Some(ThinkingEffort::Custom(effort)) => {
                request.insert("max_tokens".to_string(), Value::from(DEFAULT_MAX_TOKENS));
                request.insert("thinking".to_string(), json!({ "type": "enabled" }));
                request.insert("output_config".to_string(), json!({ "effort": effort }));
            }
            // An explicitly-disabled effort must produce the same wire as no
            // thinking configured at all: no `thinking` key (and no
            // `output_config`), just the plain `max_tokens`. Pinned as an
            // explicit arm so future arm insertions cannot swallow it into
            // the budget path (`budget_tokens: 0` is an invalid payload —
            // Anthropic requires `budget_tokens >= 1024`).
            Some(ThinkingEffort::None) => {
                request.insert("max_tokens".to_string(), Value::from(DEFAULT_MAX_TOKENS));
            }
            Some(effort) if thinking_budget_tokens(effort).is_some() => {
                let budget = thinking_budget_tokens(effort).expect("checked above");
                request.insert(
                    "max_tokens".to_string(),
                    Value::from(DEFAULT_MAX_TOKENS + budget),
                );
                request.insert(
                    "thinking".to_string(),
                    json!({ "type": "enabled", "budget_tokens": budget }),
                );
            }
            _ => {
                request.insert("max_tokens".to_string(), Value::from(DEFAULT_MAX_TOKENS));
            }
        }

        let system = self.collect_system_text(params);
        if !system.is_empty() {
            // The system prompt must travel as a block array (not a plain
            // string) so its single text block can carry an explicit prompt
            // cache breakpoint.
            request.insert(
                "system".to_string(),
                json!([{ "type": "text", "text": system, "cache_control": cache_control() }]),
            );
        }

        // Convert messages, then merge consecutive user messages into one
        // turn (before breakpoint injection, matching the TS adapter's order:
        // anthropic.ts merges at :980-1001 and injects at :1003-1005, so the
        // breakpoint lands on the final block of the merged turn). Strict
        // Anthropic-compatible backends reject consecutive user messages with
        // HTTP 400 ("roles must alternate"), while api.anthropic.com
        // concatenates them anyway — so merging is safe for native Anthropic
        // and required for strict backends. Consecutive user turns arise
        // naturally after compaction (kept user prompts + user-role summary +
        // injected reminders) and when a steer follows a tool result; the
        // shared helper applies the asymmetric merge rule.
        let mut messages = merge_consecutive_user_messages(
            convert_messages_to_anthropic(params)?,
            is_user_turn,
            is_tool_result_only,
            merge_user_turns,
        );
        inject_cache_control_on_last_block(&mut messages);
        request.insert("messages".to_string(), Value::Array(messages));

        let mut tools = convert_tools_to_anthropic(&params.tools);
        if let Some(last_tool) = tools.last_mut() {
            set_cache_control(last_tool);
        }
        if !tools.is_empty() {
            request.insert("tools".to_string(), Value::Array(tools));
        }

        request.insert("stream".to_string(), Value::Bool(true));

        // Fork-chain cache routing (TS parity: provider-manager.ts maps
        // promptCacheKey onto `metadata.user_id` for anthropic transports
        // — the cache-affinity / end-user id). Only minted sessions with a
        // cache root carry it; fresh/legacy sessions omit the field.
        if let Some(cache_root_id) = &params.cache_root_id {
            request.insert(
                "metadata".to_string(),
                json!({ "user_id": cache_root_id }),
            );
        }

        Ok(Value::Object(request))
    }

    /// The effective system prompt: the adapter's own prompt followed by any
    /// `System` messages' text, joined with blank lines. Sent in the
    /// top-level `system` field; `System` messages are skipped inline.
    fn collect_system_text(&self, params: &LlmChatParams) -> String {
        let mut parts = Vec::new();
        if !self.system_prompt.is_empty() {
            parts.push(self.system_prompt.clone());
        }
        for message in &params.messages {
            if message.role == Role::System {
                let text = message.text_content();
                if !text.is_empty() {
                    parts.push(text);
                }
            }
        }
        parts.join("\n\n")
    }
}

impl Llm for AnthropicLlm {
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

    /// The media materializer (T11a): rewrite inline `data:` image URIs in
    /// the *transient* params onto `fileref:anthropic;<file_id>` references,
    /// uploading unseen content once through the Files API and deduplicating
    /// by content hash through the provider file cache.
    ///
    /// Degradation is total: any failure (unsupported media type, upload
    /// error, cache read/write error) keeps the inline payload and is only
    /// warned — materialization never fails a turn (brief §4.5).
    fn materialize_media<'a>(
        &'a self,
        params: &'a mut LlmChatParams,
    ) -> Pin<Box<dyn Future<Output = EngineResult<()>> + Send + 'a>> {
        Box::pin(async move {
            let (Some(files), Some(media_cache)) = (self.files.as_ref(), self.media_cache.as_ref())
            else {
                // No files channel — fully inline, the historical behavior.
                return Ok(());
            };

            for message in &mut params.messages {
                for part in &mut message.content {
                    let ContentPart::ImageUrl { image_url, .. } = part else {
                        continue;
                    };
                    // Only inline data URIs are materializable; anything
                    // else (remote URLs, an already-rewritten fileref from
                    // a prior materialize pass) passes through untouched.
                    let Some((media_type, payload)) = parse_data_url(&image_url.url) else {
                        continue;
                    };
                    let data = match base64::engine::general_purpose::STANDARD.decode(payload) {
                        Ok(data) => data,
                        Err(error) => {
                            tracing::warn!(
                                %error,
                                "undecodable data URI; keeping the inline image"
                            );
                            continue;
                        }
                    };
                    let content_sha = sha256_hex(&data);

                    if let Some(cached) = media_cache.lookup(&self.media_ns, &content_sha) {
                        image_url.url = format_fileref(FILEREF_PROVIDER, &cached.file_id);
                        continue;
                    }

                    match files.upload_image(&data, &media_type, None).await {
                        Ok(uploaded) => {
                            media_cache.record(
                                &self.media_ns,
                                &content_sha,
                                CachedProviderFile {
                                    file_id: uploaded.file_id.clone(),
                                    mime_type: media_type.clone(),
                                    size: data.len() as u64,
                                    expires_at: uploaded.expires_at,
                                },
                            );
                            image_url.url = format_fileref(FILEREF_PROVIDER, &uploaded.file_id);
                        }
                        Err(error) => {
                            // Degrade to the inline payload (brief §4.5):
                            // materialization never fails the turn.
                            tracing::warn!(
                                %error,
                                "Anthropic media upload failed; keeping the inline data URI"
                            );
                        }
                    }
                }
            }
            Ok(())
        })
    }
}

/// The `cache_control` breakpoint payload for Anthropic prompt caching.
/// Built as a fresh `Value` per injection site (`Value` cannot be a const).
fn cache_control() -> Value {
    json!({ "type": "ephemeral" })
}

/// Content block types that accept a `cache_control` breakpoint (mirrors the
/// TS kosong adapter's `CACHEABLE_TYPES`). `thinking` is deliberately absent:
/// Anthropic does not accept cache breakpoints on thinking blocks.
fn block_type_is_cacheable(block: &Value) -> bool {
    matches!(
        block.get("type").and_then(Value::as_str),
        Some(
            "text"
                | "image"
                | "document"
                | "search_result"
                | "tool_use"
                | "tool_result"
                | "server_tool_use"
                | "web_search_tool_result"
        )
    )
}

/// Set an ephemeral `cache_control` breakpoint on a content block or tool
/// definition.
fn set_cache_control(target: &mut Value) {
    if let Value::Object(map) = target {
        map.insert("cache_control".to_string(), cache_control());
    }
}

/// Set a `cache_control` breakpoint on the last content block of the last
/// message, when that block's type is cacheable.
///
/// This ports the TS kosong adapter's breakpoint strategy verbatim: only the
/// literal last block is considered (no walk-back to an earlier cacheable
/// block), and the total number of breakpoints per request stays at three
/// (system block + last tool definition + last message block) — the TS
/// reference implements no multi-breakpoint upgrade logic against
/// Anthropic's four-breakpoint cap, so none is added here either.
fn inject_cache_control_on_last_block(messages: &mut [Value]) {
    let Some(content) = messages
        .last_mut()
        .and_then(|message| message.get_mut("content"))
        .and_then(Value::as_array_mut)
    else {
        return;
    };
    let Some(last_block) = content.last_mut() else {
        return;
    };
    if block_type_is_cacheable(last_block) {
        set_cache_control(last_block);
    }
}

/// Translate engine [`Message`]s into Anthropic `messages` array entries.
///
/// Mapping rules:
/// - `System` messages are folded into the top-level `system` field (done by
///   [`AnthropicLlm::collect_system_text`]) and skipped here.
/// - `User` messages become `{ role: "user", content: [...] }`; a
///   `tool_call_id`-carrying user message becomes a `tool_result` block
///   instead.
/// - `Assistant` messages carry `text`/`thinking` blocks followed by
///   `tool_use` blocks; messages with neither content nor tool calls are
///   dropped (Anthropic rejects empty assistant messages).
/// - `Tool` messages become `{ role: "user", content: [{ type:
///   "tool_result", ... }] }`.
/// - `partial` messages (interrupted mid-stream) are dropped — their
///   truncated content would corrupt the user/assistant alternation.
fn convert_messages_to_anthropic(params: &LlmChatParams) -> EngineResult<Vec<Value>> {
    let mut messages = Vec::new();
    for message in &params.messages {
        if message.partial {
            continue;
        }
        match message.role {
            Role::System => { /* folded into the top-level `system` field */ }
            Role::User => {
                if let Some(tool_call_id) = &message.tool_call_id {
                    messages.push(user_tool_result_message(tool_call_id, message));
                } else {
                    messages.push(json!({
                        "role": "user",
                        "content": anthropic_user_blocks(&message.content),
                    }));
                }
            }
            Role::Assistant => {
                let blocks = anthropic_assistant_blocks(message);
                if blocks.is_empty() {
                    continue;
                }
                messages.push(json!({ "role": "assistant", "content": blocks }));
            }
            Role::Tool => {
                let Some(tool_call_id) = &message.tool_call_id else {
                    continue;
                };
                messages.push(user_tool_result_message(tool_call_id, message));
            }
        }
    }
    Ok(messages)
}

/// Whether a converted wire message is a user-role turn (ports the kosong
/// adapter's `isUser` predicate).
fn is_user_turn(message: &Value) -> bool {
    message.get("role").and_then(Value::as_str) == Some("user")
}

/// Whether a user-role wire message carries only tool results and nothing
/// else (ports kosong's `isToolResultOnly`, anthropic.ts:362-367: role must
/// be user, content must be a non-empty array, and every block a
/// `tool_result`).
fn is_tool_result_only(message: &Value) -> bool {
    is_user_turn(message)
        && message
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|blocks| {
                !blocks.is_empty()
                    && blocks.iter().all(|block| {
                        block.get("type").and_then(Value::as_str) == Some("tool_result")
                    })
            })
}

/// Combine two consecutive user turns by concatenating their content blocks,
/// keeping the earlier turn's other fields (ports the kosong adapter's merge
/// op, anthropic.ts:993-999).
fn merge_user_turns(mut last: Value, next: Value) -> Value {
    if let (Some(target), Some(source)) = (
        last.get_mut("content").and_then(Value::as_array_mut),
        next.get("content").and_then(Value::as_array),
    ) {
        target.extend(source.iter().cloned());
    }
    last
}

/// `{ role: "user", content: [{ type: "tool_result", ... }] }` — the
/// Anthropic shape for a tool execution result. Used for both `Tool` messages
/// and `tool_call_id`-carrying `User` messages.
///
/// The `tool_result.content` is a block array: text plus `image` blocks (the
/// Messages API natively accepts rich blocks inside `tool_result.content`).
/// Audio has no input representation and video blocks are not yet emitted by
/// this adapter (user blocks skip them too) — both get omission placeholders
/// so the gap stays visible instead of silently vanishing.
fn user_tool_result_message(tool_call_id: &str, message: &Message) -> Value {
    let mut blocks: Vec<Value> = Vec::new();
    let text = message.text_content();
    if !text.is_empty() {
        blocks.push(json!({ "type": "text", "text": text }));
    }
    for part in &message.content {
        match part {
            ContentPart::ImageUrl { image_url, .. } => {
                blocks.push(anthropic_image_block(image_url));
            }
            ContentPart::AudioUrl { .. } => {
                push_omission_placeholder(&mut blocks, OMITTED_AUDIO_PLACEHOLDER);
            }
            ContentPart::VideoUrl { .. } => {
                push_omission_placeholder(&mut blocks, OMITTED_VIDEO_PLACEHOLDER);
            }
            _ => {}
        }
    }
    let content = if blocks.is_empty() {
        // Degenerate (no parts at all): keep the legacy plain-string shape.
        Value::String(message.text_content())
    } else {
        Value::Array(blocks)
    };
    json!({
        "role": "user",
        "content": [{
            "type": "tool_result",
            "tool_use_id": tool_call_id,
            "content": content,
        }],
    })
}

/// Push an omission placeholder text block, collapsing consecutive identical
/// placeholders (ports kosong `anthropic.ts` `toolResultToBlock`).
fn push_omission_placeholder(blocks: &mut Vec<Value>, placeholder: &str) {
    let already_last = blocks
        .last()
        .and_then(|b| b.get("type"))
        .and_then(Value::as_str)
        == Some("text")
        && blocks
            .last()
            .and_then(|b| b.get("text"))
            .and_then(Value::as_str)
            == Some(placeholder);
    if !already_last {
        blocks.push(json!({ "type": "text", "text": placeholder }));
    }
}

/// Map user-message content parts to Anthropic content blocks.
///
/// Images become `image` blocks with a `url` (remote) or `base64` (data URL)
/// source. Audio/video/thinking parts are not representable in Anthropic user
/// input; they are skipped, and a placeholder text block guards the empty
/// case so the `content` array stays non-empty (Anthropic rejects it).
fn anthropic_user_blocks(content: &[ContentPart]) -> Vec<Value> {
    let mut blocks = Vec::new();
    for part in super::convert::inline_file_attachments(content) {
        match part {
            ContentPart::Text { text } => blocks.push(json!({ "type": "text", "text": text })),
            ContentPart::ImageUrl { ref image_url, .. } => {
                blocks.push(anthropic_image_block(image_url))
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
            "text": "[content not representable in the Anthropic Messages API]",
        }));
    }
    blocks
}

/// Map an assistant-message content part / tool call to Anthropic content
/// blocks: `text`, `thinking`, and `tool_use` blocks in that order.
fn anthropic_assistant_blocks(message: &Message) -> Vec<Value> {
    let mut blocks = Vec::new();
    for part in &message.content {
        match part {
            ContentPart::Text { text } => {
                blocks.push(json!({ "type": "text", "text": text }));
            }
            ContentPart::Think { text } => {
                blocks.push(json!({ "type": "thinking", "thinking": text }));
            }
            _ => { /* images/audio/video are not valid assistant input */ }
        }
    }
    for call in &message.tool_calls {
        blocks.push(anthropic_tool_use_block(call));
    }
    blocks
}

/// `{ type: "tool_use", id, name, input }` — the arguments JSON string is
/// parsed back into an object (the engine stores it as a raw string).
fn anthropic_tool_use_block(call: &ToolCall) -> Value {
    json!({
        "type": "tool_use",
        "id": &call.id,
        "name": &call.function.name,
        "input": parse_arguments(&call.function.arguments),
    })
}

/// An Anthropic `image` block. Three source shapes:
/// - a `fileref:` URL (produced by the materializer) → `source.type =
///   "file"` with the file id (the Files API reference channel, T11a);
/// - a data URL (`data:<media_type>;base64,<data>`) → `source.type =
///   "base64"`;
/// - anything else (a remote URL) → `source.type = "url"`.
fn anthropic_image_block(image_url: &ImageUrlData) -> Value {
    if let Some(file_id) = parse_fileref(&image_url.url, FILEREF_PROVIDER) {
        json!({
            "type": "image",
            "source": { "type": "file", "file_id": file_id },
        })
    } else if let Some((media_type, data)) = parse_data_url(&image_url.url) {
        json!({
            "type": "image",
            "source": { "type": "base64", "media_type": media_type, "data": data },
        })
    } else {
        json!({
            "type": "image",
            "source": { "type": "url", "url": &image_url.url },
        })
    }
}

/// Split a `data:` URL into `(media_type, base64_data)`; `None` for anything
/// that is not a `data:<media_type>;base64,<data>` URL.
fn parse_data_url(url: &str) -> Option<(String, String)> {
    let rest = url.strip_prefix("data:")?;
    let (media_type, data) = rest.split_once(',')?;
    let media_type = media_type.strip_suffix(";base64")?;
    Some((media_type.to_string(), data.to_string()))
}

/// Lowercase hex sha256 of `data` — the content-addressed cache key.
fn sha256_hex(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Parse a tool-arguments JSON string into a `Value`, defaulting to an empty
/// object for empty, malformed, or non-object input (matching the kosong
/// dispatchers).
fn parse_arguments(arguments: &str) -> Value {
    serde_json::from_str::<Value>(arguments)
        .ok()
        .filter(|v| v.is_object())
        .unwrap_or_else(|| Value::Object(Map::new()))
}

/// Translate [`ToolDefinition`]s into Anthropic `tools` entries, skipping
/// deferred (progressive-disclosure) tools.
fn convert_tools_to_anthropic(tools: &[ToolDefinition]) -> Vec<Value> {
    tools
        .iter()
        .filter(|tool| !tool.deferred)
        .map(|tool| {
            json!({
                "name": &tool.name,
                "description": &tool.description,
                "input_schema": &tool.parameters,
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

    /// An [`LlmChatParams`] carrying one user message, for request-building
    /// tests that only care about the top-level shape.
    fn chat_params_with(messages: Vec<Message>, tools: Vec<ToolDefinition>) -> LlmChatParams {
        LlmChatParams {
            messages,
            tools,
            cancel: CancelToken::new(),
            request_log_fields: None,
            cache_root_id: None,
        }
    }

    fn test_llm() -> AnthropicLlm {
        AnthropicLlm::new(
            "test-api-key",
            "https://api.anthropic.com/v1",
            "claude-test",
            "You are a test assistant.",
            None,
        )
    }

    #[test]
    fn should_inject_cache_control_on_system_last_tool_and_last_tool_result_when_request_carries_all_breakpoints()
     {
        // Given an Anthropic-backed LLM with a system prompt, two tools, and
        // a conversation whose final message is a tool-result user turn
        let llm = test_llm();
        let params = chat_params_with(
            vec![
                Message::user("read /tmp"),
                Message {
                    role: Role::Assistant,
                    name: None,
                    content: vec![ContentPart::Text {
                        text: "Let me read it.".to_string(),
                    }],
                    tool_calls: vec![ToolCall {
                        id: "toolu_1".to_string(),
                        call_type: "function".to_string(),
                        function: ToolCallFunction {
                            name: "Read".to_string(),
                            arguments: r#"{"path":"/tmp"}"#.to_string(),
                        },
                    }],
                    tool_call_id: None,
                    partial: false,
                },
                Message::tool_result("toolu_1", "file contents"),
            ],
            vec![
                ToolDefinition {
                    name: "Read".to_string(),
                    description: "Read a file".to_string(),
                    parameters: json!({"type": "object"}),
                    deferred: false,
                },
                ToolDefinition {
                    name: "Grep".to_string(),
                    description: "Search a file".to_string(),
                    parameters: json!({"type": "object"}),
                    deferred: false,
                },
            ],
        );

        // When the request is built
        let request = llm.build_request(&params).expect("request builds");

        // Then the system prompt is a text block with an ephemeral breakpoint
        assert_eq!(request["system"][0]["type"], "text");
        assert_eq!(request["system"][0]["text"], "You are a test assistant.");
        assert_eq!(
            request["system"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );

        // And only the last tool definition carries a breakpoint
        assert!(request["tools"][0].get("cache_control").is_none());
        assert_eq!(
            request["tools"][1]["cache_control"],
            json!({"type": "ephemeral"})
        );

        // And the last message's final tool_result block carries a breakpoint
        // while earlier messages carry none
        let messages = request["messages"].as_array().expect("messages array");
        let last = messages.last().expect("last message");
        assert_eq!(last["role"], "user");
        let blocks = last["content"].as_array().expect("content array");
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[0]["cache_control"], json!({"type": "ephemeral"}));
        assert!(
            messages[..messages.len() - 1]
                .iter()
                .all(|m| !message_has_cache_control(m)),
            "no breakpoint on earlier messages"
        );

        // And exactly three breakpoints exist in the whole request body
        assert_eq!(count_cache_control(&request), 3);
    }

    #[test]
    fn should_inject_cache_root_id_as_metadata_user_id_when_params_carry_cache_root(
    ) {
        // Given an Anthropic-backed LLM and params whose cache root id
        // (the fork-chain prompt-cache lineage) is set
        let llm = test_llm();
        let mut params = chat_params_with(vec![Message::user("hi")], Vec::new());
        params.cache_root_id = Some("session-root".to_string());

        // When the request is built
        let request = llm.build_request(&params).expect("request builds");

        // Then the top-level metadata carries the cache root as `user_id`
        // (TS parity: provider-manager.ts maps promptCacheKey onto
        // `metadata.user_id` for anthropic transports)
        assert_eq!(
            request["metadata"]["user_id"],
            json!("session-root"),
            "the cache root id must ride metadata.user_id, got: {request}"
        );
    }

    #[test]
    fn should_leave_metadata_absent_when_params_carry_no_cache_root() {
        // Given an Anthropic-backed LLM and params without a cache root id
        let llm = test_llm();
        let params = chat_params_with(vec![Message::user("hi")], Vec::new());

        // When the request is built
        let request = llm.build_request(&params).expect("request builds");

        // Then no metadata object is emitted (a fresh/legacy session must
        // not start routing cache keys)
        assert!(
            request.get("metadata").is_none(),
            "no metadata must be emitted without a cache root, got: {request}"
        );
    }

    #[test]
    fn should_skip_last_message_breakpoint_when_final_block_is_not_cacheable() {
        // Deliberate parity with the TS kosong adapter, see decision D-3 in
        // `.superpowers/sdd/followup-completion/task-t12-brief.md`: only the
        // literal last block is considered (no walk-back to an earlier
        // cacheable block), exactly matching anthropic.ts:344-354. Whether
        // to walk back is a behavior enhancement registered in ROADMAP §5,
        // not a gap.
        //
        // Given a conversation ending in an assistant message whose only
        // block is thinking (thinking is not a cacheable block type)
        let llm = test_llm();
        let params = chat_params_with(
            vec![
                Message::user("hi"),
                Message {
                    role: Role::Assistant,
                    name: None,
                    content: vec![ContentPart::Think {
                        text: "pondering".to_string(),
                    }],
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                    partial: false,
                },
            ],
            vec![ToolDefinition {
                name: "Read".to_string(),
                description: "Read a file".to_string(),
                parameters: json!({"type": "object"}),
                deferred: false,
            }],
        );

        // When the request is built
        let request = llm.build_request(&params).expect("request builds");

        // Then the last message's thinking block carries no breakpoint
        let messages = request["messages"].as_array().expect("messages array");
        let last = messages.last().expect("last message");
        assert_eq!(last["content"][0]["type"], "thinking");
        assert!(last["content"][0].get("cache_control").is_none());

        // And only the system and last-tool breakpoints remain
        assert_eq!(count_cache_control(&request), 2);
    }

    // --- T12b: consecutive user message merge (F-2) + breakpoint nails ----

    #[test]
    fn should_merge_consecutive_user_text_messages_when_converting_to_anthropic() {
        // Given a compaction-shaped conversation with two consecutive text
        // user turns (kept prompt + user-role summary)
        let llm = test_llm();
        let params = chat_params_with(
            vec![
                Message::user("kept prompt"),
                Message::user("compaction summary"),
            ],
            vec![],
        );

        // When the request is built
        let request = llm.build_request(&params).expect("request builds");

        // Then the consecutive user turns are merged into a single user turn
        // whose blocks keep their original order
        let messages = request["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 1, "consecutive user turns must merge");
        assert_eq!(messages[0]["role"], "user");
        let blocks = messages[0]["content"].as_array().expect("content array");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "kept prompt");
        assert_eq!(blocks[1]["type"], "text");
        assert_eq!(blocks[1]["text"], "compaction summary");
    }

    #[test]
    fn should_bundle_parallel_tool_results_into_single_user_turn() {
        // Given an assistant turn with two parallel tool calls answered by
        // two separate tool-result messages
        let llm = test_llm();
        let assistant = Message {
            role: Role::Assistant,
            name: None,
            content: Vec::new(),
            tool_calls: vec![
                ToolCall {
                    id: "toolu_1".to_string(),
                    call_type: "function".to_string(),
                    function: ToolCallFunction {
                        name: "Read".to_string(),
                        arguments: r#"{"path":"/tmp/a"}"#.to_string(),
                    },
                },
                ToolCall {
                    id: "toolu_2".to_string(),
                    call_type: "function".to_string(),
                    function: ToolCallFunction {
                        name: "Read".to_string(),
                        arguments: r#"{"path":"/tmp/b"}"#.to_string(),
                    },
                },
            ],
            tool_call_id: None,
            partial: false,
        };
        let params = chat_params_with(
            vec![
                Message::user("read both files"),
                assistant,
                Message::tool_result("toolu_1", "contents of a"),
                Message::tool_result("toolu_2", "contents of b"),
            ],
            vec![],
        );

        // When the request is built
        let request = llm.build_request(&params).expect("request builds");

        // Then both tool results answer in one user turn — the
        // parallel-tool-use spec requires all results of parallel calls to
        // share a single user turn (the tool-result-only turn absorbs the
        // following tool-result-only turn)
        let messages = request["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 3, "user / assistant / one merged user turn");
        let merged = messages.last().expect("merged user turn");
        assert_eq!(merged["role"], "user");
        let blocks = merged["content"].as_array().expect("content array");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[0]["tool_use_id"], "toolu_1");
        assert_eq!(
            blocks[0]["content"],
            json!([{ "type": "text", "text": "contents of a" }])
        );
        assert_eq!(blocks[1]["type"], "tool_result");
        assert_eq!(blocks[1]["tool_use_id"], "toolu_2");
        assert_eq!(
            blocks[1]["content"],
            json!([{ "type": "text", "text": "contents of b" }])
        );
    }

    #[test]
    fn should_not_merge_text_turn_with_leading_tool_result_turn() {
        // Given a text user turn directly followed by a tool-result turn
        // (the asymmetric rule's reverse case: a text turn never absorbs a
        // tool result, which must answer the preceding assistant tool_use)
        let llm = test_llm();
        let params = chat_params_with(
            vec![
                Message::user("a text turn"),
                Message::tool_result("toolu_1", "file contents"),
            ],
            vec![],
        );

        // When the request is built
        let request = llm.build_request(&params).expect("request builds");

        // Then the two turns stay separate
        let messages = request["messages"].as_array().expect("messages array");
        assert_eq!(
            messages.len(),
            2,
            "text and tool-result turns must not merge"
        );
        assert_eq!(messages[0]["content"][0]["type"], "text");
        assert_eq!(messages[1]["content"][0]["type"], "tool_result");

        // And given the mirrored shape (a steer text turn following a tool
        // result after a tool use), the tool-result-only turn does absorb
        // the following text — the asymmetry only guards the reverse order
        let assistant = Message {
            role: Role::Assistant,
            name: None,
            content: Vec::new(),
            tool_calls: vec![ToolCall {
                id: "toolu_1".to_string(),
                call_type: "function".to_string(),
                function: ToolCallFunction {
                    name: "Read".to_string(),
                    arguments: r#"{"path":"/tmp"}"#.to_string(),
                },
            }],
            tool_call_id: None,
            partial: false,
        };
        let params = chat_params_with(
            vec![
                Message::user("read /tmp"),
                assistant,
                Message::tool_result("toolu_1", "file contents"),
                Message::user("actually, stop there"),
            ],
            vec![],
        );

        // When that request is built
        let request = llm.build_request(&params).expect("request builds");

        // Then the steer text is folded into the tool-result turn, yielding
        // a valid [tool_result, text] user turn
        let messages = request["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 3);
        let merged = messages.last().expect("merged user turn");
        assert_eq!(merged["role"], "user");
        let blocks = merged["content"].as_array().expect("content array");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[1]["type"], "text");
        assert_eq!(blocks[1]["text"], "actually, stop there");
    }

    #[test]
    fn should_place_breakpoint_on_last_block_of_merged_user_turn() {
        // Given a full-shape request (system prompt + two tools) whose
        // conversation ends in two consecutive text user turns
        let llm = test_llm();
        let params = chat_params_with(
            vec![Message::user("first turn"), Message::user("second turn")],
            vec![
                ToolDefinition {
                    name: "Read".to_string(),
                    description: "Read a file".to_string(),
                    parameters: json!({"type": "object"}),
                    deferred: false,
                },
                ToolDefinition {
                    name: "Grep".to_string(),
                    description: "Search a file".to_string(),
                    parameters: json!({"type": "object"}),
                    deferred: false,
                },
            ],
        );

        // When the request is built
        let request = llm.build_request(&params).expect("request builds");

        // Then the message breakpoint lands on the last block of the merged
        // user turn (injection runs after the merge, mirroring the TS order)
        let messages = request["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 1, "the two user turns merged");
        let blocks = messages[0]["content"].as_array().expect("content array");
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].get("cache_control").is_none());
        assert_eq!(blocks[1]["cache_control"], json!({"type": "ephemeral"}));

        // And the total breakpoint count stays at three
        assert_eq!(count_cache_control(&request), 3);
    }

    #[test]
    fn should_never_exceed_four_cache_control_breakpoints_in_request() {
        // Given a request carrying every breakpoint shape the adapter
        // injects: a system prompt, tools, a merged multi-block user turn,
        // and a final tool-result turn
        let llm = test_llm();
        let assistant = Message {
            role: Role::Assistant,
            name: None,
            content: Vec::new(),
            tool_calls: vec![ToolCall {
                id: "toolu_1".to_string(),
                call_type: "function".to_string(),
                function: ToolCallFunction {
                    name: "Read".to_string(),
                    arguments: r#"{"path":"/tmp"}"#.to_string(),
                },
            }],
            tool_call_id: None,
            partial: false,
        };
        let params = chat_params_with(
            vec![
                Message::user("kept prompt"),
                Message::user("compaction summary"),
                assistant,
                Message::tool_result("toolu_1", "file contents"),
                Message::user("a steer after the tool result"),
            ],
            vec![
                ToolDefinition {
                    name: "Read".to_string(),
                    description: "Read a file".to_string(),
                    parameters: json!({"type": "object"}),
                    deferred: false,
                },
                ToolDefinition {
                    name: "Grep".to_string(),
                    description: "Search a file".to_string(),
                    parameters: json!({"type": "object"}),
                    deferred: false,
                },
            ],
        );

        // When the request is built
        let request = llm.build_request(&params).expect("request builds");

        // Then the total number of cache_control breakpoints never exceeds
        // Anthropic's four-breakpoint cap (F-1 nail: the adapter injects a
        // fixed three — system block, last tool definition, last message
        // block — with no user-configurable breakpoint surface, so no
        // downgrade path can ever be needed; the current value is pinned
        // by the dedicated three-breakpoint tests above)
        let count = count_cache_control(&request);
        assert!(
            count <= 4,
            "cache_control breakpoints must never exceed Anthropic's cap of 4, got {count}"
        );
        assert_eq!(count, 3);
    }

    #[test]
    fn should_build_anthropic_request_when_converting_messages_and_tools() {
        // Given an Anthropic-backed LLM and a full conversation including a
        // system message, a tool-using assistant message, a tool result, and
        // one deferred and one active tool
        let llm = test_llm();
        let params = chat_params_with(
            vec![
                Message::system("Be brief."),
                Message::user("read /tmp"),
                Message {
                    role: Role::Assistant,
                    name: None,
                    content: vec![ContentPart::Text {
                        text: "Let me read it.".to_string(),
                    }],
                    tool_calls: vec![ToolCall {
                        id: "toolu_1".to_string(),
                        call_type: "function".to_string(),
                        function: ToolCallFunction {
                            name: "Read".to_string(),
                            arguments: r#"{"path":"/tmp"}"#.to_string(),
                        },
                    }],
                    tool_call_id: None,
                    partial: false,
                },
                Message::tool_result("toolu_1", "file contents"),
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

        // When the Anthropic request is built
        let request = llm.build_request(&params).expect("request builds");

        // Then the top-level fields carry the model, prompt, stream flag, and
        // the default max_tokens
        assert_eq!(request["model"], "claude-test");
        assert_eq!(request["system"][0]["type"], "text");
        assert_eq!(
            request["system"][0]["text"],
            "You are a test assistant.\n\nBe brief."
        );
        assert_eq!(
            request["system"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
        assert_eq!(request["max_tokens"], 8192);
        assert_eq!(request["stream"], true);
        assert!(
            request.get("thinking").is_none(),
            "no thinking without effort"
        );

        // And system messages are not duplicated in the messages array
        let messages = request["messages"].as_array().expect("messages array");
        assert_eq!(messages[0]["role"], "user");
        assert_eq!(messages[0]["content"][0]["type"], "text");
        assert_eq!(messages[0]["content"][0]["text"], "read /tmp");

        // And the assistant message carries text then tool_use blocks
        let assistant = &messages[1];
        assert_eq!(assistant["role"], "assistant");
        assert_eq!(assistant["content"][0]["type"], "text");
        assert_eq!(assistant["content"][1]["type"], "tool_use");
        assert_eq!(assistant["content"][1]["id"], "toolu_1");
        assert_eq!(assistant["content"][1]["name"], "Read");
        assert_eq!(assistant["content"][1]["input"], json!({"path": "/tmp"}));

        // And the tool result is a user message with a tool_result block
        let tool_result = &messages[2];
        assert_eq!(tool_result["role"], "user");
        assert_eq!(tool_result["content"][0]["type"], "tool_result");
        assert_eq!(tool_result["content"][0]["tool_use_id"], "toolu_1");
        assert_eq!(
            tool_result["content"][0]["content"],
            json!([{ "type": "text", "text": "file contents" }])
        );

        // And only the non-deferred tool is advertised
        let tools = request["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "Read");
        assert_eq!(tools[0]["input_schema"], json!({"type": "object"}));
    }

    #[test]
    fn should_set_thinking_budget_and_scale_max_tokens_when_thinking_effort_is_low() {
        // Given an Anthropic-backed LLM with Low extended thinking
        let llm = AnthropicLlm::new(
            "test-api-key",
            "https://api.anthropic.com/v1",
            "claude-test",
            "",
            Some(ThinkingEffort::Low),
        );
        let params = chat_params_with(vec![Message::user("hi")], vec![]);

        // When the request is built
        let request = llm.build_request(&params).expect("request builds");

        // Then extended thinking is enabled with the Low budget and max_tokens
        // leaves 8192 headroom above the budget
        assert_eq!(
            request["thinking"],
            json!({"type": "enabled", "budget_tokens": 1024})
        );
        assert_eq!(request["max_tokens"], 8192 + 1024);
    }

    #[test]
    fn should_enable_budgetless_thinking_with_output_config_effort_when_effort_is_model_declared() {
        // Given an Anthropic-backed LLM with a model-declared custom effort
        // (e.g. "max" — a level the canonical enum does not spell)
        let llm = AnthropicLlm::new(
            "test-api-key",
            "https://api.anthropic.com/v1",
            "claude-test",
            "",
            Some(ThinkingEffort::Custom("max".to_string())),
        );

        // When the request is built
        let request = llm
            .build_request(&chat_params_with(vec![Message::user("hi")], vec![]))
            .expect("request builds");

        // Model-declared efforts on adaptive models: thinking enabled without a
        // token budget, the effort riding output_config.effort verbatim.
        assert_eq!(
            request["thinking"],
            serde_json::json!({ "type": "enabled" })
        );
        assert_eq!(
            request["output_config"],
            serde_json::json!({ "effort": "max" })
        );
        assert_eq!(request["max_tokens"], serde_json::json!(8192));
    }

    #[test]
    fn should_send_no_thinking_key_when_the_effort_is_explicitly_none() {
        // Given an Anthropic-backed LLM with an explicitly-disabled effort
        let llm = AnthropicLlm::new(
            "test-api-key",
            "https://api.anthropic.com/v1",
            "claude-test",
            "",
            Some(ThinkingEffort::None),
        );

        // When the request is built
        let request = llm
            .build_request(&chat_params_with(vec![Message::user("hi")], vec![]))
            .expect("request builds");

        // Then no thinking intent is sent at all — the same wire as no
        // thinking configured, with the plain max_tokens
        assert!(
            request.get("thinking").is_none(),
            "explicitly-disabled effort must not send a thinking key"
        );
        assert!(
            request.get("output_config").is_none(),
            "explicitly-disabled effort must not send output_config"
        );
        assert_eq!(request["max_tokens"], serde_json::json!(8192));
    }

    #[test]
    fn should_map_high_effort_to_largest_budget_when_thinking_effort_is_high() {
        // Given an Anthropic-backed LLM with High extended thinking
        let llm = AnthropicLlm::new(
            "test-api-key",
            "https://api.anthropic.com/v1",
            "claude-test",
            "",
            Some(ThinkingEffort::High),
        );

        // When the request is built
        let request = llm
            .build_request(&chat_params_with(vec![Message::user("hi")], vec![]))
            .expect("request builds");

        // Then the High budget is applied with matching max_tokens headroom
        assert_eq!(request["thinking"]["budget_tokens"], 32000);
        assert_eq!(request["max_tokens"], 8192 + 32000);
    }

    #[test]
    fn should_emit_base64_image_source_when_user_message_contains_data_url_image() {
        // Given a user message with a data-URL image part
        let llm = test_llm();
        let params = chat_params_with(
            vec![Message {
                role: Role::User,
                name: None,
                content: vec![ContentPart::ImageUrl {
                    image_url: ImageUrlData {
                        url: "data:image/png;base64,iVBORw0KGgo=".to_string(),
                        detail: None,
                        id: None,
                    },
                    detail: None,
                }],
                tool_calls: Vec::new(),
                tool_call_id: None,
                partial: false,
            }],
            vec![],
        );

        // When the request is built
        let request = llm.build_request(&params).expect("request builds");

        // Then the image block uses a base64 source split from the data URL
        let image = &request["messages"][0]["content"][0];
        assert_eq!(image["type"], "image");
        assert_eq!(image["source"]["type"], "base64");
        assert_eq!(image["source"]["media_type"], "image/png");
        assert_eq!(image["source"]["data"], "iVBORw0KGgo=");
    }

    #[test]
    fn should_emit_url_image_source_when_user_message_contains_remote_image() {
        // Given a user message with a remote image part
        let llm = test_llm();
        let params = chat_params_with(
            vec![Message {
                role: Role::User,
                name: None,
                content: vec![ContentPart::ImageUrl {
                    image_url: ImageUrlData {
                        url: "https://example.com/cat.png".to_string(),
                        detail: None,
                        id: None,
                    },
                    detail: None,
                }],
                tool_calls: Vec::new(),
                tool_call_id: None,
                partial: false,
            }],
            vec![],
        );

        // When the request is built
        let request = llm.build_request(&params).expect("request builds");

        // Then the image block uses a url source
        let image = &request["messages"][0]["content"][0];
        assert_eq!(image["type"], "image");
        assert_eq!(image["source"]["type"], "url");
        assert_eq!(image["source"]["url"], "https://example.com/cat.png");
    }

    #[test]
    fn should_skip_partial_messages_when_converting_conversation() {
        // Given a conversation whose last assistant message is a partial
        // (interrupted) stream
        let llm = test_llm();
        let params = chat_params_with(
            vec![
                Message::user("hi"),
                Message {
                    role: Role::Assistant,
                    name: None,
                    content: vec![ContentPart::Text {
                        text: "truncated".to_string(),
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

        // Then the partial message is dropped from the messages array
        let messages = request["messages"].as_array().expect("messages array");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
    }

    #[tokio::test]
    async fn should_stream_text_deltas_and_part_when_anthropic_returns_sse_response() {
        // Given a mock Anthropic endpoint streaming a text-only response
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
                         event: content_block_delta\n\
                         data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\" world\"}}\n\n\
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

        // And an Anthropic-backed LLM pointed at the mock
        let llm = AnthropicLlm::new("test-api-key", format!("{}/v1", server.uri()), "claude-test", "", None);
        let params = chat_params_with(vec![Message::user("hi")], vec![]);

        // When the chat stream is consumed to EOF
        let mut stream = llm.chat(params).expect("chat starts");
        let events = drain(&mut stream).await;

        // Then deltas are forwarded incrementally, the accumulated text is
        // flushed as a block-level part at Done, and usage from both
        // message_start and message_delta is surfaced in order
        let tags: Vec<String> = events.iter().map(event_tag).collect();
        assert_eq!(
            tags,
            [
                "usage:10:0:0:0",
                "text_delta:Hello",
                "text_delta: world",
                "usage:0:10:0:0",
                "text_part:Hello world",
                "done:end_turn",
            ]
        );
    }

    #[tokio::test]
    async fn should_emit_think_deltas_and_think_part_when_anthropic_streams_thinking() {
        // Given a mock Anthropic endpoint streaming a thinking block followed
        // by a text block
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(
                        "event: content_block_start\n\
                         data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n\
                         event: content_block_delta\n\
                         data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"Let me \"}}\n\n\
                         event: content_block_delta\n\
                         data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"think.\"}}\n\n\
                         event: content_block_stop\n\
                         data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
                         event: content_block_start\n\
                         data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
                         event: content_block_delta\n\
                         data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"The answer\"}}\n\n\
                         event: content_block_stop\n\
                         data: {\"type\":\"content_block_stop\",\"index\":1}\n\n\
                         event: message_delta\n\
                         data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":9}}\n\n\
                         event: message_stop\n\
                         data: {\"type\":\"message_stop\"}\n\n",
                    ),
            )
            .mount(&server)
            .await;

        // And an Anthropic-backed LLM pointed at the mock
        let llm = AnthropicLlm::new("test-api-key", format!("{}/v1", server.uri()), "claude-test", "", None);

        // When the chat stream is consumed
        let mut stream = llm
            .chat(chat_params_with(vec![Message::user("hi")], vec![]))
            .expect("chat starts");
        let events = drain(&mut stream).await;

        // Then thinking and text deltas are forwarded and the block-level
        // transcript carries the flushed think part before the done event
        let think_deltas: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                LlmEvent::ThinkDelta { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(think_deltas, vec!["Let me ", "think."]);
        assert!(events.iter().any(|e| matches!(
            e,
            LlmEvent::ThinkPart { text } if text == "Let me think."
        )));
        assert!(events.iter().any(|e| matches!(
            e,
            LlmEvent::TextPart { text } if text == "The answer"
        )));
        assert!(matches!(
            events.last(),
            Some(LlmEvent::Done { stop_reason }) if stop_reason == "end_turn"
        ));
    }

    #[tokio::test]
    async fn should_flush_text_part_before_tool_call_when_anthropic_streams_text_then_tool_use() {
        // Given a mock Anthropic endpoint streaming a text block followed by a
        // tool-use block
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(
                        "event: content_block_start\n\
                         data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
                         event: content_block_delta\n\
                         data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Let me read that.\"}}\n\n\
                         event: content_block_stop\n\
                         data: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
                         event: content_block_start\n\
                         data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_2\",\"name\":\"Read\",\"input\":{}}}\n\n\
                         event: content_block_delta\n\
                         data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"path\\\":\\\"/tmp/test\\\"}\"}}\n\n\
                         event: content_block_stop\n\
                         data: {\"type\":\"content_block_stop\",\"index\":1}\n\n\
                         event: message_delta\n\
                         data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":25}}\n\n\
                         event: message_stop\n\
                         data: {\"type\":\"message_stop\"}\n\n",
                    ),
            )
            .mount(&server)
            .await;

        // And an Anthropic-backed LLM pointed at the mock
        let llm = AnthropicLlm::new("test-api-key", format!("{}/v1", server.uri()), "claude-test", "", None);

        // When the chat stream is consumed
        let mut stream = llm
            .chat(chat_params_with(
                vec![Message::user("read /tmp/test")],
                vec![],
            ))
            .expect("chat starts");
        let events = drain(&mut stream).await;

        // Then the accumulated text is flushed as a TextPart *before* the
        // ToolCall, which precedes the Done event
        let text_part_index = events
            .iter()
            .position(|e| matches!(e, LlmEvent::TextPart { .. }))
            .expect("text part present");
        let tool_call_index = events
            .iter()
            .position(|e| matches!(e, LlmEvent::ToolCall { .. }))
            .expect("tool call present");
        let done_index = events
            .iter()
            .position(|e| matches!(e, LlmEvent::Done { .. }))
            .expect("done present");
        assert!(text_part_index < tool_call_index);
        assert!(tool_call_index < done_index);

        let tools: Vec<&LlmEvent> = events
            .iter()
            .filter(|e| matches!(e, LlmEvent::ToolCall { .. }))
            .collect();
        assert_eq!(tools.len(), 1);
        match tools[0] {
            LlmEvent::ToolCall {
                tool,
                args,
                call_id,
            } => {
                assert_eq!(tool, "Read");
                assert_eq!(args["path"], "/tmp/test");
                assert_eq!(call_id, "toolu_2");
            }
            _ => unreachable!(),
        }
        assert!(matches!(
            events.last(),
            Some(LlmEvent::Done { stop_reason }) if stop_reason == "tool_use"
        ));
    }

    #[tokio::test]
    async fn should_emit_error_event_when_anthropic_provider_returns_error_status() {
        // Given a mock Anthropic endpoint answering 500
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(500).set_body_string(
                r#"{"type":"error","error":{"type":"overloaded_error","message":"boom"}}"#,
            ))
            .mount(&server)
            .await;

        // And an Anthropic-backed LLM pointed at the mock
        let llm = AnthropicLlm::new("test-api-key", format!("{}/v1", server.uri()), "claude-test", "", None);

        // When the chat stream is consumed
        let mut stream = llm
            .chat(chat_params_with(vec![Message::user("hi")], vec![]))
            .expect("chat starts");
        let events = drain(&mut stream).await;

        // Then exactly one error event carrying the status is emitted
        assert!(matches!(&events[..], [LlmEvent::Error { message }] if message.contains("500")));
    }

    #[tokio::test]
    async fn should_yield_no_events_when_cancel_token_is_already_cancelled() {
        // Given a mock Anthropic endpoint streaming a valid response
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(
                        "event: message_start\n\
                         data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-sonnet-4-20250514\",\"usage\":{\"input_tokens\":1}}}\n\n\
                         event: content_block_delta\n\
                         data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n\
                         event: message_delta\n\
                         data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
                    ),
            )
            .mount(&server)
            .await;

        // And a cancel token that is cancelled before the request starts
        let cancel = CancelToken::new();
        cancel.cancel();
        let params = LlmChatParams {
            messages: vec![Message::user("hi")],
            tools: vec![],
            cancel,
            request_log_fields: None,
            cache_root_id: None,
        };

        // When the chat stream is consumed to EOF
        let mut stream = llm_with_server(&server).chat(params).expect("chat starts");
        let events = drain(&mut stream).await;

        // Then the pump stops before forwarding anything
        assert!(events.is_empty());
    }

    #[tokio::test]
    async fn should_send_cache_control_breakpoints_on_the_wire_when_streaming_a_chat() {
        // Given a mock Anthropic endpoint streaming a minimal SSE response
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(
                        "event: message_start\n\
                         data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-test\",\"usage\":{\"input_tokens\":10}}}\n\n\
                         event: message_delta\n\
                         data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n\
                         event: message_stop\n\
                         data: {\"type\":\"message_stop\"}\n\n",
                    ),
            )
            .mount(&server)
            .await;

        // And an Anthropic-backed LLM with a system prompt, two tools, and a
        // tool-result conversation pointed at the mock
        let llm = AnthropicLlm::new(
            "test-api-key",
            format!("{}/v1", server.uri()),
            "claude-test",
            "You are a test assistant.",
            None,
        );
        let params = chat_params_with(
            vec![
                Message::user("read /tmp"),
                Message {
                    role: Role::Assistant,
                    name: None,
                    content: vec![ContentPart::Text {
                        text: "Let me read it.".to_string(),
                    }],
                    tool_calls: vec![ToolCall {
                        id: "toolu_1".to_string(),
                        call_type: "function".to_string(),
                        function: ToolCallFunction {
                            name: "Read".to_string(),
                            arguments: r#"{"path":"/tmp"}"#.to_string(),
                        },
                    }],
                    tool_call_id: None,
                    partial: false,
                },
                Message::tool_result("toolu_1", "file contents"),
            ],
            vec![
                ToolDefinition {
                    name: "Read".to_string(),
                    description: "Read a file".to_string(),
                    parameters: json!({"type": "object"}),
                    deferred: false,
                },
                ToolDefinition {
                    name: "Grep".to_string(),
                    description: "Search a file".to_string(),
                    parameters: json!({"type": "object"}),
                    deferred: false,
                },
            ],
        );

        // When the chat stream is consumed
        let mut stream = llm.chat(params).expect("chat starts");
        let events = drain(&mut stream).await;
        assert!(
            !events.iter().any(|e| matches!(e, LlmEvent::Error { .. })),
            "chat should not error"
        );

        // Then the request body sent over the wire carries exactly the three
        // cache_control breakpoints: the system text block, the last tool
        // definition, and the final tool_result block of the last message
        // (request-body evidence for the cache-observability contract)
        let requests = server.received_requests().await.expect("recorded requests");
        let body: Value = requests[0].body_json().expect("request body is valid JSON");
        assert_eq!(
            body["system"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
        let tools = body["tools"].as_array().expect("tools array");
        assert!(tools[0].get("cache_control").is_none());
        assert_eq!(tools[1]["cache_control"], json!({"type": "ephemeral"}));
        let messages = body["messages"].as_array().expect("messages array");
        let blocks = messages.last().expect("last message")["content"]
            .as_array()
            .expect("content array");
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[0]["cache_control"], json!({"type": "ephemeral"}));
        assert_eq!(count_cache_control(&body), 3);
    }

    #[tokio::test]
    async fn should_carry_cache_buckets_through_the_stream_when_message_start_reports_them() {
        // Given a mock Anthropic endpoint whose message_start usage carries
        // the prompt-cache read and creation buckets (the response to the
        // cache_control breakpoints P1a injects)
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(
                        "event: message_start\n\
                         data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-test\",\"usage\":{\"input_tokens\":15,\"cache_read_input_tokens\":200,\"cache_creation_input_tokens\":40}}}\n\n\
                         event: content_block_delta\n\
                         data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n\
                         event: message_delta\n\
                         data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n\
                         event: message_stop\n\
                         data: {\"type\":\"message_stop\"}\n\n",
                    ),
            )
            .mount(&server)
            .await;

        // And an Anthropic-backed LLM pointed at the mock
        let llm = AnthropicLlm::new(
            "test-api-key",
            format!("{}/v1", server.uri()),
            "claude-test",
            "You are a test assistant.",
            None,
        );

        // When the chat stream is consumed to EOF
        let mut stream = llm
            .chat(chat_params_with(vec![Message::user("hi")], vec![]))
            .expect("chat starts");
        let events = drain(&mut stream).await;

        // Then both Usage events carry the cache buckets through the kosong
        // → LlmEvent pump: the message_start buckets survive alongside the
        // split output-only accounting of message_delta
        let tags: Vec<String> = events.iter().map(event_tag).collect();
        assert_eq!(
            tags,
            [
                "usage:15:0:200:40",
                "text_delta:hi",
                "usage:0:5:0:0",
                "text_part:hi",
                "done:end_turn",
            ]
        );
    }

    // --- T11a: media materialization + the file-reference channel --------

    use std::path::Path;

    use mirri_persistence::provider_file_cache::ProviderFileCache;

    use super::super::anthropic_files::AnthropicFiles;

    /// A tiny inline PNG data URI (the base64 payload decodes to the PNG
    /// magic bytes — the mock endpoints never decode it).
    fn image_data_uri() -> String {
        "data:image/png;base64,iVBORw0KGgo=".to_string()
    }

    /// A user message pairing a text part with an inline image.
    fn image_message(url: String) -> Message {
        Message {
            role: Role::User,
            name: None,
            content: vec![
                ContentPart::Text {
                    text: "describe this".to_string(),
                },
                ContentPart::ImageUrl {
                    image_url: ImageUrlData {
                        url,
                        detail: None,
                        id: None,
                    },
                    detail: None,
                },
            ],
            tool_calls: Vec::new(),
            tool_call_id: None,
            partial: false,
        }
    }

    /// An Anthropic-backed LLM with the media channel (Files API client +
    /// provider file cache rooted at `cache_root`) pointed at `server`.
    fn media_llm(server: &MockServer, cache_root: &Path) -> AnthropicLlm {
        media_llm_with_key(server, cache_root, "test-api-key")
    }

    /// An [`AnthropicLlm`] with the media channel authenticated with
    /// `api_key` — the cache namespace is scoped by endpoint *and* key
    /// (F-T11a-1), so two keys at one shared endpoint hold disjoint
    /// entries even over one cache root.
    fn media_llm_with_key(server: &MockServer, cache_root: &Path, api_key: &str) -> AnthropicLlm {
        AnthropicLlm::new(
            api_key,
            format!("{}/v1", server.uri()),
            "claude-test",
            "You are a test assistant.",
            None,
        )
        .with_media_files(
            AnthropicFiles::new(api_key, format!("{}/v1", server.uri())),
            std::sync::Arc::new(ProviderFileCache::new(cache_root.to_path_buf())),
        )
    }

    /// Mount a mock Files API upload answering `id`, and a mock Messages
    /// endpoint streaming a minimal SSE response.
    async fn mount_files_and_messages(server: &MockServer, file_id: &str, status: u16) {
        Mock::given(method("POST"))
            .and(path("/v1/files"))
            .respond_with(
                ResponseTemplate::new(status)
                    .set_body_json(serde_json::json!({ "id": file_id, "type": "file" })),
            )
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(
                        "event: message_start\n\
                         data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-test\",\"usage\":{\"input_tokens\":10}}}\n\n\
                         event: message_delta\n\
                         data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n\
                         event: message_stop\n\
                         data: {\"type\":\"message_stop\"}\n\n",
                    ),
            )
            .mount(server)
            .await;
    }

    /// The number of upload POSTs the mock received on `/v1/files`.
    async fn files_upload_count(server: &MockServer) -> usize {
        server
            .received_requests()
            .await
            .expect("recorded requests")
            .iter()
            .filter(|request| request.url.path() == "/v1/files")
            .count()
    }

    /// The `/v1/messages` request body the mock received (exactly one).
    async fn sole_chat_body(server: &MockServer) -> Value {
        let requests = server.received_requests().await.expect("recorded requests");
        let request = requests
            .iter()
            .find(|request| request.url.path() == "/v1/messages")
            .expect("a chat request reached the mock");
        request.body_json().expect("chat body is valid JSON")
    }

    #[tokio::test]
    async fn should_emit_a_file_source_without_base64_when_an_image_is_materialized() {
        // Given a mock Anthropic files + messages endpoint and an LLM with
        // the media channel
        let server = MockServer::start().await;
        mount_files_and_messages(&server, "file-x", 200).await;
        let cache_dir = tempfile::tempdir().expect("tempdir");
        let llm = media_llm(&server, cache_dir.path());

        // And params carrying an inline image with the full breakpoint set
        // (system prompt + two tools + a final image block)
        let mut params = chat_params_with(
            vec![image_message(image_data_uri())],
            vec![
                ToolDefinition {
                    name: "Read".to_string(),
                    description: "Read a file".to_string(),
                    parameters: json!({"type": "object"}),
                    deferred: false,
                },
                ToolDefinition {
                    name: "Grep".to_string(),
                    description: "Search a file".to_string(),
                    parameters: json!({}),
                    deferred: false,
                },
            ],
        );

        // When the media is materialized and the chat stream is consumed
        llm.materialize_media(&mut params)
            .await
            .expect("materialize succeeds");
        let mut stream = llm.chat(params).expect("chat starts");
        let events = drain(&mut stream).await;
        assert!(
            !events.iter().any(|e| matches!(e, LlmEvent::Error { .. })),
            "chat should not error"
        );

        // Then the image was uploaded once and the chat body referenced it
        // as a file source with no base64 payload anywhere
        assert_eq!(files_upload_count(&server).await, 1);
        let body = sole_chat_body(&server).await;
        let image = &body["messages"][0]["content"][1];
        assert_eq!(image["type"], "image");
        assert_eq!(image["source"]["type"], "file");
        assert_eq!(image["source"]["file_id"], "file-x");
        assert!(image["source"].get("data").is_none());
        let serialized = body.to_string();
        assert!(
            !serialized.contains("iVBORw0KGgo"),
            "no base64 payload may survive materialization: {serialized}"
        );

        // And the three cache_control breakpoints are unchanged
        assert_eq!(count_cache_control(&body), 3);
    }

    #[tokio::test]
    async fn should_not_reupload_the_same_image_across_turns_or_instances() {
        // Given a mock endpoint and a shared cache root
        let server = MockServer::start().await;
        mount_files_and_messages(&server, "file-x", 200).await;
        let cache_dir = tempfile::tempdir().expect("tempdir");
        let llm = media_llm(&server, cache_dir.path());

        // When the same image goes through two materialize+chat turns
        for _ in 0..2 {
            let mut params = chat_params_with(vec![image_message(image_data_uri())], vec![]);
            llm.materialize_media(&mut params)
                .await
                .expect("materialize succeeds");
            let mut stream = llm.chat(params).expect("chat starts");
            drain(&mut stream).await;
        }

        // Then the image was uploaded exactly once (the second turn hit the
        // content-addressed cache)
        assert_eq!(files_upload_count(&server).await, 1);

        // And a fresh LLM instance over the same cache root (cold in-process
        // map) still does not re-upload
        let cold_llm = media_llm(&server, cache_dir.path());
        let mut params = chat_params_with(vec![image_message(image_data_uri())], vec![]);
        cold_llm
            .materialize_media(&mut params)
            .await
            .expect("materialize succeeds");
        let mut stream = cold_llm.chat(params).expect("chat starts");
        drain(&mut stream).await;
        assert_eq!(
            files_upload_count(&server).await,
            1,
            "the cold cache must see the sidecar record"
        );

        // And the cold instance's chat still referenced the uploaded file
        let body = sole_chat_body(&server).await;
        assert_eq!(body["messages"][0]["content"][1]["source"]["type"], "file");
        assert_eq!(
            body["messages"][0]["content"][1]["source"]["file_id"],
            "file-x"
        );
    }

    #[tokio::test]
    async fn should_fall_back_to_inline_base64_when_the_upload_fails() {
        // Given a mock endpoint whose files upload answers 500
        let server = MockServer::start().await;
        mount_files_and_messages(&server, "file-x", 500).await;
        let cache_dir = tempfile::tempdir().expect("tempdir");
        let llm = media_llm(&server, cache_dir.path());
        let mut params = chat_params_with(vec![image_message(image_data_uri())], vec![]);

        // When the media is materialized and the chat is consumed
        llm.materialize_media(&mut params)
            .await
            .expect("materialization never fails the turn");
        let mut stream = llm.chat(params).expect("chat starts");
        let events = drain(&mut stream).await;

        // Then the turn did not fail and the image stayed inline base64
        assert!(
            !events.iter().any(|e| matches!(e, LlmEvent::Error { .. })),
            "chat should not error after an upload failure"
        );
        let body = sole_chat_body(&server).await;
        let image = &body["messages"][0]["content"][1];
        assert_eq!(image["source"]["type"], "base64");
        assert_eq!(image["source"]["media_type"], "image/png");
        assert_eq!(image["source"]["data"], "iVBORw0KGgo=");
    }

    #[tokio::test]
    async fn should_reupload_when_the_cached_entry_has_expired() {
        // Given a mock files endpoint whose uploads expire immediately
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/files"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "id": "file-x",
                "type": "file",
                "expires_at": "2020-01-01T00:00:00Z"
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(
                        "event: message_start\n\
                         data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-test\",\"usage\":{\"input_tokens\":10}}}\n\n\
                         event: message_delta\n\
                         data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n\
                         event: message_stop\n\
                         data: {\"type\":\"message_stop\"}\n\n",
                    ),
            )
            .mount(&server)
            .await;
        let cache_dir = tempfile::tempdir().expect("tempdir");
        let llm = media_llm(&server, cache_dir.path());

        // When the same image goes through two materialize turns
        for expected_uploads in 1..=2 {
            let mut params = chat_params_with(vec![image_message(image_data_uri())], vec![]);
            llm.materialize_media(&mut params)
                .await
                .expect("materialize succeeds");

            // Then the expired entry was a miss and the upload re-ran
            assert_eq!(
                files_upload_count(&server).await,
                expected_uploads,
                "turn {expected_uploads}"
            );
        }
    }

    #[tokio::test]
    async fn should_upload_once_per_base_url_when_namespaces_differ() {
        // Given two mock endpoints and one shared cache root
        let server_a = MockServer::start().await;
        let server_b = MockServer::start().await;
        mount_files_and_messages(&server_a, "file-a", 200).await;
        mount_files_and_messages(&server_b, "file-b", 200).await;
        let cache_dir = tempfile::tempdir().expect("tempdir");

        // When the same image is materialized through two adapters whose
        // base URLs differ (the same provider type, different endpoints)
        let llm_a = media_llm(&server_a, cache_dir.path());
        let llm_b = media_llm(&server_b, cache_dir.path());
        for llm in [&llm_a, &llm_b] {
            let mut params = chat_params_with(vec![image_message(image_data_uri())], vec![]);
            llm.materialize_media(&mut params)
                .await
                .expect("materialize succeeds");
        }

        // Then each endpoint saw exactly one upload — the cache namespaces
        // isolate the two base URLs
        assert_eq!(files_upload_count(&server_a).await, 1);
        assert_eq!(files_upload_count(&server_b).await, 1);
    }

    #[tokio::test]
    async fn should_upload_once_per_api_key_when_keys_differ_at_one_endpoint() {
        // Given one shared mock endpoint and one shared cache root, but two
        // adapters authenticated with different api keys (a shared gateway
        // with per-user keys)
        let server = MockServer::start().await;
        mount_files_and_messages(&server, "file-x", 200).await;
        let cache_dir = tempfile::tempdir().expect("tempdir");
        let llm_a = media_llm_with_key(&server, cache_dir.path(), "key-a");
        let llm_b = media_llm_with_key(&server, cache_dir.path(), "key-b");

        // When the same image is materialized under both keys, then once
        // more under the first key
        for llm in [&llm_a, &llm_b, &llm_a] {
            let mut params = chat_params_with(vec![image_message(image_data_uri())], vec![]);
            llm.materialize_media(&mut params)
                .await
                .expect("materialize succeeds");
        }

        // Then each key uploaded exactly once — the cache namespaces
        // isolate credentials at the same endpoint (F-T11a-1), and the
        // first key's third turn was a cache hit
        assert_eq!(
            files_upload_count(&server).await,
            2,
            "two keys must never share a cache entry"
        );
    }

    #[test]
    fn should_emit_a_file_source_when_the_image_url_is_a_fileref() {
        // Given a materializer-minted fileref URL
        let block = anthropic_image_block(&ImageUrlData {
            url: format_fileref("anthropic", "file-abc123"),
            detail: None,
            id: None,
        });

        // When the block is built
        // Then it references the file id with no media type or data
        assert_eq!(block["type"], "image");
        assert_eq!(block["source"]["type"], "file");
        assert_eq!(block["source"]["file_id"], "file-abc123");
        assert!(block["source"].get("media_type").is_none());
        assert!(block["source"].get("data").is_none());
    }

    #[test]
    fn should_emit_a_file_source_only_for_the_matching_provider() {
        // Given a fileref minted for another provider's channel
        let block = anthropic_image_block(&ImageUrlData {
            url: format_fileref("kimi", "file-abc123"),
            detail: None,
            id: None,
        });

        // When the block is built
        // Then it is not consumed as an Anthropic file reference (the URL
        // is treated as an opaque remote URL instead)
        assert_eq!(block["source"]["type"], "url");
        assert_eq!(block["source"]["url"], "fileref:kimi;file-abc123");
    }

    // --- helpers ----------------------------------------------------------

    /// Whether any content block in a converted message carries
    /// `cache_control`.
    fn message_has_cache_control(message: &Value) -> bool {
        message["content"]
            .as_array()
            .is_some_and(|blocks| blocks.iter().any(|b| b.get("cache_control").is_some()))
    }

    /// Count `cache_control` occurrences anywhere in a serialized request
    /// body — request-body evidence for the cache-observability contract.
    fn count_cache_control(value: &Value) -> usize {
        match value {
            Value::Object(map) => {
                map.values().map(count_cache_control).sum::<usize>()
                    + usize::from(map.contains_key("cache_control"))
            }
            Value::Array(items) => items.iter().map(count_cache_control).sum(),
            _ => 0,
        }
    }

    fn llm_with_server(server: &MockServer) -> AnthropicLlm {
        AnthropicLlm::new("test-api-key", format!("{}/v1", server.uri()), "claude-test", "", None)
    }

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

    // --- T2: tool-result media blocks inside `tool_result.content` --------

    #[test]
    fn should_embed_tool_result_images_inside_tool_result_content_when_converting_tool_messages() {
        // Given a tool result carrying text and an image part
        let params = chat_params_with(
            vec![Message {
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
                ],
                tool_calls: Vec::new(),
                tool_call_id: Some("call_1".to_string()),
                partial: false,
            }],
            vec![],
        );

        // When converting to Anthropic messages
        let messages = convert_messages_to_anthropic(&params).expect("conversion succeeds");

        // Then the tool_result block carries a rich content array with the
        // text and the image natively embedded
        assert_eq!(
            messages[0],
            json!({
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "call_1",
                    "content": [
                        { "type": "text", "text": "<image path=\"/tmp/cat.png\">" },
                        {
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": "image/png",
                                "data": "iVBORw0KGgo="
                            }
                        },
                    ],
                }],
            })
        );
    }

    #[test]
    fn should_note_omitted_audio_when_tool_result_carries_audio() {
        // Given a tool result carrying text and an audio part
        let params = chat_params_with(
            vec![Message {
                role: Role::Tool,
                name: None,
                content: vec![
                    ContentPart::Text {
                        text: "transcribed".to_string(),
                    },
                    ContentPart::AudioUrl {
                        audio_url: "data:audio/mpeg;base64,AAA".to_string(),
                    },
                ],
                tool_calls: Vec::new(),
                tool_call_id: Some("call_2".to_string()),
                partial: false,
            }],
            vec![],
        );

        // When converting to Anthropic messages
        let messages = convert_messages_to_anthropic(&params).expect("conversion succeeds");

        // Then the tool_result content carries an audio omission placeholder
        // (the Messages API has no audio input representation)
        assert_eq!(
            messages[0]["content"][0]["content"],
            json!([
                { "type": "text", "text": "transcribed" },
                { "type": "text", "text": "(audio omitted: not supported by this provider)" },
            ])
        );
    }

    #[test]
    fn should_classify_network_and_stream_errors_as_retryable_when_no_status_is_present() {
        let llm = AnthropicLlm::new("k", "https://example.test", "claude-x", "sys", None);
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
        let llm = AnthropicLlm::new("k", "https://example.test", "claude-x", "sys", None);
        assert!(llm.is_retryable_error(&ProviderErrorInfo::from_message(
            "Anthropic API error 503 Service Unavailable: x"
        )));
        assert!(llm.is_retryable_error(&ProviderErrorInfo::from_message(
            "429 Too Many Requests: slow down"
        )));
    }

    #[test]
    fn should_not_classify_auth_failures_as_retryable() {
        let llm = AnthropicLlm::new("k", "https://example.test", "claude-x", "sys", None);
        assert!(!llm.is_retryable_error(&ProviderErrorInfo::from_message(
            "Anthropic API error 401 Unauthorized: bad key"
        )));
        assert!(!llm.is_retryable_error(&ProviderErrorInfo::from_message(
            "403 Forbidden: invalid api key"
        )));
    }

    /// Given tool-call argument strings that are empty, malformed, or valid
    /// JSON but not an object, When they are parsed into `tool_use.input`
    /// blocks, Then each must degrade to an empty JSON object — Anthropic
    /// rejects non-object `input` on replay, and a `null` value (the old
    /// malformed fallback) would poison every later turn.
    #[test]
    fn should_degrade_non_object_tool_arguments_to_an_empty_object() {
        assert_eq!(parse_arguments(r#"{"path":"/tmp"}"#), json!({"path": "/tmp"}));
        assert_eq!(parse_arguments("[1,2]"), json!({}));
        assert_eq!(parse_arguments("null"), json!({}));
        assert_eq!(parse_arguments("3.14"), json!({}));
        assert_eq!(parse_arguments(r#"{"path":"/tm"#), json!({}));
        assert_eq!(parse_arguments(""), json!({}));
    }
}
