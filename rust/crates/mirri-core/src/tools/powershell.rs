//! `PowerShellTool` — the model's native Windows shell runner (WO-3).
//!
//! Executes a PowerShell command through the PowerShell interpreter
//! (`pwsh.exe` preferred, `powershell.exe` — Windows PowerShell 5.1 — as the
//! fallback) via the [`OsProcess`] abstraction, reusing the Bash tool's
//! execution skeleton ([`bash::run_external_command`]): closed stdin,
//! concurrent stdout/stderr pumps, the Bash foreground timeout semantics,
//! and turn-cancel kills.
//!
//! Exit codes are honest (WO3-R8): native commands propagate
//! `$LASTEXITCODE` verbatim; cmdlet terminating errors make the PowerShell
//! process exit nonzero itself; a cmdlet's non-terminating error (the
//! PowerShell 5.1 default — error-stream writes that set no `$LASTEXITCODE`)
//! surfaces as exit 1 through the `-not $?` bridge (ADR-WO3-3). Both
//! console encodings are forced to UTF-8 up front, so output decoding is
//! UTF-8 on both sides.
//!
//! First version is foreground-only: the schema carries no
//! `run_in_background` and the description steers long-running work to the
//! Bash tool's background mode.

use std::path::PathBuf;
use std::sync::Arc;

use mirri_os::DynOsProcess;

use crate::cancel::CancelToken;
use crate::error::EngineResult;
use crate::task::{TaskKind, TaskManager};
use crate::tool::{
    AccessKind, AccessOperation, ExecutionPlan, Tool, ToolAccess, ToolExecution,
    ToolInputDisplay, ToolProgressHandle, ToolResult,
};
use crate::tools::tool_execution_error;

// The spawn-and-run phase rides these only on Windows (where the tool is
// registered); importing them there keeps the non-Windows build warning-free.
#[cfg(windows)]
use std::time::Duration;

#[cfg(windows)]
use mirri_os::ExecOptions;

#[cfg(windows)]
use crate::tools::bash;

/// Default command timeout when the model does not specify one (seconds) —
/// the Bash foreground semantics (bash.rs's `DEFAULT_TIMEOUT_SECS` mirror;
/// foreground-only, so no background variant exists here).
const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// Maximum allowed command timeout (seconds). Values above this are clamped —
/// the Bash foreground semantics.
const MAX_TIMEOUT_SECS: u64 = 300;

/// `PowerShell` — execute a native PowerShell command (Windows only).
///
/// The run is tracked as a foreground task row keyed by the model-side tool
/// call id, so `task.detach` fires the release the same way it does for the
/// Bash tool.
pub struct PowerShellTool {
    /// The session-scoped task manager backing the foreground task rows.
    tasks: Arc<TaskManager>,
    /// The owning agent's id — the `dispatcher` stamped on every task row the
    /// tool registers.
    agent_id: String,
    /// The session id stamped on task rows.
    session_id: String,
    /// The process provider every command executes through (zero-IO T13a).
    process: Arc<dyn DynOsProcess>,
    /// The session's workDir used when the model omits `cwd`.
    default_cwd: Option<PathBuf>,
}

impl PowerShellTool {
    /// Create the tool bound to the session-scoped task manager.
    ///
    /// `process` is the injected process provider; `default_cwd` is the
    /// session's workDir — the directory the command runs in when the model
    /// omits `cwd` (the same effectiveCwd rule the Bash tool applies).
    pub fn new(
        tasks: Arc<TaskManager>,
        agent_id: String,
        session_id: String,
        process: Arc<dyn DynOsProcess>,
        default_cwd: Option<PathBuf>,
    ) -> Self {
        Self {
            tasks,
            agent_id,
            session_id,
            process,
            default_cwd,
        }
    }
}

/// Parsed arguments for a single PowerShell invocation: `command` is
/// required; `cwd` and `timeout` follow the Bash foreground semantics.
struct PowerShellInput {
    command: String,
    cwd: Option<PathBuf>,
    timeout: Option<u64>,
}

