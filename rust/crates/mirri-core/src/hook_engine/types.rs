//! Type definitions for the external command hook engine.
//!
//! These types model the 19 hook event types from the TypeScript agent-core
//! hook system, the configuration shape of a user-defined external hook
//! ([`HookDef`]), and the execution result envelope ([`HookOutcome`]).

use std::collections::HashMap;

/// The 19 hook event types, aligned with the TypeScript `HookEventType`.
///
/// Each variant identifies a well-defined injection point in the agent loop
/// lifecycle where an external command may be run.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum HookEventType {
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    PermissionRequest,
    PermissionResult,
    UserPromptSubmit,
    Stop,
    StopFailure,
    Interrupt,
    SessionStart,
    SessionEnd,
    SubagentStart,
    SubagentStop,
    PreCompact,
    PostCompact,
    Notification,
    PreLlmRequest,
    PostLlmRequest,
}

/// A single external command hook definition.
///
/// The hook engine executes `command` in a shell when the matching `event`
/// fires. An optional `pattern` restricts the hook to inputs whose
/// `tool_name` field (or serialized JSON) matches the given regex.
///
/// Field aliases `matcher` (for `pattern`) and `timeout` (for seconds)
/// accept the TypeScript agent configuration spellings, so TOML configs
/// written against the TS config format deserialize without errors.
///
/// # Timeout unit conversion
///
/// The TypeScript config spells `timeout` in **seconds**; the Rust field
/// `timeout_ms` is in **milliseconds**. Deserialization normalizes the TS
/// spelling via a dedicated `timeout_secs` shadow input: `timeout = 5`
/// yields `timeout_ms = 5000`. When both spellings appear, the explicit
/// `timeout_ms` wins.
///
/// # Fail-closed behavior
///
/// When `fail_closed` is `true` (TS alias `failClosed`), an execution
/// failure (timeout or spawn error) produces a blocking outcome instead of
/// a recorded error — hooks with fail-closed semantics can gate operations
/// on their own successful execution.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct HookDef {
    pub event: HookEventType,
    pub command: String,
    #[serde(alias = "matcher")]
    pub pattern: Option<String>,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    pub env: Option<HashMap<String, String>>,
    /// The hook command's working directory. `None` (the default) inherits
    /// the engine process's working directory (TS parity: the `cwd` field
    /// on the TS `HookDef`).
    pub cwd: Option<String>,
    /// Whether execution failures (timeout, spawn error) block the
    /// operation ([`HookOutcome::Block`]) instead of being recorded as
    /// [`HookOutcome::Error`]. Defaults to `false` (fail-open).
    #[serde(default, alias = "failClosed")]
    pub fail_closed: bool,
}

impl HookDef {
    /// Convenience constructor with a sensible default timeout.
    pub fn new(event: HookEventType, command: impl Into<String>) -> Self {
        Self {
            event,
            command: command.into(),
            pattern: None,
            timeout_ms: default_timeout_ms(),
            env: None,
            cwd: None,
            fail_closed: false,
        }
    }
}

/// The raw deserialization shape of a [`HookDef`].
///
/// Kept separate from the public struct so `timeout`/`timeout_secs` can be
/// normalized into `timeout_ms` after deserialization (the TS config spells
/// `timeout` in seconds). `timeout_ms` is optional here so "absent" is
/// distinguishable from an explicit value.
#[derive(serde::Deserialize)]
struct HookDefInput {
    event: HookEventType,
    command: String,
    #[serde(alias = "matcher")]
    pattern: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    /// The TypeScript spelling `timeout` — seconds, converted to ms.
    #[serde(alias = "timeout")]
    timeout_secs: Option<u64>,
    env: Option<HashMap<String, String>>,
    /// The hook command's working directory (TS parity).
    cwd: Option<String>,
    #[serde(default, alias = "failClosed")]
    fail_closed: bool,
}

impl HookDefInput {
    /// Convert the raw input into a normalized [`HookDef`].
    ///
    /// Precedence: an explicit `timeout_ms` wins over the `timeout` seconds
    /// alias; when neither is present the engine default applies.
    fn into_hook_def(self) -> HookDef {
        let timeout_ms = match (self.timeout_ms, self.timeout_secs) {
            (Some(ms), _) => ms,
            (None, Some(secs)) => secs.saturating_mul(1000),
            (None, None) => default_timeout_ms(),
        };
        HookDef {
            event: self.event,
            command: self.command,
            pattern: self.pattern,
            timeout_ms,
            env: self.env,
            cwd: self.cwd,
            fail_closed: self.fail_closed,
        }
    }
}

