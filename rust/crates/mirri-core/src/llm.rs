use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use futures::Stream;
use serde_json::Value;

use crate::cancel::CancelToken;
use crate::error::EngineResult;
use crate::message::Message;
use crate::scope::AgentScopeConfig;
use crate::tool::ToolDefinition;

/// Structured error information passed to [`Llm::is_retryable_error`].
///
/// This replaces the previous `&dyn Any` parameter, which was type-unsafe and
/// required adapters to downcast. Callers construct this from the provider
/// error message, and adapters inspect the fields to decide retryability.
#[derive(Debug, Clone)]
pub struct ProviderErrorInfo {
    /// HTTP status code from the provider response, if available.
    pub status_code: Option<u16>,
    /// The error message text.
    pub message: String,
    /// Provider-specific error type string (e.g. `"overloaded_error"`,
    /// `"rate_limit_error"`), if available.
    pub error_type: Option<String>,
}

impl ProviderErrorInfo {
    /// Create a `ProviderErrorInfo` from just a message string.
    ///
    /// The kosong transport formats non-2xx responses as
    /// `"<Provider> API error <status>: <body>"`, so the HTTP status is
    /// recovered from the text when present (transport failures carry no
    /// status and stay `None`).
    pub fn from_message(message: impl Into<String>) -> Self {
        let message = message.into();
        Self {
            status_code: parse_status_code_from_message(&message),
            error_type: parse_error_type_from_message(&message),
            message,
        }
    }

    /// Whether this error is a **quota-exhaustion** 429 — the provider
    /// account is out of balance/quota and the request will keep failing
    /// until the account is topped up (P8b-1, aligned with the TS
    /// `classifyMirriQuotaError` vocabulary).
    ///
    /// A bare 429 is a *transient* rate limit and stays retryable; only
    /// billing-anchored wordings or the structured
    /// `exceeded_current_quota_error` code classify as quota exhaustion —
    /// deliberately no bare "quota"/"balance" match, so transient throttle
    /// messages ("token quota per minute") keep retrying.
    pub fn is_quota_exhausted(&self) -> bool {
        // Structured hit: the provider body's error code/type (the Kimi
        // Moonshot backend signals it as `exceeded_current_quota_error`).
        if self.error_type.as_deref() == Some("exceeded_current_quota_error") {
            return true;
        }
        // The status may not be structured (message-only errors carry the
        // 429 in the text) — accept either spelling of the status.
        let is_429 = self.status_code == Some(429) || self.message.contains("429");
        if !is_429 {
            return false;
        }
        let lower = self.message.to_lowercase();
        const QUOTA_MESSAGE_PATTERNS: [&str; 8] = [
            "exceeded your current quota",
            "exceeded your current token quota",
            "check your account balance",
            "insufficient balance",
            "recharge your account",
            "please recharge",
            "account in arrears",
            "account is in arrears",
        ];
        QUOTA_MESSAGE_PATTERNS
            .iter()
            .any(|pattern| lower.contains(pattern))
    }

