//! Hook command runner — executes a single external command hook.
//!
//! The runner spawns the hook's `command` in a shell via the injected
//! [`DynOsProcess`] provider (zero-IO T13c-3), pipes the JSON input to stdin,
//! enforces a timeout, and interprets the exit code / structured stdout to
//! produce a [`HookOutcome`].

use std::collections::HashMap;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::timeout;

use mirri_os::{DynChildStdin, DynChildStream, DynOsProcess, ExecOptions};

use serde::de::Error as _;

use crate::hook_engine::types::{
    HookBlockSource, HookDef, HookExecutionResult, HookOutcome, HookStructuredOutput,
};

/// Execute a single hook command through the given process provider and
/// interpret the result.
///
/// # Behaviour
///
/// - The `command` string is run via `/bin/sh -c <command>` on Unix and
///   `cmd /C <command>` on Windows, spawned by `process`.
/// - `input` is serialized to JSON and piped to the command's stdin.
/// - `extra_env` is merged on top of the hook's own `env` map and the
///   current process environment.
/// - If the command does not finish within `hook.timeout_ms` it is killed
///   (its whole process group on Unix — the provider's
///   [`mirri_os::DynChildProcess::kill`]) and [`HookOutcome::Error`] is
///   returned — or [`HookOutcome::Block`] when `hook.fail_closed` is set.
///
/// # Exit-code interpretation
///
/// | Exit code | Outcome                                           |
/// |-----------|---------------------------------------------------|
/// | 0         | `Pass` (structured output may upgrade to `Block`) |
/// | 2         | `Block` (`HookBlockSource::ExitCode2`)           |
/// | other     | `Pass` (non-blocking)                             |
///
/// When stdout contains valid JSON it is parsed as
/// [`HookStructuredOutput`]. A `permissionDecision` of `"deny"` upgrades
/// the outcome to `Block` (`HookBlockSource::StructuredOutput`).
///
/// Execution failures (spawn failure, timeout) follow the fail-closed rule:
/// `fail_closed: true` produces `Block` (`HookBlockSource::ExecutionFailure`),
/// the default `false` produces `Error`.
pub async fn run_hook(
    process: &dyn DynOsProcess,
    hook: &HookDef,
    input: &serde_json::Value,
    extra_env: &HashMap<String, String>,
) -> HookOutcome {
    let stdin_data = serde_json::to_vec(input).unwrap_or_default();
    let timeout_duration = Duration::from_millis(hook.timeout_ms.max(1));

    let (shell, shell_args) = build_shell_command(&hook.command);

    // Merge environments: process env < hook env < extra_env. The provider
    // layers `opts.env` on top of the inherited parent environment, which is
    // the same precedence the old `command.env(k, v)` calls produced.
    let mut env: HashMap<String, String> = HashMap::new();
    if let Some(hook_env) = &hook.env {
        for (k, v) in hook_env {
            env.insert(k.clone(), v.clone());
        }
    }
    for (k, v) in extra_env {
        env.insert(k.clone(), v.clone());
    }

    let opts = ExecOptions {
        cwd: hook.cwd.as_ref().map(std::path::PathBuf::from),
        env,
        timeout_ms: None,
    };

    let arg_refs: Vec<&str> = shell_args.iter().map(String::as_str).collect();

    let mut child = match process.exec(&shell, &arg_refs, &opts).await {
        Ok(child) => child,
        Err(e) => {
            // Surface the underlying spawn failure (the root cause), matching
            // the previous `std::process` spawn error text.
            return execution_failure_outcome(
                hook,
                format!("Failed to spawn hook command: {}", e.root_cause()),
            );
        }
    };

    // Take the stdio handles so we can read them independently while waiting
    // for the process to exit. This avoids `wait_with_output` which consumes
    // the child and prevents us from killing it on timeout.
    let child_stdin: Option<DynChildStdin> = child.take_stdin_writer();
    let child_stdout: Option<DynChildStream> = child.take_stdout();
    let child_stderr: Option<DynChildStream> = child.take_stderr();

    // Write stdin concurrently so that the timeout still fires even if the
    // child blocks reading stdin.
    let stdin_task = tokio::spawn(async move {
        if let Some(mut stdin) = child_stdin {
            let _ = stdin.write_all(&stdin_data).await;
            let _ = stdin.shutdown().await;
        }
    });

    // Concurrently read stdout and stderr.
    let stdout_task = tokio::spawn(async move {
        match child_stdout {
            Some(mut s) => {
                let mut buf = Vec::new();
                let _ = s.read_to_end(&mut buf).await;
                buf
            }
            None => Vec::new(),
        }
    });

    let stderr_task = tokio::spawn(async move {
        match child_stderr {
            Some(mut s) => {
                let mut buf = Vec::new();
                let _ = s.read_to_end(&mut buf).await;
                buf
            }
            None => Vec::new(),
        }
    });

    // Wait for the process with a timeout.
    let wait_result = timeout(timeout_duration, child.wait()).await;

    match wait_result {
        Ok(Ok(code)) => {
            // Process exited normally. Collect the I/O results.
            let stdout_bytes = stdout_task.await.unwrap_or_default();
            let stderr_bytes = stderr_task.await.unwrap_or_default();
            let _ = stdin_task.await;

            let stdout = String::from_utf8_lossy(&stdout_bytes).to_string();
            let stderr = String::from_utf8_lossy(&stderr_bytes).to_string();
            // The erased trait reports signal termination as `-1` (there is
            // no `Option<i32>` at that boundary); map it back to `None` so
            // `interpret_result` sees the same shape `ExitStatus::code()`
            // gave before (a signal-killed hook is a non-blocking `Pass`).
            let exit_code = if code == -1 { None } else { Some(code) };

            interpret_result(exit_code, stdout, stderr, false)
        }
        Ok(Err(e)) => {
            let _ = stdin_task.await;
            HookOutcome::Error(format!("Hook process I/O error: {e}"))
        }
        Err(_) => {
            // Timed out — kill the process (its whole process group on
            // Unix, SIGTERM then SIGKILL after a grace period, via the
            // provider's `kill`) and reap it.
            let _ = child.kill().await;
            // Best-effort cleanup of the spawned tasks.
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            let _ = stdin_task.await;

            execution_failure_outcome(hook, format!("Hook timed out after {}ms", hook.timeout_ms))
        }
    }
}

