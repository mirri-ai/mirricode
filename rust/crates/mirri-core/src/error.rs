use thiserror::Error;

/// Error codes matching the TypeScript MirriError system.
///
/// Wire format uses `domain.reason` dot-notation (e.g. `"loop.max_steps_exceeded"`)
/// to align with the TS `ErrorCodes` const. Adding a code is a minor change;
/// renaming or removing one is a major change.
///
/// Codes without a TS counterpart use Rust-specific namespaces:
/// `permission.*`, `tool.*`, `hook.*`, `abort.*`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorCode {
    // ── Engine-layer codes (TS-aligned) ───────────────────────────────
    LoopMaxStepsExceeded,
    ContextOverflow,
    CompactionFailed,
    CompactionUnable,
    RequestInvalid,
    ProviderApiError,
    /// A transient provider rate limit (T7, F-P8-8): the provider throttled
    /// the request (HTTP 429 or rate-limit wording) but it is not a
    /// billing-anchored quota exhaustion — a later retry can succeed. Wire
    /// reason `provider.rate_limit`, matching the TS vocabulary's
    /// retryable code.
    ProviderRateLimit,
    /// Provider authentication failed (HTTP 401/403 or auth wording) —
    /// re-authentication is required; retrying the same request cannot
    /// succeed. Wire reason `provider.auth_error`.
    ProviderAuthError,
    /// A network/transport failure reaching the provider (connection
    /// refused/reset, timeout, DNS) — transient, retryable. Wire reason
    /// `provider.connection_error`.
    ProviderConnectionError,
    /// The requested operation is known but not implemented by this engine
    /// (wire reason `not_implemented`, matching the TS `NOT_IMPLEMENTED`
    /// code in agent-core's `ErrorCodes`). Not retryable.
    /// The provider account's quota/balance is exhausted. Terminal — a
    /// retry cannot succeed until the account is topped up.
    ProviderQuotaExhausted,
    NotImplemented,
    Internal,

    // ── Engine-layer codes (Rust-specific namespaces) ────────────────
    PermissionDenied,
    ToolExecutionFailed,
    HookBlocked,
    AbortCancelled,
    AbortUserCancelled,

    // ── Session-domain codes ──────────────────────────────────────────
    SessionNotFound,
    SessionAlreadyExists,
    SessionClosed,
    SessionInitFailed,
    /// The write-side commit fence rejected the send (session-consistency
    /// batch 2, ADR-002): the session's main journal diverged from this
    /// process's known-size anchor before the user message would be
    /// recorded, so the message never reached the disk.
    SessionConflict,

    // ── Agent-domain codes ────────────────────────────────────────────
    AgentNotFound,
}

impl ErrorCode {
    /// Every variant of the enum, in declaration order — the single list
    /// the vocabulary tests iterate (T7: a new variant added without
    /// registering it in the `mirri-proto` adjudication lists turns the
    /// vocabulary test red).
    pub const ALL: &[ErrorCode] = &[
        ErrorCode::LoopMaxStepsExceeded,
        ErrorCode::ContextOverflow,
        ErrorCode::CompactionFailed,
        ErrorCode::CompactionUnable,
        ErrorCode::RequestInvalid,
        ErrorCode::ProviderApiError,
        ErrorCode::ProviderRateLimit,
        ErrorCode::ProviderAuthError,
        ErrorCode::ProviderConnectionError,
        ErrorCode::ProviderQuotaExhausted,
        ErrorCode::NotImplemented,
        ErrorCode::Internal,
        ErrorCode::PermissionDenied,
        ErrorCode::ToolExecutionFailed,
        ErrorCode::HookBlocked,
        ErrorCode::AbortCancelled,
        ErrorCode::AbortUserCancelled,
        ErrorCode::SessionNotFound,
        ErrorCode::SessionAlreadyExists,
        ErrorCode::SessionClosed,
        ErrorCode::SessionInitFailed,
        ErrorCode::SessionConflict,
        ErrorCode::AgentNotFound,
    ];