    /// The provider-family domain code this error classifies to (T7,
    /// F-P8-8) — the **single mapping table** from provider error text /
    /// HTTP status to a `provider.*` domain code, mirroring the TS
    /// `translateProviderError` semantics
    /// (`agent-core-v2/src/kosong/protocol/errors.ts`) and its L0 wording
    /// tables (`kosong/contract/errors.ts`):
    ///
    /// | condition                                | domain code                     |
    /// |------------------------------------------|---------------------------------|
    /// | quota-exhaustion 429 (billing wordings)  | `provider.quota_exhausted`      |
    /// | transient 429 / rate-limit wordings      | `provider.rate_limit`           |
    /// | 401/403 / auth wordings                  | `provider.auth_error`           |
    /// | network / timeout wordings               | `provider.connection_error`     |
    /// | anything else                            | `provider.api_error`            |
    ///
    /// Order matters: the quota check runs first so a billing-anchored 429
    /// never degrades into a retryable rate limit. Callers surface the
    /// verdict as an `EngineError` variant (see `loop_engine::retry`).
    pub fn provider_domain_code(&self) -> &'static str {
        if self.is_quota_exhausted() {
            return "provider.quota_exhausted";
        }
        if self.is_rate_limited() {
            return "provider.rate_limit";
        }
        if self.is_auth_failure() {
            return "provider.auth_error";
        }
        if self.is_connection_failure() {
            return "provider.connection_error";
        }
        "provider.api_error"
    }

    /// Whether this error is **transient** — a network/transport failure,
    /// a rate-limit throttle, or a retryable server status (429/5xx).
    ///
    /// This is the shared retryability verdict the engine's request-level
    /// retry loop consults (mirroring the TS `isRetryableGenerateError`:
    /// connection/timeout errors always retry, 429 and 5xx retry, auth and
    /// quota-exhaustion never do).
    pub fn is_transient_provider_error(&self) -> bool {
        // Quota exhaustion (P8b-1) is terminal even when the message text
        // also carries a "429" that `is_rate_limited` would match — the
        // engine classifies it before the retry decision anyway; this guard
        // keeps the verdict self-contained.
        if self.is_quota_exhausted() {
            return false;
        }
        self.is_connection_failure()
            || self.is_rate_limited()
            || matches!(self.status_code, Some(429) | Some(500..=599))
    }

    /// A transient rate limit (non-quota): HTTP 429, or the TS
    /// `PROVIDER_RATE_LIMIT_MESSAGE_PATTERNS` wordings ("too many
    /// requests", "rate limit" in its separator spellings, "max rpm").
    fn is_rate_limited(&self) -> bool {
        if self.status_code == Some(429) || self.message.contains("429") {
            return true;
        }
        let lower = self.message.to_lowercase();
        [
            "too many requests",
            "rate limit",
            "rate_limit",
            "rate-limited",
            "max rpm",
        ]
        .iter()
        .any(|pattern| lower.contains(pattern))
    }

    /// An authentication failure: HTTP 401/403 or the common auth wordings.
    fn is_auth_failure(&self) -> bool {
        if matches!(self.status_code, Some(401) | Some(403)) {
            return true;
        }
        let lower = self.message.to_lowercase();
        [
            "unauthorized",
            "forbidden",
            "invalid api key",
            "invalid_api_key",
            "authentication failed",
            "authentication error",
        ]
        .iter()
        .any(|pattern| lower.contains(pattern))
    }

    /// A network/transport failure — the TS `NETWORK_RE` + `TIMEOUT_RE`
    /// wordings (connection, disconnect, terminated, timed out, timeout,
    /// deadline), plus reqwest's canonical transport-phase message
    /// `"error sending request"` (reqwest only emits it for
    /// connection/send-phase failures; non-2xx statuses surface separately)
    /// and the SSE body-read failure prefix `"error reading SSE stream"`
    /// (mirri-kosong's `SseLineReader` wraps every body-read/decode error
    /// with it — decode failures, read timeouts, and the data-line idle
    /// timeout alike; all transport-layer, all transient).
    fn is_connection_failure(&self) -> bool {
        let lower = self.message.to_lowercase();
        [
            "error sending request",
            "error reading sse stream",
            "network",
            "connect",
            "disconnect",
            "terminated",
            "timed out",
            "timeout",
            "deadline",
        ]
        .iter()
        .any(|pattern| lower.contains(pattern))
    }
}

/// Recover an HTTP status code embedded in a provider error message.
///
/// Matches the kosong non-2xx format `"<Provider> API error <status>: …"`
/// (e.g. `"OpenAI API error 429 Too Many Requests: …"`) by scanning for
/// `" error "` followed by exactly three digits. Returns the first valid
/// 3-digit code; transport failures without a status yield `None`.
fn parse_status_code_from_message(message: &str) -> Option<u16> {
    let mut rest = message;
    while let Some(pos) = rest.find(" error ") {
        let after = &rest[pos + " error ".len()..];
        let digits: String = after.chars().take(3).filter(char::is_ascii_digit).collect();
        if digits.len() == 3
            // The token must be exactly three digits, not the prefix of a
            // longer number ("error 1234" is not a status code).
            && after.chars().nth(3).is_none_or(|c| !c.is_ascii_digit())
            && let Ok(code) = digits.parse::<u16>()
        {
            return Some(code);
        }
        rest = after;
    }
    None
}