/// Build the outcome for a hook that could not be executed (timeout, spawn
/// failure).
///
/// With `fail_closed: true` the failure **blocks** the operation
/// ([`HookOutcome::Block`] with [`HookBlockSource::ExecutionFailure`]); with
/// the default `fail_closed: false` it is recorded as a non-blocking
/// [`HookOutcome::Error`].
fn execution_failure_outcome(hook: &HookDef, message: String) -> HookOutcome {
    if hook.fail_closed {
        HookOutcome::Block {
            reason: message,
            // TS failResult: an execution failure carries no display
            // message; the reason (or the default) is what surfaces.
            message: None,
            additional_context: None,
            source: HookBlockSource::ExecutionFailure,
        }
    } else {
        HookOutcome::Error(message)
    }
}

/// Interpret the raw execution output into a [`HookOutcome`].
///
/// This is extracted as a pure function so it can be unit-tested without
/// spawning processes.
pub(crate) fn interpret_result(
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    timed_out: bool,
) -> HookOutcome {
    if timed_out {
        return HookOutcome::Error("Hook timed out".to_string());
    }

    let result = HookExecutionResult {
        exit_code,
        stdout: stdout.clone(),
        stderr,
        timed_out: false,
    };

    // Try to parse structured JSON output regardless of exit code.
    if let Ok(structured) = parse_structured_output(&stdout)
        && let Some(hso) = &structured.hook_specific_output
        && hso.permission_decision.as_deref() == Some("deny")
    {
        return HookOutcome::Block {
            // TS structured deny: the reason is the decision reason,
            // undefined when the hook supplied none — an empty reason
            // normalises to None at the merge so consumer fallbacks
            // (the "Blocked by <event> hook" defaults) engage.
            reason: hso
                .permission_decision_reason
                .as_deref()
                .map(str::trim)
                .unwrap_or_default()
                .to_string(),
            // TS structured deny: message = top-level message falling
            // back to hookSpecificOutput.message.
            message: structured
                .message
                .or_else(|| hso.message.clone())
                .filter(|m| !m.trim().is_empty()),
            // TS structured deny: additionalContext rides along (only
            // updatedInput is dropped) — hookSpecificOutput first, then the
            // top-level spelling.
            additional_context: hso
                .additional_context
                .clone()
                .or_else(|| structured.additional_context.clone())
                .filter(|c| !c.is_empty()),
            source: HookBlockSource::StructuredOutput,
        };
    }

    match exit_code {
        Some(0) => HookOutcome::Pass(result),
        Some(2) => {
            // TS resultFromExitCode: the stderr text is both the display
            // message and the block reason. Exit 2 carries no
            // additionalContext (the TS exit-2 branch returns none).
            let stderr_text = result.stderr.trim().to_string();
            HookOutcome::Block {
                reason: stderr_text,
                message: Some(result.stderr.trim().to_string()),
                additional_context: None,
                source: HookBlockSource::ExitCode2,
            }
        }
        Some(_) => HookOutcome::Pass(result),
        None => HookOutcome::Pass(result),
    }
}

