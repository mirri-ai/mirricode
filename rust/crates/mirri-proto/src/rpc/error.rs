//! D5 — the single JSON-RPC error-code mapping.
//!
//! Before M1.1 the two engines carried two hand-copied mappings:
//!
//! - `mirri-embedded`'s `embedded_server/mod.rs` defined `RPC_CODE_NOT_FOUND`
//!   / `RPC_CODE_INVALID_PARAMS` / `RPC_CODE_INTERNAL` plus per-family mapping
//!   helpers whose doc comments claimed to "mirror mirri-server".
//! - `mirri-server`'s `rpc.rs` derived codes from `RpcError::json_rpc_code()`.
//!
//! This module is the convergence point: the constants and the domain-code →
//! JSON-RPC-code mapping live here once, and both engines call into it
//! (`mirri-embedded` since M1.1; `mirri-server` since the M2 session-domain
//! migration, whose `RpcError::json_rpc_code()` routes engine/session errors
//! through [`json_rpc_code_for_domain_code`]). The server's own semantics —
//! its `Unauthorized` variant — stay server-side and are only *recorded* here
//! as constants.
//!
//! ## The mapping (facts as of M1.1)
//!
//! The domain error codes (`mirri-core`'s `ErrorCode::as_str()` strings —
//! `request.invalid`, `session.not_found`, …) map to JSON-RPC codes as:
//!
//! | domain code                     | JSON-RPC code | meaning        |
//! |---------------------------------|---------------|----------------|
//! | `request.invalid`               | `-32602`      | invalid params |
//! | `session.not_found`             | `-32001`      | not found      |
//! | `agent.not_found`               | `-32001`      | not found      |
//! | `not_implemented`               | `-32603`      | internal       |
//! | anything else (incl. unknown)   | `-32603`      | internal       |
//!
//! The two error families reaching this mapping (`mirri_core::EngineError`
//! and `mirri_core::SessionError`) produce disjoint domain-code namespaces,
//! so a single string-keyed mapping is exact — no per-family parameter.
//!
//! ## Divergence ledger (cleared)
//!
//! ~~`mirri-server`'s `json_rpc_code()` mapped **every** `RpcError::Engine(_)`
//! to `-32603`, including `request.invalid` engine errors.~~ Cleared by the
//! M2 session-domain migration: the server now routes engine/session errors
//! through [`json_rpc_code_for_domain_code`], so `request.invalid` answers
//! `-32602` on both backends and the two engines share one mapping exactly.

use serde_json::{Value, json};

/// JSON-RPC 2.0 "method not found" (`-32601`).
///
/// Server-side semantics today (`mirri-server`'s
/// `RpcError::MethodNotFound`); the embedded engine's unknown-method path
/// raises a transport-level `EngineError::MethodNotFound` instead. Recorded
/// here so the constant has one home when M2 converges the two.
pub const JSON_RPC_METHOD_NOT_FOUND: i64 = -32601;

/// Application "not found" (`-32001`).
///
/// Used by both backends for entity-lookup failures (`session.not_found`,
/// `agent.not_found`) so clients branching on the code keep working.
pub const JSON_RPC_NOT_FOUND: i64 = -32001;

/// JSON-RPC 2.0 "invalid params" (`-32602`).
///
/// Client-correctable failures: malformed params and the
/// `request.invalid` domain code.
pub const JSON_RPC_INVALID_PARAMS: i64 = -32602;

/// JSON-RPC 2.0 "internal error" (`-32603`).
///
/// The default for every domain error without a more specific mapping.
pub const JSON_RPC_INTERNAL: i64 = -32603;

/// Application "unauthorized" (`-32002`).
///
/// Server-only semantics today (`mirri-server`'s `RpcError::Unauthorized`;
/// the embedded engine has no auth layer). Recorded, not shared behavior.
pub const JSON_RPC_UNAUTHORIZED: i64 = -32002;