/// Recover the structured `error.type`/`error.code` a provider transport
/// appended to the error message (the `[type: X]` / `[code: Y]` suffix
/// kosong emits), so `is_quota_exhausted`'s structured branch works on
/// message-only error paths.
fn parse_error_type_from_message(message: &str) -> Option<String> {
    // Find the LAST marker occurrence — the suffix is appended at the end;
    // an earlier `[type: …]` inside the body text must not win.
    let type_pos = message.rfind("[type: ")?;
    let after = &message[type_pos + "[type: ".len()..];
    // The suffix may carry `[type: X, code: Y]` — stop at the first `]` OR
    // `,` so the type is recovered without the code fragment.
    let end = after.find([']', ','])?;
    let value = &after[..end];
    if value.is_empty() {
        return None;
    }
    Some(value.to_string())
}

/// Classify a provider HTTP error response as Moonshot-style quota
/// exhaustion, returning the `Retry-After` hint in milliseconds when the
/// response is a quota-exhausted 429 (TS `classifyKimiQuotaError` parity,
/// commit cdbd33c13).
///
/// Accepts the raw response facts — status, body text, `Retry-After` header
/// value — so files clients (which hold the reqwest response directly) can
/// classify without first flattening everything into a message string.
/// `Some(retry_after_ms)` means quota exhausted (`0` when no header was
/// present); `None` keeps the caller's base classification.
///
/// The verdict reuses [`ProviderErrorInfo::is_quota_exhausted`]'s single
/// table: a structured `error.type`/`error.code` of
/// `exceeded_current_quota_error`, or a billing-anchored wording in the
/// body. A transient throttle 429 ("token quota per minute") stays `None`.
pub fn classify_quota_response(
    status: u16,
    body: &str,
    retry_after_header: Option<&str>,
) -> Option<u64> {
    if status != 429 {
        return None;
    }
    // Structured hit: the body's error.type / error.code (either spelling —
    // gateways hoist one or the other onto the body's error object).
    let structured_hit = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            let error = value.get("error")?;
            error
                .get("type")
                .and_then(|t| t.as_str())
                .or_else(|| error.get("code").and_then(|c| c.as_str()))
                .map(str::to_owned)
        })
        .is_some_and(|code| code == "exceeded_current_quota_error");
    let wording_hit = ProviderErrorInfo {
        status_code: Some(429),
        message: body.to_string(),
        error_type: None,
    }
    .is_quota_exhausted();
    if !structured_hit && !wording_hit {
        return None;
    }
    // Retry-After: integer seconds → milliseconds (the TS parseRetryAfterMs
    // semantics; an unparseable or absent header contributes 0).
    let retry_after_ms = retry_after_header
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .map(|seconds| seconds * 1000)
        .unwrap_or(0);
    Some(retry_after_ms)
}