/// Attempt to parse the stdout as a [`HookStructuredOutput`].
///
/// Returns `Ok` only when the stdout is valid JSON and (at minimum) looks
/// like an object. Whitespace-only or non-JSON stdout yields `Err`.
pub(crate) fn parse_structured_output(
    stdout: &str,
) -> Result<HookStructuredOutput, serde_json::Error> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        // Empty stdout is not an error per se, but there's nothing to parse.
        // We return an error so the caller treats it as "no structured output".
        return Err(serde_json::Error::custom("empty stdout"));
    }
    serde_json::from_str(trimmed)
}

/// Build the platform-specific shell command (program + arguments) to hand
/// to the process provider.
fn build_shell_command(command_str: &str) -> (String, Vec<String>) {
    #[cfg(unix)]
    {
        (
            "/bin/sh".to_string(),
            vec!["-c".to_string(), command_str.to_string()],
        )
    }

    #[cfg(not(unix))]
    {
        (
            "cmd".to_string(),
            vec!["/C".to_string(), command_str.to_string()],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hook_engine::types::HookBlockSource;

    fn local_process() -> mirri_os::LocalProcess {
        mirri_os::LocalProcess::new()
    }

    #[test]
    fn should_return_pass_when_exit_code_is_zero() {
        let outcome = interpret_result(Some(0), String::new(), String::new(), false);
        assert!(matches!(outcome, HookOutcome::Pass(_)));
    }

    #[test]
    fn should_block_with_exit_code_2_source_when_exit_code_is_two() {
        let outcome = interpret_result(Some(2), String::new(), String::new(), false);
        match outcome {
            HookOutcome::Block { source, .. } => {
                assert!(matches!(source, HookBlockSource::ExitCode2));
            }
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn should_return_pass_when_exit_code_is_nonzero_non_two() {
        let outcome = interpret_result(Some(1), String::new(), String::new(), false);
        assert!(matches!(outcome, HookOutcome::Pass(_)));
    }

    #[test]
    fn should_block_with_structured_output_source_when_permission_decision_is_deny() {
        let stdout = r#"{"hookSpecificOutput":{"permissionDecision":"deny"}}"#;
        let outcome = interpret_result(Some(0), stdout.to_string(), String::new(), false);
        match outcome {
            HookOutcome::Block { source, .. } => {
                assert!(matches!(source, HookBlockSource::StructuredOutput));
            }
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn should_return_pass_when_permission_decision_is_allow() {
        let stdout = r#"{"hookSpecificOutput":{"permissionDecision":"allow"}}"#;
        let outcome = interpret_result(Some(0), stdout.to_string(), String::new(), false);
        assert!(matches!(outcome, HookOutcome::Pass(_)));
    }

    #[test]
    fn should_return_err_when_parsing_empty_stdout() {
        assert!(parse_structured_output("").is_err());
        assert!(parse_structured_output("   \n  ").is_err());
    }

    #[test]
    fn should_parse_structured_output_when_stdout_is_valid_json() {
        let stdout = r#"{"additionalContext":"hello"}"#;
        let parsed = parse_structured_output(stdout).unwrap();
        assert_eq!(parsed.additional_context.as_deref(), Some("hello"));
    }

    #[test]
    fn should_carry_stderr_as_message_and_reason_when_exit_code_is_two() {
        // Given a hook that exited 2 with a stderr explanation
        let outcome = interpret_result(Some(2), String::new(), "no way".to_string(), false);

        // Then the block carries stderr as both message and reason (TS
        // resultFromExitCode), not a generic placeholder
        match outcome {
            HookOutcome::Block {
                message, reason, ..
            } => {
                assert_eq!(message.as_deref(), Some("no way"));
                assert_eq!(reason, "no way");
            }
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn should_carry_the_structured_message_and_decision_reason_when_denied() {
        // Given a structured deny with a top-level message and a decision
        // reason
        let stdout = r#"{"message":"stop it","hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"policy"}}"#.to_string();
        let outcome = interpret_result(Some(0), stdout, String::new(), false);

        // Then the block message is the structured message and the reason
        // is the decision reason
        match outcome {
            HookOutcome::Block {
                message, reason, ..
            } => {
                assert_eq!(message.as_deref(), Some("stop it"));
                assert_eq!(reason, "policy");
            }
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn should_fall_back_to_hook_specific_message_when_denied_without_top_level() {
        // Given a structured deny whose message rides only under
        // hookSpecificOutput
        let stdout =
            r#"{"hookSpecificOutput":{"permissionDecision":"deny","message":"nested stop"}}"#
                .to_string();
        let outcome = interpret_result(Some(0), stdout, String::new(), false);

        // Then the block message falls back to the nested message and the
        // reason is empty (the TS deny reason is the decision reason,
        // undefined here — the merge normalises it to None)
        match outcome {
            HookOutcome::Block {
                message, reason, ..
            } => {
                assert_eq!(message.as_deref(), Some("nested stop"));
                assert_eq!(reason, "");
            }
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn should_render_the_default_text_when_a_bare_deny_has_no_message_or_reason() {
        // Given a bare structured deny (no message, no decision reason)
        let stdout = r#"{"hookSpecificOutput":{"permissionDecision":"deny"}}"#.to_string();
        let outcome = interpret_result(Some(0), stdout, String::new(), false);

        // Then both the message and the reason are empty — the merge
        // normalises them to None so the "Blocked by <event> hook" default
        // surfaces downstream (the TS render chain)
        match outcome {
            HookOutcome::Block {
                message, reason, ..
            } => {
                assert!(message.is_none());
                assert_eq!(reason, "");
            }
            other => panic!("expected Block, got {other:?}"),
        }
    }

    fn hook_with_fail_closed(fail_closed: bool) -> HookDef {
        HookDef {
            event: crate::hook_engine::types::HookEventType::Stop,
            command: "echo x".to_string(),
            pattern: None,
            timeout_ms: 1_000,
            env: None,
            cwd: None,
            fail_closed,
        }
    }

    #[test]
    fn should_block_with_execution_failure_source_when_hook_fails_to_execute_and_fail_closed() {
        // Given a fail-closed hook and an execution failure message
        let hook = hook_with_fail_closed(true);

        // When converting the failure into an outcome
        let outcome = execution_failure_outcome(&hook, "Hook timed out after 1000ms".to_string());

        // Then the failure blocks with the ExecutionFailure source
        match outcome {
            HookOutcome::Block { reason, source, .. } => {
                assert!(matches!(source, HookBlockSource::ExecutionFailure));
                assert!(reason.contains("timed out"));
            }
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[test]
    fn should_return_error_when_hook_fails_to_execute_and_fail_closed_is_false() {
        // Given a fail-open hook and an execution failure message
        let hook = hook_with_fail_closed(false);

        // When converting the failure into an outcome
        let outcome =
            execution_failure_outcome(&hook, "Failed to spawn hook command: nope".to_string());

        // Then the failure is recorded as a non-blocking error
        match outcome {
            HookOutcome::Error(message) => assert!(message.contains("Failed to spawn")),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn should_block_when_hook_times_out_and_fail_closed_is_true() {
        // Given a fail-closed hook whose command sleeps past the timeout
        let mut hook = hook_with_fail_closed(true);
        hook.command = "sleep 5".to_string();
        hook.timeout_ms = 100;

        // When the hook runs
        let outcome = run_hook(
            &local_process(),
            &hook,
            &serde_json::json!({}),
            &HashMap::<String, String>::new(),
        )
        .await;

        // Then the timeout blocks the operation
        match outcome {
            HookOutcome::Block { reason, source, .. } => {
                assert!(matches!(source, HookBlockSource::ExecutionFailure));
                assert!(reason.contains("timed out"));
            }
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn should_return_error_when_hook_times_out_and_fail_closed_is_false() {
        // Given a fail-open hook whose command sleeps past the timeout
        let mut hook = hook_with_fail_closed(false);
        hook.command = "sleep 5".to_string();
        hook.timeout_ms = 100;

        // When the hook runs
        let outcome = run_hook(
            &local_process(),
            &hook,
            &serde_json::json!({}),
            &HashMap::<String, String>::new(),
        )
        .await;

        // Then the timeout is recorded as a non-blocking error
        match outcome {
            HookOutcome::Error(message) => assert!(message.contains("timed out")),
            other => panic!("expected Error, got {other:?}"),
        }
    }
}