/// Map a domain error code string (e.g. `"request.invalid"`,
/// `"session.not_found"` — `mirri-core`'s `ErrorCode::as_str()` vocabulary)
/// to its JSON-RPC error code.
///
/// This is the single fact source for the mapping (plan decision D5). Both
/// engines' error paths call it — the embedded engine's helpers directly,
/// and `mirri-server`'s `RpcError::json_rpc_code()` for engine/session
/// errors (since the M2 session-domain migration); see the module docs for
/// the table.
pub fn json_rpc_code_for_domain_code(domain_code: &str) -> i64 {
    match domain_code {
        // Client-correctable request failures are invalid-params, not
        // internal (embedded behavior since the mapping's introduction).
        "request.invalid" => JSON_RPC_INVALID_PARAMS,
        // Entity-lookup failures keep the -32001 not-found code so clients
        // that branch on it continue to work. `provider.not_found` /
        // `model.not_found` are the TS `ModelCatalogErrors` codes the
        // modelResolver catalog lookups throw (klient contract parity).
        "session.not_found" | "agent.not_found" | "provider.not_found" | "model.not_found" => {
            JSON_RPC_NOT_FOUND
        }
        // A known-but-unimplemented operation is an explicit server-side
        // failure, never method-not-found (-32601 is reserved for unknown
        // methods): it stays an internal/application error like every other
        // engine-domain code, differentiated by the `domainCode` payload.
        // Explicitly listed (not just the `_` default) so intent survives.
        "not_implemented" => JSON_RPC_INTERNAL,
        // Everything else — engine, session, and unknown codes alike — is
        // an internal/application error.
        _ => JSON_RPC_INTERNAL,
    }
}

// ---------------------------------------------------------------------------
// Domain-code vocabulary adjudication (T7, F-P8-8)
// ---------------------------------------------------------------------------

/// TS vocabulary codes the rust engine **produces** on the wire (mapped):
/// every code here has a rust emission path — `ErrorCode::as_str()`
/// variants, the model-catalog lookups, or the embedded MCP-config face.
///
/// This list partitions the TS vocabulary with [`TS_ONLY_DOMAIN_CODES`]:
/// the fixture test in this module proves the two lists together cover
/// exactly the `codes.ts` code set (no gap, no stray entry).
pub const RUST_PRODUCED_TS_CODES: &[&str] = &[
    "loop.max_steps_exceeded",
    "context.overflow",
    "compaction.failed",
    "compaction.unable",
    "request.invalid",
    "provider.api_error",
    // Surfaced by the provider-error classification table
    // (`ProviderErrorInfo::provider_domain_code`) since T7.
    "provider.rate_limit",
    "provider.auth_error",
    "provider.connection_error",
    "not_implemented",
    "internal",
    "session.not_found",
    "session.already_exists",
    "session.closed",
    "session.init_failed",
    "agent.not_found",
    // The embedded MCP-config and plugin faces (`not_found_domain`).
    "mcp.server_not_found",
];

/// TS vocabulary codes the rust engine **cannot** produce (TS-only product
/// faces): SDK-level validation (`session.id_*`, `session.title_empty`, …),
/// goal/skill/plugin lifecycle codes whose rust equivalents surface as
/// `request.invalid` (via serde) or `internal`, and TS-only product errors.
///
/// Whitelisted so the fixture test stays exhaustive: a new TS code must be
/// adjudicated into this list or the produced list before the suite is
/// green again.
pub const TS_ONLY_DOMAIN_CODES: &[&str] = &[
    "config.invalid",
    "session.id_invalid",
    "session.id_required",
    "session.id_empty",
    "session.title_empty",
    "session.state_not_found",
    "session.state_invalid",
    "session.fork_active_turn",
    "session.export_not_found",
    "session.export_missing_version",
    "session.permission_mode_invalid",
    "session.thinking_empty",
    "session.model_empty",
    "session.plan_mode_invalid",
    "session.approval_handler_error",
    "session.question_handler_error",
    "turn.agent_busy",
    "goal.already_exists",
    "goal.not_found",
    "goal.objective_empty",
    "goal.objective_too_long",
    "goal.status_invalid",
    "goal.metadata_reserved",
    "goal.not_resumable",
    "model.not_configured",
    "model.config_invalid",
    "auth.login_required",
    "skill.not_found",
    "skill.type_unsupported",
    "skill.name_empty",
    "records.write_failed",
    "background.task_id_empty",
    "mcp.server_disabled",
    "mcp.startup_failed",
    "mcp.tool_name_collision",
    "plugin.not_found",
    "plugin.load_failed",
    "request.work_dir_required",
    "request.prompt_input_empty",
    "shell.git_bash_not_found",
];