    /// Stable wire representation of the code (`domain.reason` dot-notation),
    /// used when the code is serialized into RPC error responses for client
    /// correlation.
    pub fn as_str(&self) -> &'static str {
        match self {
            // TS-aligned
            Self::LoopMaxStepsExceeded => "loop.max_steps_exceeded",
            Self::ContextOverflow => "context.overflow",
            Self::CompactionFailed => "compaction.failed",
            Self::CompactionUnable => "compaction.unable",
            Self::RequestInvalid => "request.invalid",
            Self::ProviderApiError => "provider.api_error",
            Self::ProviderRateLimit => "provider.rate_limit",
            Self::ProviderAuthError => "provider.auth_error",
            Self::ProviderConnectionError => "provider.connection_error",
            Self::ProviderQuotaExhausted => "provider.quota_exhausted",
            Self::NotImplemented => "not_implemented",
            Self::Internal => "internal",
            // Rust-specific namespaces
            Self::PermissionDenied => "permission.denied",
            Self::ToolExecutionFailed => "tool.execution_failed",
            Self::HookBlocked => "hook.blocked",
            Self::AbortCancelled => "abort.cancelled",
            Self::AbortUserCancelled => "abort.user_cancelled",
            // Session domain
            Self::SessionNotFound => "session.not_found",
            Self::SessionAlreadyExists => "session.already_exists",
            Self::SessionClosed => "session.closed",
            Self::SessionInitFailed => "session.init_failed",
            Self::SessionConflict => "session.conflict",
            // Agent domain
            Self::AgentNotFound => "agent.not_found",
        }
    }

    /// Whether this error is retryable — delegates to the shared
    /// `mirri-proto` domain-code table (`domain_code_retryable`), the single
    /// fact source the wire surfaces (`turn.ended` error object, JSON-RPC
    /// error `data`) also consult. This is a static default; the runtime
    /// retry logic in `loop_engine::retry` may override this for provider
    /// errors where dynamic information (HTTP status code) is available —
    /// but by the time an error surfaces to the wire the engine's retries
    /// are exhausted, and the static verdict is the honest answer.
    pub fn retryable(&self) -> bool {
        mirri_proto::rpc::domain_code_retryable(self.as_str())
    }
}

/// The main error type for the engine layer.
#[derive(Debug, Error)]
pub enum EngineError {
    #[error("Max steps exceeded: {0}")]
    MaxStepsExceeded(String),

    #[error("Context overflow after {attempts} compaction attempts")]
    ContextOverflow { attempts: u32 },

    #[error("Compaction failed: {0}")]
    CompactionFailed(String),

    #[error("Compaction unable: {0}")]
    CompactionUnable(String),

    #[error("Request invalid: {0}")]
    RequestInvalid(String),

    #[error("Permission denied: {0}")]
    PermissionDenied(String),

    #[error("Tool execution failed: {0}")]
    ToolExecutionFailed(String),

    #[error("Provider error: {0}")]
    ProviderError(String),

    /// A transient provider rate limit (T7, F-P8-8): a 429 that is *not*
    /// billing-anchored quota exhaustion. Surfaces when the engine's
    /// retries are exhausted or the adapter deems it non-retryable; the
    /// domain code `provider.rate_limit` marks it client-retryable.
    #[error("Provider rate limit: {0}")]
    ProviderRateLimit(String),

    /// Provider authentication failed (401/403 or auth wording). Retrying
    /// the same request cannot succeed — re-authentication is required.
    /// The attribution (O-5, ADR-009) rides as a structured field; Display
    /// renders the message verbatim so the wire face is unchanged, and
    /// `None` marks an unattributed failure (never reported).
    #[error("Provider authentication error: {message}")]
    ProviderAuthError {
        message: String,
        #[doc = "The provider the failing adapter was built from, when known."]
        provider_id: Option<String>,
    },