/// Events emitted by the LLM during a chat call.
///
/// This replaces the 5 streaming callbacks from the TypeScript LLM interface
/// with a single stream. The caller handles events as they arrive.
///
/// There are two granularities:
/// - `*Delta` / `ToolCallPart` — incremental, fire-and-forget, chunk-level.
/// - `*Part` / `ToolCall` / `Usage` / `Done` / `Error` — block-level, ordered.
#[derive(Debug, Clone)]
pub enum LlmEvent {
    /// Incremental text delta (fire-and-forget, chunk-level).
    TextDelta { text: String },
    /// Incremental thinking/reasoning delta (fire-and-forget, chunk-level).
    ThinkDelta { text: String },
    /// Incremental tool-call argument fragment (index-addressed). `call_id`/
    /// `tool_name` carry the fragment's identity when the provider already
    /// knows it (see `mirri-kosong`'s `StreamEvent::ToolCallPart`); `None` on
    /// fragments that precede their identity header.
    ToolCallPart {
        index: u64,
        arguments_part: String,
        call_id: Option<String>,
        tool_name: Option<String>,
    },
    /// A complete tool call (emitted when the content block is fully received).
    ToolCall {
        tool: String,
        args: Value,
        call_id: String,
    },
    /// A complete text part (block-level, ordered — preserves transcript order).
    TextPart { text: String },
    /// A complete think part (block-level, ordered).
    ThinkPart { text: String },
    /// Token usage accounting.
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        cache_read_tokens: u64,
        cache_creation_tokens: u64,
    },
    /// Stream completed; carries the provider's stop reason.
    Done { stop_reason: String },
    /// Stream-level error.
    Error { message: String },
}

/// A boxed stream of LLM events.
pub type LlmEventStream = Pin<Box<dyn Stream<Item = LlmEvent> + Send>>;

/// Model capability flags.
#[derive(Debug, Clone, Default)]
pub struct ModelCapability {
    pub image_in: bool,
    pub video_in: bool,
    pub audio_in: bool,
    pub dynamically_loaded_tools: bool,
}

/// Parameters for an LLM chat call.
#[derive(Debug, Clone)]
pub struct LlmChatParams {
    /// The messages to send (already projected, wire-valid).
    pub messages: Vec<Message>,
    /// The tools available to the model.
    pub tools: Vec<ToolDefinition>,
    /// Cancellation token.
    pub cancel: CancelToken,
    /// Optional request log fields for telemetry.
    pub request_log_fields: Option<Value>,
    /// The fork chain's cache root id (the provider prompt-cache routing
    /// key, TS parity: the TS kernel passes the session id as
    /// `promptCacheKey`). Minted by the fork pipeline and persisted in
    /// `state.json` as `cacheRootId`; `None` on fresh sessions and legacy
    /// states — providers then omit the cache-routing field entirely.
    pub cache_root_id: Option<String>,
}

/// The response from an LLM chat call (after stream completes).
#[derive(Debug, Clone)]
pub struct LlmChatResponse {
    /// The stop reason normalized to our taxonomy.
    pub stop_reason: StopReason,
    /// Text content from the response.
    pub text: String,
    /// Tool calls from the response.
    pub tool_calls: Vec<LlmToolCall>,
    /// Token usage.
    pub usage: LlmUsage,
}

/// Normalized stop reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    /// Model finished naturally (end of turn).
    EndTurn,
    /// Model hit max output tokens.
    MaxTokens,
    /// Model wants to call tools (loop continues).
    ToolUse,
    /// Content was filtered.
    Filtered,
    /// Response was paused (some providers).
    Paused,
    /// Unknown stop reason.
    Unknown,
}

impl StopReason {
    /// Whether this stop reason means the turn should continue (tool_use).
    pub fn is_loop_continuing(&self) -> bool {
        matches!(self, Self::ToolUse)
    }
}

/// A complete tool call surfaced in an [`LlmChatResponse`].
#[derive(Debug, Clone)]
pub struct LlmToolCall {
    pub id: String,
    pub name: String,
    pub args: Value,
}