/// Domain codes the rust engine produces that the TS v1 vocabulary does
/// **not** define (naming divergence): rust-specific namespaces
/// (`permission.*`, `tool.*`, `hook.*`, `abort.*`), the precise
/// quota-exhaustion code (`provider.quota_exhausted` — the TS v2
/// translation folds quota 429s into `provider.api_error`; rust keeps the
/// finer code, which the wire already surfaces since P8b-1), the model
/// catalog codes (`provider.not_found` / `model.not_found` — the v2
/// `ModelCatalogErrors` registry), the
/// import-conflict code (`provider.already_exists` — the TS edge
/// answers the same conflict as the numeric 40921 PROVIDER_ALREADY_EXISTS),
/// and the OAuth-managed import rejection (`provider.oauth_managed` — the
/// TS PROVIDER_OAUTH_MANAGED face: an OAuth-managed provider is never
/// rewritten by a directory import).
pub const RUST_ONLY_DOMAIN_CODES: &[&str] = &[
    "provider.quota_exhausted",
    "permission.denied",
    "tool.execution_failed",
    "hook.blocked",
    "abort.cancelled",
    "abort.user_cancelled",
    "provider.not_found",
    "model.not_found",
    "provider.already_exists",
    "provider.oauth_managed",
    // The write-side commit fence's rejection (session-consistency batch 2,
    // ADR-002): the session's main journal diverged from this process's
    // anchor before the user message would land on disk. Rust-only by
    // design — the TS v1 engine has no consistency fence, so the TS
    // vocabulary never needed the code. Not retryable: the client must
    // re-converge (resume) before resubmitting the same message.
    "session.conflict",
];

/// Classify whether a domain error code (e.g. `"context.overflow"`,
/// `"session.not_found"` — the string vocabulary `mirri-core`'s
/// `ErrorCode::as_str()` and the session/model-catalog layers produce) is
/// retryable, mirroring the TS `MIRRI_ERROR_INFO` metadata semantics.
///
/// This is the single fact source for the verdict (D5's error module owns
/// the whole mapping): `mirri-core`'s `ErrorCode::retryable()` delegates
/// here, and the wire surfaces (`domain_error_data`, the `turn.ended` error
/// object, the NAPI error suffix) all consult it. The true branch is synced
/// **per code** with the TS `MIRRI_ERROR_INFO` metadata by the fixture
/// test below — the eight retryable TS codes and nothing else. Static by
/// design — the engine's dynamic provider-retry decision (HTTP status
/// aware) happens *before* an error surfaces here; by the time a code
/// reaches the wire the engine's retries are exhausted, and a
/// non-retryable verdict is the honest answer. Unknown / future codes
/// degrade to non-retryable: never guess `true` — a wrong retry costs more
/// than a missed one.
pub fn domain_code_retryable(domain_code: &str) -> bool {
    match domain_code {
        // The TS MIRRI_ERROR_INFO retryable set, per-code synced (T7):
        // context overflow can clear after compaction; an active-turn fork
        // and a busy agent clear once the turn settles; rate limits and
        // connection failures clear with time; record writes and MCP
        // startup can succeed on retry (disk transient / server health).
        "context.overflow"
        | "session.fork_active_turn"
        | "turn.agent_busy"
        | "provider.rate_limit"
        | "provider.connection_error"
        | "records.write_failed"
        | "mcp.startup_failed"
        | "plugin.load_failed" => true,
        // Everything else — durable conditions (validation, permissions,
        // entity state), exhausted provider errors, cancellations, and
        // unknown codes alike — is not.
        _ => false,
    }
}