    /// A network/transport failure reaching the provider (connection
    /// refused/reset, timeout, DNS). Transient by nature.
    #[error("Provider connection error: {0}")]
    ProviderConnectionError(String),

    /// The model's response was completely empty — no text, no thinking, no
    /// tool calls — while stopping naturally (turn-scheduling US1.4 /
    /// SDR-002 / ADR-002). Anomalous but recoverable: the step-level retry
    /// service retries it structurally (independent of message patterns),
    /// and a drained budget fails the turn explicitly instead of silently
    /// completing it.
    #[error(
        "Model returned an empty response (no text, no thinking, no tool calls) with a natural end-turn stop"
    )]
    EmptyLlmResponse,

    /// The provider account's quota/balance is exhausted (P8b-1). Terminal —
    /// never retried by the engine (a retry cannot succeed until the account
    /// is topped up). Carries the provider's `Retry-After` hint when present,
    /// so clients can surface "retry after N ms" without re-parsing the raw
    /// message.
    #[error("Provider quota exhausted: {message}")]
    ProviderQuotaExhausted {
        message: String,
        #[doc = "The provider's Retry-After hint in milliseconds, when present."]
        retry_after_ms: Option<u64>,
    },

    /// The operation is known but not implemented by this engine. Wire
    /// reason `not_implemented`; never retryable.
    #[error("Not implemented: {0}")]
    NotImplemented(String),

    /// A foreign writer appended to (or rebuilt) the session's main journal
    /// between this process's last absorb and its own submit — the
    /// write-side commit fence's rejection (session-consistency batch 2,
    /// ADR-002). Raised BEFORE the user message is recorded, so nothing of
    /// the rejected send reached the disk; the client recovers by
    /// re-converging (the follow driver's next absorb, or a session resume
    /// which re-establishes the anchor wholesale) and resubmitting.
    #[error(
        "session '{session_id}' has concurrent journal updates from another instance; retry after the next follow tick or resume the session"
    )]
    SessionConflict {
        #[doc = "The session whose main journal diverged."]
        session_id: String,
    },

    #[error("Operation cancelled by user")]
    UserCancelled,

    #[error("Operation aborted: {0}")]
    Aborted(String),

    #[error("Internal error: {0}")]
    Internal(String),

    #[error("Step blocked by before_step hook: {0}")]
    HookBlocked(String),

    /// An I/O failure propagated with its source chain preserved
    /// (`#[from]`), so `source()` exposes the underlying [`std::io::Error`].
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// A JSON (de)serialization failure propagated with its source chain
    /// preserved (`#[from]`), so `source()` exposes the underlying
    /// [`serde_json::Error`].
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}

impl EngineError {
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::MaxStepsExceeded(_) => ErrorCode::LoopMaxStepsExceeded,
            Self::ContextOverflow { .. } => ErrorCode::ContextOverflow,
            Self::CompactionFailed(_) => ErrorCode::CompactionFailed,
            Self::CompactionUnable(_) => ErrorCode::CompactionUnable,
            Self::RequestInvalid(_) => ErrorCode::RequestInvalid,
            Self::PermissionDenied(_) => ErrorCode::PermissionDenied,
            Self::ToolExecutionFailed(_) => ErrorCode::ToolExecutionFailed,
            Self::ProviderError(_) => ErrorCode::ProviderApiError,
            Self::ProviderRateLimit(_) => ErrorCode::ProviderRateLimit,
            Self::ProviderAuthError { .. } => ErrorCode::ProviderAuthError,
            Self::ProviderConnectionError(_) => ErrorCode::ProviderConnectionError,
            Self::EmptyLlmResponse => ErrorCode::ProviderApiError,
            Self::ProviderQuotaExhausted { .. } => ErrorCode::ProviderQuotaExhausted,
            Self::NotImplemented(_) => ErrorCode::NotImplemented,
            Self::SessionConflict { .. } => ErrorCode::SessionConflict,
            Self::UserCancelled => ErrorCode::AbortUserCancelled,
            Self::Aborted(_) => ErrorCode::AbortCancelled,
            // Internal failures must never masquerade as cancellations: they
            // get their own code for observability and correct client routing.
            Self::Internal(_) => ErrorCode::Internal,
            Self::HookBlocked(_) => ErrorCode::HookBlocked,
            // The `#[from]` conversions preserve the source error; keep the
            // codes that the old manual `From` impls produced so wire
            // semantics are unchanged.
            Self::Io(_) => ErrorCode::ToolExecutionFailed,
            Self::Json(_) => ErrorCode::RequestInvalid,
        }
    }

    /// Whether this error represents an abort (user or system cancellation).
    pub fn is_abort(&self) -> bool {
        matches!(self, Self::UserCancelled | Self::Aborted(_))
    }

    /// Whether this error represents an explicit user cancellation.
    pub fn is_user_cancellation(&self) -> bool {
        matches!(self, Self::UserCancelled)
    }
}