/// Token usage accounting.
///
/// Four buckets: non-cached prompt (`input_tokens`), completion
/// (`output_tokens`), prompt-cache hits (`cache_read_tokens`), and
/// prompt-cache writes (`cache_creation_tokens`). Mirrors
/// [`crate::usage::TokenUsage`]; the cache buckets were added in the
/// three-bucket carry-through so prompt-cache observability survives the
/// hop from the provider adapters to the wire.
///
/// **Dispatcher normalisation contract (invariant, F7):** `input_tokens`
/// is *always* the exclusive value — the non-cached prompt tokens, with
/// cache hits reported separately in `cache_read_tokens`. Every
/// mirri-kosong dispatcher enforces this before usage crosses the LLM
/// boundary: Anthropic reports it natively; OpenAI/DeepSeek, OpenAI
/// Responses, and Google subtract their inclusive `cached_tokens`/
/// `cachedContentTokenCount` from the prompt total (Kimi inherits the
/// OpenAI base). Downstream consumers may therefore sum the buckets
/// (see [`crate::context_memory::LoopEvent::context_tokens`]) without
/// double-counting cached tokens, regardless of provider.
#[derive(Debug, Clone, Default)]
pub struct LlmUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
}

/// The LLM trait — abstraction over the provider's chat API.
///
/// This is the Rust equivalent of the TypeScript `LLM` interface. Concrete
/// adapters live in `mirri-embedded/src/providers` (e.g. `AnthropicLlm`,
/// `OpenAILlm`), bridging this onto the `mirri-kosong`
/// `ChatProvider`/streaming API.
pub trait Llm: Send + Sync {
    /// The originating provider id this adapter was built from, when
    /// known — the attribution a surfaced auth failure carries so
    /// credential-status reporting can name the provider (O-5, ADR-009).
    /// The default `None` marks an unattributed adapter (test mocks,
    /// direct constructions): an unattributed failure is never reported.
    fn provider_id(&self) -> Option<&str> {
        None
    }

    /// The system prompt (held separately from the message history).
    fn system_prompt(&self) -> &str;

    /// The model name.
    fn model_name(&self) -> &str;

    /// Model capability flags.
    fn capability(&self) -> &ModelCapability;

    /// Whether an error is retryable (for exponential backoff).
    ///
    /// The default implementation never retries. Adapters override this to
    /// inspect provider-specific error shapes via [`ProviderErrorInfo`].
    fn is_retryable_error(&self, _error: &ProviderErrorInfo) -> bool {
        false
    }

    /// Send a chat request and return a stream of events.
    ///
    /// The caller consumes the stream, collecting deltas and waiting for the
    /// `Done` event to get the final stop reason.
    fn chat(&self, params: LlmChatParams) -> EngineResult<LlmEventStream>;

    /// Materialize inline media onto provider-side file references (T11a).
    ///
    /// The default is a no-op (providers without a files channel — and every
    /// test mock — inherit it). An adapter with a provider files API
    /// overrides this to rewrite the transient `data:` URIs in `params` into
    /// provider-scoped references (e.g. `fileref:anthropic;<file_id>`) before
    /// the chat call, uploading unseen content through its files channel.
    ///
    /// Contract (the materialization invariants, brief T11a §4.1):
    /// - the rewrite touches **only the transient `params`** — context
    ///   memory and wire records keep the provider-neutral `data:` URIs, so
    ///   provider/model switches, resume, and compaction are unaffected;
    /// - implementations must degrade, never fail: an upload error keeps the
    ///   inline payload and is only warned (materialization cannot fail a
    ///   turn).
    ///
    /// AFIT (`async fn` in traits) is not object-safe, so this returns a
    /// boxed future — `Llm` is consumed as `dyn Llm` throughout the engine.
    /// The trait itself stays IO-free (contract 6): overrides live in
    /// `mirri-embedded`'s provider adapters.
    fn materialize_media<'a>(
        &'a self,
        params: &'a mut LlmChatParams,
    ) -> Pin<Box<dyn Future<Output = EngineResult<()>> + Send + 'a>> {
        Box::pin(async {
            let _ = params;
            Ok(())
        })
    }
}