/// Build the error `data` payload that carries the domain code alongside the
/// JSON-RPC code: `{"domainCode": "<domain_code>", "retryable": <bool>}`.
///
/// Both backends attach the domain code this way so clients can correlate a
/// `-32603` to a precise reason (`provider.api_error`, `session.closed`, …)
/// and branch on the retryability verdict without a second lookup — the
/// TS `KimiErrorPayload` contract carries the same `retryable` field.
pub fn domain_error_data(domain_code: &str) -> Value {
    json!({
        "domainCode": domain_code,
        "retryable": domain_code_retryable(domain_code),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every branch of the mapping, plus the full known domain-code
    /// vocabulary (mirri-core `ErrorCode::as_str()`), asserted against the
    /// embedded engine's pre-M1.1 behavior so the convergence is provably
    /// zero-change.
    #[test]
    fn should_map_request_invalid_to_invalid_params() {
        assert_eq!(
            json_rpc_code_for_domain_code("request.invalid"),
            JSON_RPC_INVALID_PARAMS
        );
        assert_eq!(JSON_RPC_INVALID_PARAMS, -32602);
    }

    #[test]
    fn should_map_entity_not_found_codes_to_not_found() {
        assert_eq!(
            json_rpc_code_for_domain_code("session.not_found"),
            JSON_RPC_NOT_FOUND
        );
        assert_eq!(
            json_rpc_code_for_domain_code("agent.not_found"),
            JSON_RPC_NOT_FOUND
        );
        assert_eq!(JSON_RPC_NOT_FOUND, -32001);
    }

    #[test]
    fn should_map_every_engine_domain_code_to_internal_except_request_invalid() {
        // The complete engine-layer vocabulary of mirri-core's ErrorCode.
        let engine_codes = [
            "loop.max_steps_exceeded",
            "context.overflow",
            "compaction.failed",
            "compaction.unable",
            "provider.api_error",
            "provider.rate_limit",
            "provider.auth_error",
            "provider.connection_error",
            "internal",
            "not_implemented",
            "permission.denied",
            "tool.execution_failed",
            "hook.blocked",
            "abort.cancelled",
            "abort.user_cancelled",
        ];
        for code in engine_codes {
            assert_eq!(
                json_rpc_code_for_domain_code(code),
                JSON_RPC_INTERNAL,
                "engine code {code} must stay internal"
            );
        }
    }

    #[test]
    fn should_map_non_not_found_session_codes_to_internal() {
        // Session-layer codes that are not entity-not-found.
        let session_codes = [
            "session.already_exists",
            "session.closed",
            "session.init_failed",
        ];
        for code in session_codes {
            assert_eq!(
                json_rpc_code_for_domain_code(code),
                JSON_RPC_INTERNAL,
                "session code {code} must map to internal"
            );
        }
    }

    #[test]
    fn should_map_unknown_codes_to_internal() {
        // Unknown / future domain codes degrade to internal, never panic.
        assert_eq!(
            json_rpc_code_for_domain_code("totally.new.code"),
            JSON_RPC_INTERNAL
        );
        assert_eq!(json_rpc_code_for_domain_code(""), JSON_RPC_INTERNAL);
    }

    #[test]
    fn should_build_domain_code_data_payload() {
        assert_eq!(
            domain_error_data("provider.api_error"),
            json!({ "domainCode": "provider.api_error", "retryable": false })
        );
    }

    /// The retryable lookup covers the known domain-code vocabulary and
    /// degrades unknown codes to non-retryable (never panic, never guess
    /// true — a wrong retry is costlier than a missed one).
    #[test]
    fn should_classify_retryability_for_the_domain_code_vocabulary() {
        // Durable-condition failures — retrying cannot change the outcome.
        for code in [
            "loop.max_steps_exceeded",
            "compaction.failed",
            "compaction.unable",
            "request.invalid",
            "provider.api_error",
            "provider.auth_error",
            "not_implemented",
            "internal",
            "permission.denied",
            "tool.execution_failed",
            "hook.blocked",
            "abort.cancelled",
            "abort.user_cancelled",
            "session.not_found",
            "session.already_exists",
            "session.closed",
            "session.init_failed",
            "agent.not_found",
            "provider.not_found",
            "model.not_found",
            "mcp.server_not_found",
        ] {
            assert!(!domain_code_retryable(code), "{code}");
        }

        // Context overflow is recoverable by compaction/retry in the TS
        // MIRRI_ERROR_INFO semantics; the transient provider failures
        // (rate limit, connection) clear with time.
        assert!(domain_code_retryable("context.overflow"));
        assert!(domain_code_retryable("provider.rate_limit"));
        assert!(domain_code_retryable("provider.connection_error"));

        // Unknown / future codes degrade to non-retryable.
        assert!(!domain_code_retryable("totally.new.code"));
        assert!(!domain_code_retryable(""));
    }

    /// The data payload carries the retryability verdict alongside the
    /// domain code so clients can branch without a second round trip.
    #[test]
    fn should_carry_the_retryable_verdict_in_the_domain_error_data() {
        assert_eq!(
            domain_error_data("context.overflow"),
            json!({ "domainCode": "context.overflow", "retryable": true })
        );
        assert_eq!(
            domain_error_data("session.not_found"),
            json!({ "domainCode": "session.not_found", "retryable": false })
        );
    }

    #[test]
    fn should_pin_registered_code_constants() {
        assert_eq!(JSON_RPC_METHOD_NOT_FOUND, -32601);
        assert_eq!(JSON_RPC_NOT_FOUND, -32001);
        assert_eq!(JSON_RPC_INVALID_PARAMS, -32602);
        assert_eq!(JSON_RPC_INTERNAL, -32603);
        assert_eq!(JSON_RPC_UNAUTHORIZED, -32002);
    }
}

// ---------------------------------------------------------------------------
// TS vocabulary fixture tests (T7, F-P8-8)
// ---------------------------------------------------------------------------

/// The TS v1 error-code vocabulary — the parity fixture, read at compile
/// time (zero IO at runtime; contract 6 untouched). Single consumer of the
/// file: this module's parser plus the mirri-core vocabulary test (which
/// asserts against the declared lists instead of re-reading the fixture).
#[cfg(test)]
mod ts_vocabulary_fixture {
    use std::collections::BTreeSet;

    use super::*;

    /// Path: this file (`rust/crates/mirri-proto/src/rpc/`) → repo root.
    const TS_CODES_SOURCE: &str =
        include_str!("../../../../../packages/agent-core/src/errors/codes.ts");

    /// Parse the `ErrorCodes` const block: one `(key, code)` pair per
    /// `KEY: 'code',` line.
    fn parse_error_codes(src: &str) -> Vec<(String, String)> {
        let body = src
            .split("export const ErrorCodes = {")
            .nth(1)
            .and_then(|rest| rest.split("} as const").next())
            .expect("ErrorCodes block found");
        let mut entries = Vec::new();
        for line in body.lines() {
            let line = line.trim();
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim().trim_end_matches(',').trim();
            let Some(code) = value.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')) else {
                continue;
            };
            entries.push((key.trim().to_string(), code.to_string()));
        }
        entries
    }

    /// Parse the `MIRRI_ERROR_INFO` block: one `(code, retryable)` pair per
    /// metadata entry (quoted keys like `'config.invalid': {` and bare keys
    /// like `not_implemented: {` both carry a `retryable:` line).
    fn parse_retryable(src: &str) -> Vec<(String, bool)> {
        let body = src
            .split("export const MIRRI_ERROR_INFO = {")
            .nth(1)
            .and_then(|rest| rest.split("} as const").next())
            .expect("MIRRI_ERROR_INFO block found");
        let mut entries = Vec::new();
        let mut current: Option<String> = None;
        for line in body.lines() {
            let line = line.trim();
            if line.ends_with('{') {
                let key = line
                    .trim_end_matches('{')
                    .trim()
                    .trim_end_matches(':')
                    .trim()
                    .trim_matches('\'');
                current = Some(key.to_string());
                continue;
            }
            if let Some((field, value)) = line.split_once(':')
                && field.trim() == "retryable"
            {
                let value = value.trim().trim_end_matches(',').trim();
                if let Some(code) = &current {
                    entries.push((code.clone(), value == "true"));
                }
            }
        }
        entries
    }

    /// Given the TS vocabulary, every code is adjudicated exactly once:
    /// rust-produced or TS-only — the partition is exact (no gap where a TS
    /// code escapes adjudication, no stray entry the TS side does not
    /// define), and the three rust lists are pairwise disjoint.
    #[test]
    fn should_partition_the_ts_vocabulary_into_produced_and_ts_only_exactly() {
        // Given the TS vocabulary fixture
        let ts_entries = parse_error_codes(TS_CODES_SOURCE);
        assert!(
            ts_entries.len() >= 50,
            "fixture parse sanity: got {} entries",
            ts_entries.len()
        );
        let ts_codes: BTreeSet<&str> = ts_entries.iter().map(|(_, code)| code.as_str()).collect();
        assert_eq!(
            ts_codes.len(),
            ts_entries.len(),
            "TS code strings must be unique"
        );

        // And the rust adjudication lists
        let produced: BTreeSet<&str> = RUST_PRODUCED_TS_CODES.iter().copied().collect();
        let ts_only: BTreeSet<&str> = TS_ONLY_DOMAIN_CODES.iter().copied().collect();
        let rust_only: BTreeSet<&str> = RUST_ONLY_DOMAIN_CODES.iter().copied().collect();
        assert_eq!(
            produced.len(),
            RUST_PRODUCED_TS_CODES.len(),
            "RUST_PRODUCED_TS_CODES has duplicate entries"
        );
        assert_eq!(
            ts_only.len(),
            TS_ONLY_DOMAIN_CODES.len(),
            "TS_ONLY_DOMAIN_CODES has duplicate entries"
        );
        assert_eq!(
            rust_only.len(),
            RUST_ONLY_DOMAIN_CODES.len(),
            "RUST_ONLY_DOMAIN_CODES has duplicate entries"
        );

        // Then the lists are pairwise disjoint
        assert!(
            produced.is_disjoint(&ts_only),
            "produced ∩ ts-only must be empty"
        );
        assert!(
            produced.is_disjoint(&rust_only),
            "produced ∩ rust-only must be empty"
        );
        assert!(
            ts_only.is_disjoint(&rust_only),
            "ts-only ∩ rust-only must be empty"
        );
        assert!(
            rust_only.is_disjoint(&ts_codes),
            "rust-only codes must not collide with TS vocabulary codes"
        );

        // And produced ∪ ts-only equals the TS vocabulary exactly
        let adjudicated: BTreeSet<&str> = produced.union(&ts_only).copied().collect();
        assert_eq!(
            adjudicated, ts_codes,
            "every TS code must be adjudicated (produced or ts-only), with no strays"
        );
    }

    /// Per-code retryable sync: for every code in the TS vocabulary, the
    /// rust `domain_code_retryable` verdict equals the TS `MIRRI_ERROR_INFO`
    /// `retryable` flag. Deleting a true-branch mapping line (e.g.
    /// `"provider.rate_limit"`) turns this red.
    #[test]
    fn should_sync_retryability_with_the_ts_metadata_per_code() {
        // Given the TS metadata (every code carries a retryable verdict)
        let ts_entries = parse_error_codes(TS_CODES_SOURCE);
        let ts_codes: BTreeSet<String> = ts_entries.iter().map(|(_, code)| code.clone()).collect();
        let retryable_entries = parse_retryable(TS_CODES_SOURCE);
        let retryable_codes: BTreeSet<String> = retryable_entries
            .iter()
            .map(|(code, _)| code.clone())
            .collect();
        assert_eq!(
            retryable_codes, ts_codes,
            "every TS code must carry retryable metadata in the fixture"
        );

        // Then the rust table agrees per code
        for (code, ts_retryable) in &retryable_entries {
            assert_eq!(
                domain_code_retryable(code),
                *ts_retryable,
                "retryable verdict drift for {code}"
            );
        }

        // And the TS retryable set is exactly the true branch (a red here
        // means the true branch lists a code TS marks non-retryable).
        let expected_true: BTreeSet<&str> = retryable_entries
            .iter()
            .filter(|(_, retryable)| *retryable)
            .map(|(code, _)| code.as_str())
            .collect();
        let rust_true: BTreeSet<&str> = ts_codes
            .iter()
            .map(String::as_str)
            .filter(|code| domain_code_retryable(code))
            .collect();
        assert_eq!(
            rust_true, expected_true,
            "the rust true branch must match the TS retryable set exactly"
        );
    }

    /// Rust-only codes (naming divergence: rust namespaces, the precise
    /// quota code, catalog and provider faces) are all non-retryable — they
    /// describe durable conditions.
    #[test]
    fn should_classify_every_rust_only_code_as_non_retryable() {
        for code in RUST_ONLY_DOMAIN_CODES {
            assert!(
                !domain_code_retryable(code),
                "rust-only code {code} must be non-retryable"
            );
        }
    }
}