impl<'de> serde::Deserialize<'de> for HookDef {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        HookDefInput::deserialize(deserializer).map(HookDefInput::into_hook_def)
    }
}

fn default_timeout_ms() -> u64 {
    30_000
}

/// Raw execution output of a hook command.
#[derive(Debug, Clone)]
pub struct HookExecutionResult {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

/// The outcome of executing a single hook, after interpreting exit codes
/// and structured JSON output.
#[derive(Debug, Clone)]
pub enum HookOutcome {
    /// Hook executed successfully and does not block the operation.
    Pass(HookExecutionResult),
    /// Hook blocks the operation.
    ///
    /// This happens when the command exits with code 2 or returns a
    /// structured output whose `permissionDecision` is `"deny"`.
    ///
    /// `message` is the user-facing display message (TS `HookResult.message`
    /// chain: stderr for exit 2, structured `message` for a deny); `reason`
    /// is the machine-facing block reason; `additional_context` carries a
    /// structured deny's `additionalContext` (TS parity: the deny branch
    /// keeps it while dropping `updatedInput` — an exit-2 block has none).
    Block {
        reason: String,
        message: Option<String>,
        additional_context: Option<String>,
        source: HookBlockSource,
    },
    /// Hook could not be executed or failed unexpectedly (timeout, command
    /// not found, I/O error).
    Error(String),
}

/// What caused a [`HookOutcome::Block`].
#[derive(Debug, Clone)]
pub enum HookBlockSource {
    /// The command exited with code 2.
    ExitCode2,
    /// The structured JSON output contained a deny decision.
    StructuredOutput,
    /// The hook could not be executed at all (timeout, spawn failure) and
    /// was configured with `fail_closed: true`.
    ExecutionFailure,
}

/// Structured JSON output parsed from the hook command's stdout.
///
/// A hook may emit a JSON object on stdout to communicate decisions and
/// modifications back to the engine. All fields are optional; the engine
/// acts only on the fields that are present. `message` and the
/// `hookSpecificOutput` sub-fields follow the TS `HookJsonOutput` shape:
/// the display message is the top-level `message` falling back to
/// `hookSpecificOutput.message`, and `additionalContext` / `updatedInput`
/// nest under `hookSpecificOutput` (the top-level spellings stay accepted
/// for configs written against the earlier rust shape).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct HookStructuredOutput {
    #[serde(rename = "hookSpecificOutput")]
    pub hook_specific_output: Option<HookSpecificOutput>,
    #[serde(rename = "updatedInput")]
    pub updated_input: Option<serde_json::Value>,
    #[serde(rename = "additionalContext")]
    pub additional_context: Option<String>,
    pub message: Option<String>,
}

/// The `hookSpecificOutput` sub-object of a [`HookStructuredOutput`].
#[derive(Debug, Clone, serde::Deserialize)]
pub struct HookSpecificOutput {
    #[serde(rename = "permissionDecision")]
    pub permission_decision: Option<String>,
    #[serde(rename = "permissionDecisionReason")]
    pub permission_decision_reason: Option<String>,
    pub message: Option<String>,
    #[serde(rename = "updatedInput")]
    pub updated_input: Option<serde_json::Value>,
    #[serde(rename = "additionalContext")]
    pub additional_context: Option<String>,
}

/// The merged result of triggering all hooks for a single event.
#[derive(Debug, Clone, Default)]
pub struct HookTriggerResult {
    /// Whether any hook blocked the operation.
    pub blocked: bool,
    /// Human-readable reason from the first blocking hook.
    pub block_reason: Option<String>,
    /// User-facing display message from the first blocking hook (the TS
    /// `HookResult.message` chain), if the blocking hook produced one.
    pub block_message: Option<String>,
    /// All `additionalContext` values from passing hooks, concatenated with
    /// newlines.
    pub additional_context: String,
    /// The display message rendered from the passing hooks' outputs
    /// (structured `message` falling back to trimmed stdout, invalid passes
    /// excluded), joined with blank lines — the TS `renderUserPromptHookResult`
    /// displayMessage.
    pub message: Option<String>,
    /// The last hook's `updatedInput`, if any hook provided one.
    pub updated_input: Option<serde_json::Value>,
    /// Per-hook results, in the order the hooks were registered.
    pub results: Vec<(HookDef, HookOutcome)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deserialize_hook(json: serde_json::Value) -> HookDef {
        serde_json::from_value(json).expect("hook def should deserialize")
    }

