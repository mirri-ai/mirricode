//! Error serialization tests (G-18/G-20 restore).
//!
//! `EngineError` and `SessionError` do not derive `Serialize`/`Deserialize`
//! directly — their wire representation is produced by the RPC layer
//! (`mirri_server::rpc::ApiErrorResponse`, built from the JSON-RPC code, the
//! `Display` message, and the stable `ErrorCode::as_str()` domain code).
//!
//! `mirri-core` integration tests cannot depend on `mirri-server` (the server
//! depends on the core), so these tests mirror the envelope construction
//! exactly as `rpc.rs` does for `RpcError::Engine` / `RpcError::Session` and
//! verify that every error variant survives a JSON round-trip with its code,
//! message, and domain code intact.

use std::error::Error;
use std::panic::{AssertUnwindSafe, catch_unwind};

use mirri_core::{EngineError, ErrorCode, SessionError};
use serde_json::{Value, json};

/// Local mirror of `mirri_server::rpc::ApiErrorResponse` — the wire envelope
/// clients receive. Built the same way `ApiErrorResponse::error` builds it for
/// engine/session errors.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
struct WireErrorEnvelope {
    code: i32,
    msg: String,
    /// Domain error code (e.g. `"provider.api_error"`, `"session.not_found"`);
    /// serialized only when present, like the real envelope.
    #[serde(skip_serializing_if = "Option::is_none")]
    domain_code: Option<String>,
    data: Value,
    request_id: String,
}

impl WireErrorEnvelope {
    /// Mirrors `ApiErrorResponse::error(&RpcError::Engine(e), request_id)`.
    fn from_engine(error: &EngineError, request_id: &str) -> Self {
        Self {
            code: -32603, // RpcError::Engine json_rpc_code
            msg: error.to_string(),
            domain_code: Some(error.code().as_str().to_string()),
            data: Value::Null,
            request_id: request_id.to_string(),
        }
    }

    /// Mirrors `ApiErrorResponse::error(&RpcError::Session(e), request_id)`.
    fn from_session(error: &SessionError, request_id: &str) -> Self {
        let code = match error {
            // Session "not found" variants preserve the -32001 NotFound code.
            SessionError::NotFound(_) | SessionError::AgentNotFound(_) => -32001,
            _ => -32603,
        };
        Self {
            code,
            msg: error.to_string(),
            domain_code: Some(error.code().as_str().to_string()),
            data: Value::Null,
            request_id: request_id.to_string(),
        }
    }
}

/// Every `EngineError` variant, so the round-trip test covers all of them.
fn all_engine_error_variants() -> Vec<EngineError> {
    vec![
        EngineError::MaxStepsExceeded("loop hit 10 steps".to_string()),
        EngineError::ContextOverflow { attempts: 3 },
        EngineError::CompactionFailed("token budget exceeded".to_string()),
        EngineError::CompactionUnable("history not compactable".to_string()),
        EngineError::RequestInvalid("missing session_id".to_string()),
        EngineError::PermissionDenied("read blocked by policy".to_string()),
        EngineError::ToolExecutionFailed("bash exited with status 2".to_string()),
        EngineError::ProviderError("upstream 503".to_string()),
        EngineError::UserCancelled,
        EngineError::Aborted("user pressed stop".to_string()),
        EngineError::Internal("worker panicked".to_string()),
        EngineError::HookBlocked("hook denied the step".to_string()),
        EngineError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "profile.toml missing",
        )),
        EngineError::Json(serde_json::from_str::<Value>("{").unwrap_err()),
    ]
}

/// Every `SessionError` variant, so the round-trip test covers all of them.
fn all_session_error_variants() -> Vec<SessionError> {
    vec![
        SessionError::NotFound("sess-1".to_string()),
        SessionError::AlreadyExists("sess-1".to_string()),
        SessionError::AgentNotFound("agent-1".to_string()),
        SessionError::AgentAlreadyExists("agent-1".to_string()),
        SessionError::WireLog("append failed".to_string()),
        SessionError::StateFile("corrupt state.json".to_string()),
        SessionError::Snapshot("snapshot write failed".to_string()),
        SessionError::ResumeFailed("wire log replay failed".to_string()),
        SessionError::AlreadyClosed("sess-1".to_string()),
        SessionError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "denied",
        )),
        SessionError::Json(serde_json::from_str::<Value>("]").unwrap_err()),
    ]
}

/// should serialize every EngineError variant through the RPC envelope and
/// deserialize it back with code, message, and domain code intact
#[test]
fn should_roundtrip_all_engine_error_variants_through_serde() {
    for error in all_engine_error_variants() {
        let envelope = WireErrorEnvelope::from_engine(&error, "req-1");

        let json = serde_json::to_string(&envelope).expect("envelope should serialize");
        let decoded: WireErrorEnvelope =
            serde_json::from_str(&json).expect("envelope should deserialize");

        assert_eq!(decoded, envelope, "round-trip should preserve the envelope");
        assert_eq!(
            decoded.domain_code.as_deref(),
            Some(error.code().as_str()),
            "domain code should match the variant's ErrorCode"
        );
        assert!(!decoded.msg.is_empty(), "message should never be empty");
    }
}

