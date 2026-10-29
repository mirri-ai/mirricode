//! External command hook engine.
//!
//! This module implements the execution of user-configured external command
//! hooks. It is the Rust counterpart of the TypeScript agent-core hook
//! system.
//!
//! # Architecture
//!
//! - [`types`] — type definitions: `HookEventType`, `HookDef`,
//!   `HookOutcome`, `HookTriggerResult`.
//! - [`matcher`] — regex-based pattern matching against hook input.
//! - [`runner`] — `run_hook`, executes a single hook command.
//! - [`engine`] — `HookEngine`, manages registrations and orchestrates
//!   parallel execution.
//! - [`bridge`] — `ExternalHooksBridge`, adapts [`HookEngine`] to the
//!   [`LoopHooks`](crate::hooks::LoopHooks) trait.
//! - [`lifecycle`] — in-process session lifecycle callbacks
//!   ([`SessionLifecycleHooks`]).
//!
//! # Wired event types
//!
//! | HookEventType         | Fired from                                                        |
//! |-----------------------|-------------------------------------------------------------------|
//! | `PreLlmRequest`       | bridge `before_step`                                              |
//! | `PostLlmRequest`      | bridge `after_step`                                               |
//! | `PreToolUse`          | [`TurnFlowHooks`](crate::turn_flow::TurnFlowHooks) `prepare_tool_execution` (deny → block; `additionalContext` → context injection; `updatedInput` → rewritten args) |
//! | `PermissionRequest`   | [`TurnFlowHooks`](crate::turn_flow::TurnFlowHooks) `authorize_tool_execution` (observation of the permission ask — never a gate) |
//! | `PermissionResult`    | permission decision points (bridge `authorize_tool_execution`, [`TurnFlowHooks`](crate::turn_flow::TurnFlowHooks) `authorize_tool_execution`, [`PermissionManager::before_tool_call`](crate::permission::PermissionManager::before_tool_call)) |
//! | `PostToolUse`         | bridge `finalize_tool_result` (success results only)               |
//! | `PostToolUseFailure`  | bridge `finalize_tool_result` (error results only — exactly one of the two fires per result) |
//! | `Stop`                | [`TurnFlowHooks`](crate::turn_flow::TurnFlowHooks) `should_continue_after_stop` (block → one forced continuation with the reason injected) |
//! | `UserPromptSubmit`    | [`TurnFlow::run_one_turn`](crate::TurnFlow::run_one_turn)         |
//! | `SessionStart`        | [`SessionManager`](crate::SessionManager) create/resume           |
//! | `SessionEnd`          | [`SessionManager`](crate::SessionManager) close                   |
//! | `PreCompact`          | [`TurnFlow`](crate::TurnFlow) compaction (auto + overflow)        |
//! | `PostCompact`         | [`TurnFlow`](crate::TurnFlow) compaction (auto + overflow)        |
//! | `SubagentStart`       | [`SessionSubagentHost`](crate::subagent::SessionSubagentHost) `spawn` |
//! | `SubagentStop`        | [`SessionSubagentHost`](crate::subagent::SessionSubagentHost) `spawn` (every exit path) |
//! | `StopFailure`         | [`TurnFlow::run_one_turn`](crate::TurnFlow::run_one_turn) (non-abort error)  |
//! | `Interrupt`           | [`TurnFlow::run_one_turn`](crate::TurnFlow::run_one_turn) (user cancellation) |
//! | `Notification`        | the notification delivery engine — [`fire_notification_hook`](crate::task_notification::fire_notification_hook), fired once per due notification at park or delivery (observation only) |
//!
//! # Superset event types
//!
//! `PreLlmRequest` and `PostLlmRequest` exist only in the Rust core (the
//! TS v2 `HookEventType` union has no counterparts); they are kept as
//! additive capability. Tool-input rewriting deliberately has NO superset
//! event: the `PreToolUse` hook's `updatedInput` output is the single
//! external rewrite channel (TS parity), and
//! [`LoopHooks::rewrite_tool_input`](crate::hooks::LoopHooks::rewrite_tool_input)
//! stays a host-injection seam without an external command hook.
//!
//! # Example
//!
//! ```no_run
//! use std::collections::HashMap;
//! use mirri_core::hook_engine::{HookEngine, HookEventType, HookDef};
//!
//! # async fn example() {
//! let engine = HookEngine::new();
//! engine.register(HookDef::new(HookEventType::PreToolUse, "echo hello"));
//!
//! let input = serde_json::json!({"tool_name": "Read"});
//! let result = engine
//!     .trigger(HookEventType::PreToolUse, input, &HashMap::new())
//!     .await;
//!
//! assert!(!result.blocked);
//! # }
//! ```