/// Parse the raw JSON tool input into a [`PowerShellInput`]. Returns `None`
/// when the required `command` argument is missing or not a string.
fn parse_input(input: &serde_json::Value) -> Option<PowerShellInput> {
    Some(PowerShellInput {
        command: input.get("command")?.as_str()?.to_string(),
        cwd: input
            .get("cwd")
            .and_then(serde_json::Value::as_str)
            .map(PathBuf::from),
        timeout: input.get("timeout").and_then(serde_json::Value::as_u64),
    })
}

/// Build the `-Command` payload: encoding prelude + user command + exit-code
/// bridge (ADR-WO3-3).
///
/// - Encoding: `[Console]::OutputEncoding` and `$OutputEncoding` are forced
///   to UTF-8 so both the decoded output and the re-encoded input are UTF-8.
/// - Exit codes: the user command runs inside `& { ... }`; `$LASTEXITCODE`
///   (native commands) propagates verbatim, a cmdlet non-terminating error
///   (a bare `-not $?`) maps to exit 1, and anything else exits 0. A user
///   `exit N` terminates the process before the wrapper, with the same
///   honest outcome. A terminating error (`throw`, `-ErrorAction Stop`)
///   makes PowerShell itself exit nonzero, skipping the wrapper — also
///   correct.
///
/// Pure string construction, so the shape is testable on any host.
#[cfg(any(windows, test))]
pub(crate) fn build_ps_command_payload(user_command: &str) -> String {
    format!(
        "[Console]::OutputEncoding=[Text.Encoding]::UTF8; \
         $OutputEncoding=[Text.Encoding]::UTF8; \
         & {{ {user_command} }}; \
         if ($LASTEXITCODE -ne $null) {{ exit $LASTEXITCODE }} \
         elseif (-not $?) {{ exit 1 }} else {{ exit 0 }}"
    )
}

impl Tool for PowerShellTool {
    fn name(&self) -> &str {
        "PowerShell"
    }

    fn description(&self) -> &str {
        "Execute a PowerShell command and return its stdout and stderr as plain \
         text, with a status line for failures and timeouts. Use PowerShell \
         syntax (cmdlets, pipes, $variables); stderr is the main signal for \
         cmdlet failures — check it when a command does not do what you \
         expected. The command runs in the session's working directory unless \
         'cwd' is given; 'timeout' is in seconds (default 60, max 300 — \
         clamped). Runs in the foreground only: use the Bash tool's \
         run_in_background for long-running tasks. Prefer Read/Glob/Grep over \
         shell file access."
    }

    fn parameters(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The PowerShell command to execute."
                    },
                    "timeout": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 300,
                        "description": "Timeout in seconds. Defaults to 60, maximum 300; values above 300 are clamped to 300. The command is killed when the timeout fires."
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Working directory for the command. Defaults to the session's working directory."
                    }
                },
                "required": ["command"],
                "additionalProperties": false
            })
        })
    }

    fn resolve_execution(&self, input: serde_json::Value) -> EngineResult<ToolExecution> {
        let Some(input) = parse_input(&input) else {
            return Ok(tool_execution_error(
                "PowerShell requires a string 'command' argument",
            ));
        };
        // Foreground-only: the Bash foreground timeout semantics (default 60,
        // clamped to 300).
        let timeout_secs = input
            .timeout
            .unwrap_or(DEFAULT_TIMEOUT_SECS)
            .clamp(1, MAX_TIMEOUT_SECS);
        // The model-supplied `cwd` wins; otherwise the session's workDir.
        let cwd = input.cwd.clone().or_else(|| self.default_cwd.clone());

        let plan = ExecutionPlan {
            accesses: vec![ToolAccess {
                // A shell command can touch anything — treat it as
                // conflicting with every other tool call in the batch.
                kind: AccessKind::All,
                path: None,
                operation: AccessOperation::ReadWrite,
            }],
            display: ToolInputDisplay::Command {
                command: input.command.clone(),
            },
            approval_rule: "PowerShell".to_string(),
            stop_batch_after_this: false,
        };

        let process = Arc::clone(&self.process);
        let tasks = Arc::clone(&self.tasks);
        let session_id = self.session_id.clone();
        let agent_id = self.agent_id.clone();
        let command = input.command;
        Ok(ToolExecution::Runnable {
            plan,
            execute: Box::new(move |ctx| {
                Box::pin(run_foreground(
                    process,
                    command,
                    cwd,
                    Some(timeout_secs),
                    ctx.cancel,
                    ctx.progress,
                    tasks,
                    session_id,
                    agent_id,
                    ctx.tool_call_id,
                ))
            }),
        })
    }
}