/// should serialize every SessionError variant through the RPC envelope and
/// deserialize it back with code, message, and domain code intact
#[test]
fn should_roundtrip_all_session_error_variants_through_serde() {
    for error in all_session_error_variants() {
        let envelope = WireErrorEnvelope::from_session(&error, "req-1");

        let json = serde_json::to_string(&envelope).expect("envelope should serialize");
        let decoded: WireErrorEnvelope =
            serde_json::from_str(&json).expect("envelope should deserialize");

        assert_eq!(decoded, envelope, "round-trip should preserve the envelope");
        assert_eq!(
            decoded.domain_code.as_deref(),
            Some(error.code().as_str()),
            "domain code should match the variant's ErrorCode"
        );
        assert!(!decoded.msg.is_empty(), "message should never be empty");
    }
}

/// should preserve the source chain of wrapped errors when serializing
#[test]
fn should_preserve_error_chain_when_serializing() {
    // EngineError::Io carries its source via #[from]; source() must expose
    // the underlying io::Error so operators can see the full chain.
    let io_error = std::io::Error::new(std::io::ErrorKind::NotFound, "profile.toml missing");
    let engine = EngineError::Io(io_error);

    assert!(
        engine.source().is_some(),
        "Io variant should expose its source chain"
    );
    let mut chain: Vec<String> = Vec::new();
    let mut cursor = engine.source();
    while let Some(source) = cursor {
        chain.push(source.to_string());
        cursor = source.source();
    }
    assert!(
        chain
            .iter()
            .any(|link| link.contains("profile.toml missing")),
        "chain should include the io error: {chain:?}"
    );

    // The envelope keeps the outer context visible in the wire message while
    // the chain stays intact on the error object.
    let envelope = WireErrorEnvelope::from_engine(&engine, "req-1");
    assert!(envelope.msg.contains("I/O error"));
    assert_eq!(
        envelope.domain_code.as_deref(),
        Some("tool.execution_failed")
    );
}

/// should wrap unexpected panics with context into an EngineError::Internal
#[test]
fn should_wrap_unexpected_errors_with_context() {
    // Simulate catching a panicking worker (e.g. a hook or tool thread) and
    // converting the unwind payload into an EngineError carrying context.
    let result = catch_unwind(AssertUnwindSafe(|| {
        panic!("boom in worker");
    }));

    let payload = match result {
        Ok(()) => panic!("expected the closure to panic"),
        Err(payload) => payload,
    };
    let message = payload.downcast_ref::<&str>().copied().unwrap_or("unknown");
    let error = EngineError::Internal(format!("worker panicked: {message}"));

    assert_eq!(error.code(), ErrorCode::Internal);
    assert_eq!(error.code().as_str(), "internal");
    assert!(
        error
            .to_string()
            .contains("worker panicked: boom in worker"),
        "context should be preserved: {error}"
    );
}

/// should include the JSON-RPC code, message, and domain code fields in the
/// serialized JSON envelope
#[test]
fn should_include_error_code_and_message_in_json() {
    let error = EngineError::ProviderError("upstream 503".to_string());
    let envelope = WireErrorEnvelope::from_engine(&error, "req-abc");

    let value = serde_json::to_value(&envelope).expect("envelope should serialize");
    assert_eq!(value["code"], json!(-32603));
    assert_eq!(value["msg"], json!("Provider error: upstream 503"));
    assert_eq!(value["domain_code"], json!("provider.api_error"));
    assert_eq!(value["request_id"], json!("req-abc"));
    assert_eq!(value["data"], Value::Null);

    // A non-found session keeps its -32001 code, like the real RPC layer.
    let session = SessionError::NotFound("sess-404".to_string());
    let session_value =
        serde_json::to_value(WireErrorEnvelope::from_session(&session, "req-abc")).unwrap();
    assert_eq!(session_value["code"], json!(-32001));
    assert_eq!(session_value["domain_code"], json!("session.not_found"));
}

/// should preserve structured details (e.g. ContextOverflow.attempts) through
/// serialization
#[test]
fn should_preserve_error_details_when_serializing() {
    let error = EngineError::ContextOverflow { attempts: 7 };

    // The Display message embeds the structured detail...
    let msg = error.to_string();
    assert!(msg.contains("7"), "attempts should be visible: {msg}");

    // ...and the detail survives the wire round-trip.
    let envelope = WireErrorEnvelope::from_engine(&error, "req-1");
    let json = serde_json::to_string(&envelope).unwrap();
    let decoded: WireErrorEnvelope = serde_json::from_str(&json).unwrap();
    assert_eq!(decoded.msg, "Context overflow after 7 compaction attempts");
    assert_eq!(decoded.domain_code.as_deref(), Some("context.overflow"));
    assert_eq!(decoded.code, -32603);
}