    #[test]
    fn should_convert_timeout_seconds_to_milliseconds_when_deserializing_with_timeout_field() {
        // Given a HookDef JSON using the TypeScript `timeout` spelling (seconds)
        let json = serde_json::json!({
            "event": "Stop",
            "command": "echo stop",
            "timeout": 5,
        });

        // When deserializing
        let hook = deserialize_hook(json);

        // Then the seconds value is converted to milliseconds
        assert_eq!(hook.timeout_ms, 5_000);
    }

    #[test]
    fn should_prefer_explicit_timeout_ms_when_both_timeout_spellings_are_present() {
        // Given a HookDef JSON with both `timeout_ms` (ms) and `timeout` (secs)
        let json = serde_json::json!({
            "event": "Stop",
            "command": "echo stop",
            "timeout_ms": 1_000,
            "timeout": 60,
        });

        // When deserializing
        let hook = deserialize_hook(json);

        // Then the explicit millisecond value wins over the seconds alias
        assert_eq!(hook.timeout_ms, 1_000);
    }

    #[test]
    fn should_default_timeout_to_30_seconds_when_no_timeout_field_is_present() {
        // Given a HookDef JSON without any timeout field
        let json = serde_json::json!({
            "event": "Stop",
            "command": "echo stop",
        });

        // When deserializing
        let hook = deserialize_hook(json);

        // Then the engine default (30s in ms) applies
        assert_eq!(hook.timeout_ms, 30_000);
    }

    #[test]
    fn should_default_fail_closed_to_false_when_field_is_omitted() {
        // Given a HookDef JSON without a fail_closed field
        let json = serde_json::json!({
            "event": "Stop",
            "command": "echo stop",
        });

        // When deserializing
        let hook = deserialize_hook(json);

        // Then fail_closed defaults to false (fail-open)
        assert!(!hook.fail_closed);
    }

    #[test]
    fn should_parse_fail_closed_when_camel_case_alias_is_used() {
        // Given a HookDef JSON using the TypeScript camelCase `failClosed` spelling
        let json = serde_json::json!({
            "event": "Stop",
            "command": "echo stop",
            "failClosed": true,
        });

        // When deserializing
        let hook = deserialize_hook(json);

        // Then the alias maps onto fail_closed
        assert!(hook.fail_closed);
    }

    #[test]
    fn should_parse_cwd_when_deserializing_with_the_ts_spelling() {
        // Given a HookDef JSON with the TS `cwd` field
        let json = serde_json::json!({
            "event": "PreToolUse",
            "command": "pwd",
            "cwd": "/tmp/hooks",
        });

        // When deserializing
        let hook = deserialize_hook(json);

        // Then the working directory is carried verbatim
        assert_eq!(hook.cwd.as_deref(), Some("/tmp/hooks"));
    }

    #[test]
    fn should_default_cwd_to_none_when_field_is_omitted() {
        // Given a HookDef JSON without a cwd field
        let json = serde_json::json!({
            "event": "Stop",
            "command": "echo stop",
        });

        // When deserializing
        let hook = deserialize_hook(json);

        // Then cwd is None (the engine process's working directory applies)
        assert!(hook.cwd.is_none());
    }

    #[test]
    fn should_round_trip_timeout_ms_and_fail_closed_when_serializing() {
        // Given a hook with explicit ms timeout and fail_closed set
        let hook = HookDef {
            event: HookEventType::Stop,
            command: "echo stop".to_string(),
            pattern: None,
            timeout_ms: 7_500,
            env: None,
            cwd: None,
            fail_closed: true,
        };

        // When serializing and deserializing back
        let json = serde_json::to_value(&hook).expect("hook should serialize");
        let restored = deserialize_hook(json);

        // Then timeout_ms and fail_closed survive the round trip unchanged
        assert_eq!(restored.timeout_ms, 7_500);
        assert!(restored.fail_closed);
    }

    #[test]
    fn should_define_permission_result_in_hook_event_type() {
        // Given the HookEventType enum, when PermissionResult is referenced,
        // then it exists as a variant and serializes to its TS hook event name.
        let event = HookEventType::PermissionResult;
        assert!(matches!(event, HookEventType::PermissionResult));
        assert_eq!(
            serde_json::to_string(&event).expect("event should serialize"),
            "\"PermissionResult\""
        );
    }
}