/// Run one PowerShell command as a registered, detachable foreground task.
///
/// The task row is keyed by the model-side tool call id (B7 D-3 parity with
/// the Bash tool), so `task.detach` addresses it; the row settles through
/// the same transitions the Bash foreground runner applies:
///
/// - **Natural exit** → `Completed{exit_code}` with the accumulated output
///   appended, and the ToolResult is the captured text plus the status
///   line (exit 0 with no output → the success placeholder).
/// - **Turn cancel** → the child is killed and the row settles `Stopped`.
/// - **Foreground timeout** → per the Bash foreground semantics the run
///   would convert to background (ADR-4); this first version instead
///   kills the child and settles `Failed` with the timeout reason,
///   because the PS path has no background re-arm wiring — converting is
///   ROADMAP work with PS background support.
/// - **Spawn failure / PowerShell resolution failure** → the row settles
///   `Failed` and the ToolResult is the execution-error spelling
///   (ADR-WO3-5: resolution failures surface at execution time, the tool
///   stays registered).
// The context parameters (process, cwd, timeout, cancel, progress) feed only
// the Windows spawn-and-run phase; the non-Windows fallback ignores them.
#[cfg_attr(not(windows), allow(unused_variables))]
#[allow(clippy::too_many_arguments)]
async fn run_foreground(
    process: Arc<dyn DynOsProcess>,
    command: String,
    cwd: Option<PathBuf>,
    timeout_secs: Option<u64>,
    cancel: CancelToken,
    progress: ToolProgressHandle,
    tasks: Arc<TaskManager>,
    session_id: String,
    agent_id: String,
    tool_call_id: String,
) -> ToolResult {
    let description: String = {
        let preview: String = command.chars().take(60).collect();
        if command.chars().count() > 60 {
            format!("PowerShell: {preview}…")
        } else {
            format!("PowerShell: {preview}")
        }
    };
    let reg = tasks.register_foreground(
        TaskKind::Process {
            command: Some(command.clone()),
            dispatcher: Some(agent_id),
        },
        description,
        session_id,
        Some(tool_call_id),
    );
    let task_id = reg.task_id.clone();

    // The spawn and run phase is Windows-only; the non-Windows branch below
    // keeps the tool registered-but-errored honest (registration itself is
    // #[cfg(windows)], so this spelling is unreachable in practice — it
    // exists so the runner compiles and stays type-checked on every host).
    #[cfg(windows)]
    {
        return run_windows_shell(
            process,
            task_id,
            command,
            cwd,
            timeout_secs,
            cancel,
            progress,
            &tasks,
        )
        .await;
    }

    #[cfg(not(windows))]
    {
        let message = "The PowerShell tool is only available on Windows.".to_string();
        tasks.fail(&task_id, format!("PowerShell tool: {message}"));
        ToolResult::error(message)
    }
}