pub mod bridge;
pub mod engine;
pub mod lifecycle;
pub mod matcher;
pub mod runner;
pub mod types;

/// The process-wide Tokio runtime for bridge code that must block on async
/// hook triggers from synchronous contexts (issue 15C).
///
/// Synchronous trigger sites —
/// [`PermissionPolicy::evaluate`](crate::permission::PermissionPolicy), the
/// path-security guard, and the permission-result hook — may run on any
/// thread, including inside another runtime's async context, where
/// `block_in_place` panics. They escape to a fresh OS thread and block on
/// this shared multi-threaded runtime instead of creating a new runtime per
/// hook invocation, which is expensive.
pub(crate) fn shared_blocking_runtime() -> &'static tokio::runtime::Runtime {
    static SHARED_RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> =
        std::sync::OnceLock::new();
    SHARED_RUNTIME.get_or_init(|| {
        tokio::runtime::Runtime::new()
            .expect("failed to create shared Tokio runtime for hook triggers")
    })
}

// ---------------------------------------------------------------------------
// PermissionResult hook input
// ---------------------------------------------------------------------------

/// Outcome of a permission decision, used to build `PermissionResult` external
/// hook input (TS input shape: `{ turn_id, tool_call_id, tool_name, action,
/// decision, feedback?, error? }`).
///
/// Mirrors the TS `fireAndForgetTrigger('PermissionResult', ...)` in
/// `agent-core/src/agent/permission/index.ts`: `decision` is `"approved"` or
/// `"rejected"` with optional `feedback`, or `"error"` with an `error`
/// message when the permission flow itself failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PermissionResultOutcome {
    /// The tool call was approved.
    Approved,
    /// The tool call was rejected/denied (feedback carries the reason).
    Rejected { feedback: String },
    /// The permission flow errored (e.g. the approval RPC failed).
    Error { error: String },
}

impl PermissionResultOutcome {
    /// Hook `decision` value: `"approved"`, `"rejected"`, or `"error"`.
    pub(crate) fn decision(&self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::Rejected { .. } => "rejected",
            Self::Error { .. } => "error",
        }
    }

    /// Optional human-readable feedback for a rejection.
    pub(crate) fn feedback(&self) -> Option<&str> {
        match self {
            Self::Rejected { feedback } => Some(feedback),
            _ => None,
        }
    }

    /// Optional error message for error outcomes.
    pub(crate) fn error(&self) -> Option<&str> {
        match self {
            Self::Error { error } => Some(error),
            _ => None,
        }
    }

    /// Translate the outcome to the [`AuthorizeToolResult`] the loop engine
    /// acts on (approved → `Approved`; rejected/error → `Blocked`).
    pub(crate) fn into_authorize_tool_result(self) -> crate::hooks::AuthorizeToolResult {
        match self {
            Self::Approved => crate::hooks::AuthorizeToolResult::Approved { metadata: None },
            Self::Rejected { feedback } => {
                crate::hooks::AuthorizeToolResult::Blocked { reason: feedback }
            }
            Self::Error { error } => crate::hooks::AuthorizeToolResult::Blocked { reason: error },
        }
    }
}

/// Build the JSON input for a `PermissionResult` external hook.
///
/// The serializer adds the envelope fields (`hook_event_name`, `session_id`,
/// `cwd`) at command runtime; this payload carries the permission-specific
/// fields only. Matching uses the `tool_name` field (see [`matches`]).
pub(crate) fn permission_result_input(
    turn_id: u64,
    tool_call_id: &str,
    tool_name: &str,
    action: &str,
    outcome: &PermissionResultOutcome,
) -> serde_json::Value {
    let mut input = serde_json::json!({
        "turn_id": turn_id,
        "tool_call_id": tool_call_id,
        "tool_name": tool_name,
        "action": action,
        "decision": outcome.decision(),
    });
    if let Some(feedback) = outcome.feedback() {
        input["feedback"] = serde_json::Value::String(feedback.to_string());
    }
    if let Some(error) = outcome.error() {
        input["error"] = serde_json::Value::String(error.to_string());
    }
    input
}