/// A factory that builds an [`Llm`] provider for an agent's engine loop.
///
/// Implementations own provider construction — API keys, base URLs, model
/// routing — and may fail with a human-readable reason (e.g. a missing API
/// key). Engines hold an optional factory: `mirri-server`'s `ServerState`
/// and `mirri-embedded`'s `EmbeddedServer` both consume this one
/// authoritative trait (the two crates previously defined signature-identical
/// copies — M5 unified them here, where the `AgentScopeConfig`/`Llm` types it
/// is built from already live). `None` (the default) makes `message.send`
/// fail with `no LLM provider configured for this agent` instead of running
/// a turn.
pub trait LlmFactory: Send + Sync {
    /// Build the provider that serves the agent's configured
    /// `model_id`/`provider_type`, or fail with a human-readable reason.
    fn create_llm(&self, config: &AgentScopeConfig) -> Result<Arc<dyn Llm>, String>;

    /// A fingerprint of the external credential sources the factory bakes
    /// into every adapter it builds (e.g. the credential store file's
    /// mtime + length). Resolver-side caches treat a change as a signal to
    /// rebuild — a rotated key must not keep serving from a stale cached
    /// adapter. The default `0` (nothing fingerprintable) keeps factories
    /// without external credential sources on their existing caching.
    fn credential_fingerprint(&self) -> u64 {
        0
    }

    /// The upcast seam for concrete-factory capabilities the trait does not
    /// model (e.g. the embedded server reading the config-aware factory's
    /// credential store for its per-request paths). The default returns a
    /// unit value: a factory that never opted in is simply never
    /// downcastable — `downcast_ref` answers `None` instead of panicking,
    /// so a future call site cannot crash on a mock or test-double factory.
    fn as_any(&self) -> &dyn std::any::Any {
        &()
    }
}

/// Resolves the [`Llm`] instance a loop step should talk to.
///
/// The engine re-resolves this at every loop step so a mid-turn model
/// switch (e.g. `agent.setModel` while a turn is streaming) takes effect
/// on the next LLM call instead of the next turn. Implementations cache
/// the current instance and rebuild only when the configured model
/// changes. Failing with a human-readable reason aborts the step.
pub trait LlmResolver: Send + Sync {
    fn resolve(&self) -> Result<Arc<dyn Llm>, String>;

    /// Resolve the step's LLM together with the model ALIAS (M1) it was
    /// resolved from — the per-request attribution key for usage recording
    /// (spec 11 A13: the wire `usage.record` model field and the
    /// in-memory `by_model` key are both the alias, TS parity — the TS
    /// kernel records under `config.model`).
    ///
    /// The default reports no alias: resolvers with no alias notion
    /// (fixed-instance resolvers, test doubles) keep the
    /// `llm.model_name()` (raw upstream name, M2) fallback at the
    /// recording sites, so the trait stays backward compatible.
    fn resolve_with_alias(&self) -> Result<(Arc<dyn Llm>, Option<String>), String> {
        self.resolve().map(|llm| (llm, None))
    }
}

/// A resolver that always returns the same instance — the pre-switch
/// semantics, for call sites with no dynamic model (tests, subagent
/// hosts that build their own LLM per turn).
pub struct FixedLlmResolver {
    llm: Arc<dyn Llm>,
    /// The model alias (M1) the pinned instance was resolved from, when
    /// the builder knows it (spec 11 A13): reported through
    /// [`Self::resolve_with_alias`] so the pinning site's usage
    /// `usage.record` rows carry the alias instead of the raw upstream
    /// name. `None` (the plain [`Self::new`]) keeps the
    /// `llm.model_name()` fallback.
    alias: Option<String>,
}

impl FixedLlmResolver {
    pub fn new(llm: Arc<dyn Llm>) -> Self {
        Self { llm, alias: None }
    }

    /// Pin `llm` AND report `alias` as its model alias — the internal-turn
    /// shape (ADR-007: the model is pinned at initiation, but the usage
    /// attribution still carries the alias the instance was built from).
    pub fn with_alias(llm: Arc<dyn Llm>, alias: impl Into<String>) -> Self {
        Self {
            llm,
            alias: Some(alias.into()),
        }
    }
}

impl LlmResolver for FixedLlmResolver {
    fn resolve(&self) -> Result<Arc<dyn Llm>, String> {
        Ok(self.llm.clone())
    }