/// The Windows spawn-and-run phase of [`run_foreground`]: resolves the
/// PowerShell executable, builds the argv, rides the shared execution
/// skeleton, and settles the registered task row.
///
/// `Ok` and failure outcomes (timeout, spawn failure, resolution failure)
/// settle the row the same way [`run_foreground`]'s doc describes; a
/// resolution failure converts into the caller's execution-error spelling.
#[cfg(windows)]
#[allow(clippy::too_many_arguments)]
async fn run_windows_shell(
    process: Arc<dyn DynOsProcess>,
    task_id: String,
    command: String,
    cwd: Option<PathBuf>,
    timeout_secs: Option<u64>,
    cancel: CancelToken,
    progress: ToolProgressHandle,
    tasks: &TaskManager,
) -> ToolResult {
    // Resolution failures surface at execution time (ADR-WO3-5): the tool
    // stays registered; the model sees the actionable error and can switch
    // to the Bash tool.
    let shell = match mirri_os::shell::powershell_exe_for_tool() {
        Ok(shell) => shell,
        Err(error) => {
            tasks.fail(&task_id, format!("PowerShell tool: {error}"));
            return ToolResult::error(format!("PowerShell tool: {error}"));
        }
    };

    let payload = build_ps_command_payload(&command);
    let argv: [&str; 6] = [
        "-NoProfile",
        "-NonInteractive",
        "-ExecutionPolicy",
        "Bypass",
        "-Command",
        &payload,
    ];
    let opts = ExecOptions {
        cwd,
        env: bash::noninteractive_env(),
        timeout_ms: timeout_secs.map(|secs| secs * 1000),
    };

    let (exit_code, stdout, stderr, timed_out) = match bash::run_external_command(
        process.as_ref(),
        &shell,
        &argv,
        &opts,
        timeout_secs.map(Duration::from_secs),
        Some(&cancel),
        Some(&progress),
        None,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(error) => {
            tasks.fail(&task_id, format!("PowerShell failed to execute: {error}"));
            return ToolResult::error(format!("PowerShell failed to execute: {error}"));
        }
    };

    if timed_out {
        // The Bash foreground semantics convert a timed-out run to a
        // background task (ADR-4); this first PS version cannot re-arm a
        // background deadline, so the child was killed by the skeleton and
        // the row settles Failed with the timeout reason. PS background
        // support is ROADMAP work.
        let timeout_secs = timeout_secs.unwrap_or(0);
        tasks.fail(
            &task_id,
            format!("command timed out after {timeout_secs}s"),
        );
        return ToolResult::error(format!("command timed out after {timeout_secs}s"));
    }
    if stderr == "command cancelled" {
        // The cancel sentinel from the shared skeleton (a fired cancel
        // token killed the child): settle the row Stopped through the task
        // manager's stop path — the same mechanism the Bash foreground
        // runner's cancel arm uses — instead of Completed{-1}.
        tasks.stop(&task_id);
        return bash::format_external_result(-1, stdout, stderr);
    }
    tasks.append_output(&task_id, &stdout);
    tasks.append_output(&task_id, &stderr);
    tasks.complete(&task_id, exit_code);
    bash::format_external_result(exit_code, stdout, stderr)
}

#[cfg(test)]
mod tests {
    use super::build_ps_command_payload;

    /// Given a user command, when the -Command payload is built, then it
    /// opens with the UTF-8 encoding prelude, runs the command inside the
    /// `& { ... }` invocation operator, and closes with the exit-code
    /// bridge ($LASTEXITCODE verbatim, `-not $?` → 1, else 0).
    #[test]
    fn should_wrap_the_user_command_with_encoding_prelude_and_exit_code_bridge() {
        let payload = build_ps_command_payload("Write-Output hi");
        assert!(payload.starts_with("[Console]::OutputEncoding=[Text.Encoding]::UTF8;"));
        assert!(payload.contains("$OutputEncoding=[Text.Encoding]::UTF8;"));
        assert!(payload.contains("& { Write-Output hi };"));
        assert!(payload.contains("if ($LASTEXITCODE -ne $null) { exit $LASTEXITCODE }"));
        assert!(payload.contains("elseif (-not $?) { exit 1 } else { exit 0 }"));
    }
}