pub type EngineResult<T> = Result<T, EngineError>;

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    /// Every `ErrorCode` variant's wire string is registered in the
    /// mirri-proto adjudication vocabulary (T7, F-P8-8): produced-by-rust
    /// or rust-only. A variant added outside the declared lists turns this
    /// red — the engine's domain codes cannot escape the single-source
    /// table in `mirri-proto`'s error module.
    #[test]
    fn should_register_every_error_code_variant_in_the_domain_code_vocabulary() {
        // Given the declared vocabulary (TS-produced ∪ rust-only)
        let declared: BTreeSet<&str> = mirri_proto::rpc::RUST_PRODUCED_TS_CODES
            .iter()
            .chain(mirri_proto::rpc::RUST_ONLY_DOMAIN_CODES.iter())
            .copied()
            .collect();
        assert!(!declared.is_empty());

        // Then every engine code's wire string is declared
        for code in ErrorCode::ALL {
            assert!(
                declared.contains(code.as_str()),
                "engine code {} is missing from the mirri-proto domain-code vocabulary",
                code.as_str()
            );
        }

        // And ALL carries unique wire strings (one entry per variant)
        let wire_strings: Vec<&str> = ErrorCode::ALL.iter().map(|c| c.as_str()).collect();
        let distinct: BTreeSet<&str> = wire_strings.iter().copied().collect();
        assert_eq!(
            distinct.len(),
            wire_strings.len(),
            "ErrorCode::ALL must not repeat a wire string"
        );
    }

    /// The provider-family codes carry the TS retryable semantics: rate
    /// limit and connection failures are retryable, auth and the generic
    /// API error are not (the verdict itself is owned and per-code synced
    /// by mirri-proto's fixture test — this pins the enum delegation).
    #[test]
    fn should_delegate_provider_retryability_to_the_domain_code_table() {
        assert!(ErrorCode::ProviderRateLimit.retryable());
        assert!(ErrorCode::ProviderConnectionError.retryable());
        assert!(!ErrorCode::ProviderAuthError.retryable());
        assert!(!ErrorCode::ProviderApiError.retryable());
        assert!(!ErrorCode::ProviderQuotaExhausted.retryable());
    }

    /// The write-side commit fence's rejection (session-consistency batch 2,
    /// ADR-002): the `SessionConflict` engine error maps to the
    /// `session.conflict` domain code and is NOT retryable — the client
    /// must re-converge (a session resume re-establishes the anchor) before
    /// resubmitting; a blind retry would race the same divergence again.
    #[test]
    fn should_map_session_conflict_to_a_non_retryable_domain_code() {
        let error = EngineError::SessionConflict {
            session_id: "session-fence-test".to_string(),
        };
        assert_eq!(error.code(), ErrorCode::SessionConflict);
        assert_eq!(error.code().as_str(), "session.conflict");
        assert!(!error.code().retryable());
    }
}