    fn resolve_with_alias(&self) -> Result<(Arc<dyn Llm>, Option<String>), String> {
        Ok((self.llm.clone(), self.alias.clone()))
    }
}

#[cfg(test)]
mod resolver_tests {
    use super::*;
    use std::sync::Arc;

    struct StubLlm {
        capability: ModelCapability,
    }
    impl Llm for StubLlm {
        fn system_prompt(&self) -> &str {
            ""
        }
        fn model_name(&self) -> &str {
            "stub"
        }
        fn capability(&self) -> &ModelCapability {
            &self.capability
        }
        fn chat(&self, _params: LlmChatParams) -> EngineResult<LlmEventStream> {
            unimplemented!()
        }
    }

    #[test]
    fn should_return_the_fixed_instance_when_resolved_repeatedly() {
        // Given a fixed resolver over one instance
        let llm: Arc<dyn Llm> = Arc::new(StubLlm {
            capability: ModelCapability::default(),
        });
        let resolver = FixedLlmResolver::new(llm);
        // When resolved twice, both yield the same instance
        let a = resolver.resolve().unwrap();
        let b = resolver.resolve().unwrap();
        assert!(Arc::ptr_eq(&a, &b));
    }
}

#[cfg(test)]
mod tests {
    use super::{ProviderErrorInfo, classify_quota_response};

    #[test]
    fn should_extract_status_code_when_message_carries_the_api_error_prefix() {
        // Given a kosong-style non-2xx error message
        let info = ProviderErrorInfo::from_message(
            "OpenAI API error 503 Service Unavailable: upstream overloaded",
        );
        // Then the status is parsed out of the text
        assert_eq!(info.status_code, Some(503));
    }

    #[test]
    fn should_return_none_when_message_has_no_embedded_status_code() {
        let info = ProviderErrorInfo::from_message(
            "error sending request: error trying to connect: tcp connect error",
        );
        assert_eq!(info.status_code, None);
    }

    #[test]
    fn should_return_none_when_digits_after_error_are_not_three_long() {
        // "error 12" must not parse as a status code (and must not
        // short-circuit a later valid match in the same message)
        let info = ProviderErrorInfo::from_message("failed with error 12 things");
        assert_eq!(info.status_code, None);
    }

    #[test]
    fn should_classify_connection_wordings_as_transient() {
        for message in [
            "error sending request: connection reset by peer",
            "error reading SSE stream: error reading a body from connection",
            "error sending request for url (http://127.0.0.1:1/v1/chat/completions)",
            "request timed out after 30s",
            "operation timed out",
            "fetch failed: network error",
        ] {
            let info = ProviderErrorInfo::from_message(message);
            assert!(
                info.is_transient_provider_error(),
                "'{message}' must classify as transient"
            );
        }
    }

    #[test]
    fn should_classify_sse_decode_errors_as_transient() {
        // The 2026-10-19 incident: a mid-stream SSE body decode failure
        // ("error reading SSE stream: error decoding response body") was
        // classified as non-retryable and killed the subagent on attempt 1.
        // Any error surfaced through the SSE read path is a transport-layer
        // failure — transient by nature.
        for message in [
            "error reading SSE stream: error decoding response body",
            "error reading SSE stream: operation timed out",
            "error reading SSE stream: SSE idle timeout (no data line for 300s)",
        ] {
            let info = ProviderErrorInfo::from_message(message);
            assert!(
                info.is_transient_provider_error(),
                "'{message}' must classify as transient"
            );
        }
    }

    #[test]
    fn should_classify_rate_limit_and_5xx_statuses_as_transient() {
        let throttled = ProviderErrorInfo::from_message("429 Too Many Requests: slow down");
        let server_error =
            ProviderErrorInfo::from_message("Anthropic API error 500 Internal Server Error: x");
        let overloaded = ProviderErrorInfo::from_message("Google GenAI API error 529: overloaded");
        assert!(throttled.is_transient_provider_error());
        assert!(server_error.is_transient_provider_error());
        assert!(overloaded.is_transient_provider_error());
    }