/// Fire a `PermissionResult` external hook (fire-and-forget).
///
/// `PermissionResult` is an observation-only event (TS
/// `fireAndForgetTrigger`): the resulting block decision is ignored so a
/// misbehaving hook cannot change the already-made permission decision.
pub(crate) async fn fire_permission_result(
    engine: &HookEngine,
    env: &std::collections::HashMap<String, String>,
    turn_id: u64,
    tool_call_id: &str,
    tool_name: &str,
    action: &str,
    outcome: &PermissionResultOutcome,
) {
    let input = permission_result_input(turn_id, tool_call_id, tool_name, action, outcome);
    let _ = engine
        .trigger(HookEventType::PermissionResult, input, env)
        .await;
}

pub use bridge::ExternalHooksBridge;
pub use engine::HookEngine;
pub use lifecycle::{OrderedHookSlot, SessionHookCallback, SessionHookSlot, SessionLifecycleHooks};
pub use matcher::matches;
pub use runner::run_hook;
pub use types::{
    HookBlockSource, HookDef, HookEventType, HookExecutionResult, HookOutcome, HookSpecificOutput,
    HookStructuredOutput, HookTriggerResult,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_build_permission_result_input_when_outcome_is_approved() {
        // Given an approved permission outcome
        let outcome = PermissionResultOutcome::Approved;

        // When building the PermissionResult hook input
        let input = permission_result_input(7, "call-1", "Read", "execute Read", &outcome);

        // Then the input carries the envelope fields with decision "approved"
        // and no feedback/error.
        assert_eq!(input["turn_id"], 7);
        assert_eq!(input["tool_call_id"], "call-1");
        assert_eq!(input["tool_name"], "Read");
        assert_eq!(input["action"], "execute Read");
        assert_eq!(input["decision"], "approved");
        assert!(
            input.get("feedback").is_none(),
            "approved input has no feedback"
        );
        assert!(input.get("error").is_none(), "approved input has no error");
    }

    #[test]
    fn should_include_feedback_when_outcome_is_rejected() {
        // Given a rejected permission outcome with feedback
        let outcome = PermissionResultOutcome::Rejected {
            feedback: "denied by user".to_string(),
        };

        // When building the PermissionResult hook input
        let input = permission_result_input(7, "call-2", "Write", "execute Write", &outcome);

        // Then the input carries decision "rejected" with feedback and no error.
        assert_eq!(input["decision"], "rejected");
        assert_eq!(input["feedback"], "denied by user");
        assert!(input.get("error").is_none(), "rejected input has no error");
    }

    #[test]
    fn should_include_error_message_when_outcome_is_error() {
        // Given an error permission outcome
        let outcome = PermissionResultOutcome::Error {
            error: "approval request failed: boom".to_string(),
        };

        // When building the PermissionResult hook input
        let input = permission_result_input(0, "call-3", "Bash", "execute Bash", &outcome);

        // Then the input carries decision "error" with the error message and no
        // feedback.
        assert_eq!(input["decision"], "error");
        assert_eq!(input["error"], "approval request failed: boom");
        assert!(
            input.get("feedback").is_none(),
            "error input has no feedback"
        );
    }

    #[test]
    fn should_translate_rejected_outcome_to_blocked_authorize_tool_result() {
        // Given a rejected outcome, when translated, then the loop engine sees
        // a Blocked result with the feedback as reason.
        let outcome = PermissionResultOutcome::Rejected {
            feedback: "not allowed".to_string(),
        };
        assert!(matches!(
            outcome.clone().into_authorize_tool_result(),
            crate::hooks::AuthorizeToolResult::Blocked { reason } if reason == "not allowed"
        ));

        // And an error outcome also blocks.
        let outcome = PermissionResultOutcome::Error {
            error: "rpc gone".to_string(),
        };
        assert!(matches!(
            outcome.into_authorize_tool_result(),
            crate::hooks::AuthorizeToolResult::Blocked { reason } if reason == "rpc gone"
        ));
    }
}