    #[test]
    fn should_not_classify_auth_or_quota_failures_as_transient() {
        let auth = ProviderErrorInfo::from_message("403 Forbidden: invalid api key");
        let quota = ProviderErrorInfo::from_message(
            "429 Too Many Requests: You exceeded your current quota, please recharge",
        );
        assert!(!auth.is_transient_provider_error());
        assert!(!quota.is_transient_provider_error());
    }

    // -- Kimi 配额 429 快速失败（TS cdbd33c13 classifyKimiQuotaError 对接）--

    #[test]
    fn should_classify_a_structured_quota_429_body_as_quota_exhausted() {
        // Given a 429 body whose structured error type is the Moonshot
        // quota-exhaustion code
        let body = r#"{"error":{"type":"exceeded_current_quota_error","message":"You exceeded your current token quota, please check your account balance"}}"#;

        // When the response is classified
        let retry_after = classify_quota_response(429, body, None);

        // Then it is quota exhaustion (no Retry-After header → 0)
        assert_eq!(retry_after, Some(0));
    }

    #[test]
    fn should_classify_a_wording_only_quota_429_body_as_quota_exhausted() {
        // Given a 429 body a gateway flattened to text (no JSON structure)
        let body = "You exceeded your current quota, please recharge your account";

        // When the response is classified
        let retry_after = classify_quota_response(429, body, None);

        // Then the billing wording alone classifies it
        assert_eq!(retry_after, Some(0));
    }

    #[test]
    fn should_carry_the_retry_after_header_in_milliseconds() {
        // Given a quota 429 with a Retry-After header of 5 seconds
        let body = r#"{"error":{"message":"insufficient balance"}}"#;

        // When the response is classified
        let retry_after = classify_quota_response(429, body, Some("5"));

        // Then the hint travels as milliseconds
        assert_eq!(retry_after, Some(5_000));
    }

    #[test]
    fn should_not_classify_a_transient_throttle_429_as_quota_exhausted() {
        // Given a 429 whose wording is a per-minute throttle, not billing
        let body = "429 Too Many Requests: token quota per minute exceeded, slow down";

        // When the response is classified
        let verdict = classify_quota_response(429, body, None);

        // Then it stays a transient rate limit (None = not quota exhausted)
        assert_eq!(verdict, None);
    }

    #[test]
    fn should_not_classify_a_non_429_status_even_with_quota_wording() {
        // Given a 400 body carrying the balance wording (a malformed-request
        // rejection that happens to mention balance)
        let body = "bad request: please check your account balance field";

        // When the response is classified
        let verdict = classify_quota_response(400, body, None);

        // Then it is not quota exhaustion
        assert_eq!(verdict, None);
    }

    #[test]
    fn should_recover_the_error_type_from_the_structured_suffix() {
        // Given a kosong-formatted message carrying the structured marker
        let message = "OpenAI API error 429: {\"error\":{\"message\":\"quota\"}} \
                       [type: exceeded_current_quota_error]";

        // When the info is parsed from the message
        let info = ProviderErrorInfo::from_message(message);

        // Then the structured type is recovered and drives classification
        assert_eq!(
            info.error_type.as_deref(),
            Some("exceeded_current_quota_error")
        );
        assert!(info.is_quota_exhausted());
    }

    #[test]
    fn should_parse_only_the_type_when_the_suffix_carries_type_and_code_together() {
        // Given a kosong-formatted message with the combined suffix
        let message = "OpenAI API error 429: {\"error\":{\"code\":\"c1\"}} \
                       [type: exceeded_current_quota_error, code: c1]";

        // When the info is parsed from the message
        let info = ProviderErrorInfo::from_message(message);

        // Then the type is recovered cleanly, without the code fragment
        assert_eq!(
            info.error_type.as_deref(),
            Some("exceeded_current_quota_error")
        );
        assert!(info.is_quota_exhausted());
    }
}
