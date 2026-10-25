//! `BashTool` — the model's shell command runner.
//!
//! Executes a command through the shell (`sh -c` on Unix, `cmd /C` on
//! Windows) via the [`OsProcess`] abstraction. The command runs in the
//! session's working directory unless `cwd` is given, and is capped by an
//! optional timeout (seconds; foreground default 60 / max 300 — clamped;
//! background default 600 / max 86400 — over-cap rejected). A foreground
//! command that hits its timeout is NOT killed — it is moved to a background
//! task (ADR-4) that re-arms the background timeout and kills the process
//! group only if that second deadline elapses (SIGTERM, then SIGKILL after
//! 5s), settling the task as failed with the timeout reason (ADR-013: results
//! are plain text, never a JSON envelope).
//!
//! Commands run with a non-interactive environment (`NO_COLOR=1`,
//! `TERM=dumb`, `GIT_TERMINAL_PROMPT=0`, parent `SHELL` preserved) and a
//! closed stdin, so tools like `git` and `cat` fail fast or see EOF instead
//! of hanging on a prompt. Foreground runs also observe the turn's cancel
//! token: cancellation kills the process group.
//!
//! Every foreground run is registered as a **foreground task** (B7 D-3,
//! `detached=false`, keyed by the model-side tool call id) whose release
//! permit a `task.detach` fires: the run then returns the
//! moved-to-background result while the still-running child hands over to a
//! background continuation that settles the task
//! ([`TaskManager::settle_background_run`]). A timed-out foreground run
//! converts the same way (ADR-4) instead of being killed; only a turn-cancelled
//! foreground run is killed (D-4), and its task row settles — never a dangling
//! `Running` row.
//!
//! With `run_in_background: true` the command is tracked as a session
//! background task via [`TaskManager`] (optionally labelled with
//! `description`, and exempt from timeouts with `disable_timeout: true`) and
//! the call returns its task id immediately instead of waiting for the
//! command to finish.
//!
//! Every background task falls under the global lifetime ceiling
//! (US14.4): `disable_timeout` removes the per-run deadline only, not the
//! age-based ceiling. `exempt_lifetime: true` lifts the ceiling for that
//! one task — once per task, never appendable (a longer-lived need means
//! re-dispatching the work).

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use mirri_os::{DynChildProcess, DynOsProcess, ExecOptions, ProcessFingerprint, probe_process};
use tokio::sync::mpsc;

use crate::cancel::CancelToken;
use crate::error::EngineResult;
use crate::shell_command::{ShellChunk, pump_stream};
use crate::task::{ForegroundReleaseReason, TaskKind, TaskManager};
use crate::tool::{
    AccessKind, AccessOperation, ExecutionPlan, Tool, ToolAccess, ToolExecution, ToolInputDisplay,
    ToolOutput, ToolProgressHandle, ToolResult, ToolUpdate, ToolUpdateKind,
};
use crate::tools::tool_execution_error;

/// Default command timeout when the model does not specify one (seconds).
const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// Maximum allowed command timeout (seconds). Values above this are clamped.
const MAX_TIMEOUT_SECS: u64 = 300;

/// Default background task timeout when the model does not specify one
/// (seconds) — TS parity (`DEFAULT_BACKGROUND_TIMEOUT_S = 10 * 60`).
const DEFAULT_BACKGROUND_TIMEOUT_SECS: u64 = 10 * 60;

/// Maximum allowed background task timeout (seconds). Values above this are
/// rejected — TS parity (`MAX_BACKGROUND_TIMEOUT_S = 24 * 60 * 60`; the TS
/// schema rejects rather than clamps).
const MAX_BACKGROUND_TIMEOUT_SECS: u64 = 24 * 60 * 60;

/// `Bash` — execute a shell command and capture its output.
///
/// Background runs are tracked through the session-scoped [`TaskManager`] so
/// `TaskList` / `TaskOutput` / `TaskStop` can observe them.
pub struct BashTool {
    /// The session-scoped task manager backing background runs.
    tasks: Arc<TaskManager>,
    /// The owning agent's id — stamped as the `dispatcher` of every task
    /// the tool registers, so a subagent-started background bash task's
    /// completion notification routes back to that subagent (not the
    /// session's main agent).
    agent_id: String,
    /// The session id stamped on background tasks.
    session_id: String,
    /// The process provider every command executes through (zero-IO T13a).
    process: Arc<dyn DynOsProcess>,
    /// The session's workDir used when the model omits `cwd`
    /// (TS parity: `effectiveCwd = args.cwd ?? this.cwd`, bash.ts:322).
    default_cwd: Option<PathBuf>,
}

impl BashTool {
    /// Create the tool bound to the session-scoped task manager.
    /// `process` is the injected process provider (the engine default is the
    /// local one). `default_cwd` is the session's workDir — the directory the
    /// command runs in when the model omits `cwd` (audit P0-1).
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

/// Parsed arguments for a single Bash invocation.
///
/// Mirrors the TS `BashInput` schema: `command` is required; `cwd`, `timeout`
/// and `description` are strings/numbers; `run_in_background` and
/// `disable_timeout` are booleans.
pub struct BashInput {
    pub command: String,
    pub cwd: Option<PathBuf>,
    pub timeout: Option<u64>,
    pub run_in_background: Option<bool>,
    pub disable_timeout: Option<bool>,
    /// Exempt this one task from the lifetime ceiling (US14.4). Once per
    /// task, never appendable — a longer-lived need means re-dispatching.
    pub exempt_lifetime: Option<bool>,
    pub description: Option<String>,
}

/// Parse the raw JSON tool input into a [`BashInput`]. Returns `None` when
/// the required `command` argument is missing.
fn parse_input(input: &serde_json::Value) -> Option<BashInput> {
    Some(BashInput {
        command: input.get("command")?.as_str()?.to_string(),
        cwd: input
            .get("cwd")
            .and_then(serde_json::Value::as_str)
            .map(PathBuf::from),
        timeout: input.get("timeout").and_then(serde_json::Value::as_u64),
        run_in_background: input
            .get("run_in_background")
            .and_then(serde_json::Value::as_bool),
        disable_timeout: input
            .get("disable_timeout")
            .and_then(serde_json::Value::as_bool),
        exempt_lifetime: input
            .get("exempt_lifetime")
            .and_then(serde_json::Value::as_bool),
        description: input
            .get("description")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
    })
}

impl Tool for BashTool {
    fn name(&self) -> &str {
        "Bash"
    }

    fn description(&self) -> &str {
        "Execute a shell command and return its stdout and stderr as plain \
         text, with a status line for failures and timeouts. \
         The command runs in the session's working directory unless 'cwd' is given. \
         'timeout' is in seconds (foreground: default 60, max 300 — clamped; \
         background: default 600, max 86400 — over-cap rejected); a foreground \
         command that hits its timeout is moved to the background instead of being \
         killed, and you will be automatically notified when it completes. Commands run non-interactively \
         (closed stdin, no color or prompts). Set 'run_in_background' to true to \
         start the command as a background task and return its task id \
         immediately; 'disable_timeout' removes the timeout (the background task \
         is still bounded by the global lifetime ceiling unless 'exempt_lifetime' \
         exempts it), and 'description' labels the background task. \
         Prefer Read/Glob/Grep over shell file access."
    }

    fn parameters(&self) -> &serde_json::Value {
        static SCHEMA: std::sync::OnceLock<serde_json::Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute."
                    },
                    "timeout": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 86400,
                        "description": "Timeout in seconds. Foreground: defaults to 60, maximum 300; values above 300 are clamped to 300. Background: defaults to 600, maximum 86400; values above 86400 are rejected. A foreground command that hits its timeout is moved to the background. Ignored when disable_timeout is true."
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Working directory for the command. Defaults to the session's working directory."
                    },
                    "run_in_background": {
                        "type": "boolean",
                        "description": "Whether to run the command as a background task. When true, the call returns a task_id immediately instead of waiting for the command to finish."
                    },
                    "disable_timeout": {
                        "type": "boolean",
                        "description": "When true, no timeout is applied to the command. Background tasks still fall under the global lifetime ceiling; use exempt_lifetime to lift that for this one task."
                    },
                    "exempt_lifetime": {
                        "type": "boolean",
                        "description": "When true, this task is exempt from the global lifetime ceiling. Once per task, never appendable — to run longer, start a new task."
                    },
                    "description": {
                        "type": "string",
                        "description": "A short description for the background task; used when run_in_background is true. Defaults to a preview of the command."
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
                "Bash requires a string 'command' argument",
            ));
        };
        let (foreground_timeout_secs, background_rearm_secs) = match effective_timeout_secs(&input)
        {
            EffectiveTimeout::Disabled => (None, None),
            // A foreground run's standalone timeout; the background re-arm
            // defaults to the background semantics (ADR-4: a timed-out
            // foreground run converts and re-arms the background timeout).
            EffectiveTimeout::Foreground(secs) => {
                (Some(secs), Some(DEFAULT_BACKGROUND_TIMEOUT_SECS))
            }
            EffectiveTimeout::Background(secs) => (Some(secs), Some(secs)),
        };
        // The model-supplied `cwd` wins; otherwise the session's workDir
        // (TS parity: `effectiveCwd = args.cwd ?? this.cwd`, bash.ts:322).
        let cwd = input.cwd.clone().or_else(|| self.default_cwd.clone());

        let plan = ExecutionPlan {
            accesses: vec![ToolAccess {
                // A shell command can touch anything — treat it as conflicting
                // with every other tool call in the batch.
                kind: AccessKind::All,
                path: None,
                operation: AccessOperation::ReadWrite,
            }],
            display: ToolInputDisplay::Command {
                command: input.command.clone(),
            },
            approval_rule: "Bash".to_string(),
            stop_batch_after_this: false,
        };

        let BashInput {
            command,
            // Consumed above (merged with the session default).
            cwd: _,
            timeout,
            run_in_background,
            disable_timeout: _,
            // US14.4: a single explicit exemption, once per task — the
            // flag travels to the registration and nothing else. `false`
            // (the default) keeps the task under the ceiling.
            exempt_lifetime: lifetime_exempt,
            description,
        } = input;

        if run_in_background.unwrap_or(false) {
            // Values above the background cap are rejected (TS parity: the
            // schema's superRefine refuses rather than clamps).
            if timeout.is_some_and(|timeout| timeout > MAX_BACKGROUND_TIMEOUT_SECS) {
                return Ok(tool_execution_error(format!(
                    "timeout must be ≤ {MAX_BACKGROUND_TIMEOUT_SECS}s (background)"
                )));
            }
            // Background runs are detached from the turn: they keep their own
            // deadline (unless disabled) but must NOT observe the turn's
            // cancel token.
            let description = description
                .filter(|description| !description.trim().is_empty())
                .unwrap_or_else(|| default_task_description(&command));
            let tasks = Arc::clone(&self.tasks);
            let session_id = self.session_id.clone();
            let process = Arc::clone(&self.process);
            let agent_id = self.agent_id.clone();
            return Ok(ToolExecution::Runnable {
                plan,
                execute: Box::new(move |_ctx| {
                    Box::pin(start_background_command(
                        process,
                        command,
                        cwd,
                        foreground_timeout_secs,
                        lifetime_exempt.unwrap_or(false),
                        description,
                        tasks,
                        session_id,
                        agent_id,
                    ))
                }),
            });
        }

        let process = Arc::clone(&self.process);
        let tasks = Arc::clone(&self.tasks);
        let session_id = self.session_id.clone();
        let agent_id = self.agent_id.clone();
        Ok(ToolExecution::Runnable {
            plan,
            execute: Box::new(move |ctx| {
                // B7 D-3: the foreground run is a registered (detachable)
                // foreground task addressed by the model-side tool call id.
                Box::pin(run_foreground_tracked(
                    process,
                    command,
                    cwd,
                    foreground_timeout_secs,
                    background_rearm_secs,
                    lifetime_exempt.unwrap_or(false),
                    ctx.cancel,
                    ctx.progress.clone(),
                    tasks,
                    session_id,
                    agent_id,
                    ctx.tool_call_id,
                ))
            }),
        })
    }
}

/// The timeout semantics for a Bash request, keyed by run mode.
///
/// `Disabled` → no `tokio::time::timeout` is applied. `Foreground` → the
/// resolved foreground timeout (default [`DEFAULT_TIMEOUT_SECS`], clamped to
/// [`MAX_TIMEOUT_SECS`]). `Background` → the resolved background timeout
/// (default [`DEFAULT_BACKGROUND_TIMEOUT_SECS`], capped at
/// [`MAX_BACKGROUND_TIMEOUT_SECS`] — over-cap values are rejected earlier in
/// `resolve_execution`, so this arm only receives in-range values).
#[derive(Debug, PartialEq, Eq)]
enum EffectiveTimeout {
    Disabled,
    Foreground(u64),
    Background(u64),
}

fn effective_timeout_secs(input: &BashInput) -> EffectiveTimeout {
    if input.disable_timeout.unwrap_or(false) {
        EffectiveTimeout::Disabled
    } else if input.run_in_background.unwrap_or(false) {
        EffectiveTimeout::Background(input.timeout.unwrap_or(DEFAULT_BACKGROUND_TIMEOUT_SECS))
    } else {
        EffectiveTimeout::Foreground(resolve_timeout_secs(input.timeout))
    }
}

/// Resolve the foreground timeout (seconds) from the tool input, applying the
/// default when absent and clamping to [`MAX_TIMEOUT_SECS`].
fn resolve_timeout_secs(timeout: Option<u64>) -> u64 {
    timeout
        .unwrap_or(DEFAULT_TIMEOUT_SECS)
        .clamp(1, MAX_TIMEOUT_SECS)
}

/// The deadline the background continuation re-arms after a foreground timeout
/// converts the run: the background semantics (`None` = `disable_timeout`, no
/// second deadline — the run may outlive the turn indefinitely).
fn rearmed_deadline(background_rearm_secs: Option<u64>) -> Option<Duration> {
    background_rearm_secs.map(Duration::from_secs)
}

/// Default background task description: a short preview of the command.
fn default_task_description(command: &str) -> String {
    let preview: String = command.chars().take(60).collect();
    if command.chars().count() > 60 {
        format!("Bash: {preview}…")
    } else {
        format!("Bash: {preview}")
    }
}

/// Environment overrides applied to every Bash command so tools like `git`
/// and `node` don't open a pager, hang on a prompt, or colour the captured
/// stream. Mirrors the TS engine: `NO_COLOR=1`, `TERM=dumb`,
/// `GIT_TERMINAL_PROMPT=0` (honouring an explicit ambient value), and the
/// parent's `SHELL` preserved.
fn noninteractive_env() -> HashMap<String, String> {
    let git_prompt = std::env::var("GIT_TERMINAL_PROMPT").unwrap_or_else(|_| "0".to_string());
    let mut env = HashMap::new();
    env.insert("NO_COLOR".to_string(), "1".to_string());
    env.insert("TERM".to_string(), "dumb".to_string());
    env.insert("GIT_TERMINAL_PROMPT".to_string(), git_prompt);
    // The parent env is inherited and merged underneath these overrides;
    // preserving SHELL explicitly keeps it stable even if a future caller
    // overrides it.
    if let Ok(shell) = std::env::var("SHELL") {
        env.insert("SHELL".to_string(), shell);
    }
    env
}

/// The result of racing a command against its deadline / cancellation.
enum CommandOutcome {
    /// The command finished (possibly with an error while reading output).
    Finished(anyhow::Result<(i32, String, String)>),
    /// The configured timeout elapsed; the child must be killed.
    TimedOut { timeout_secs: u64 },
    /// The turn's cancel token fired; the child must be killed.
    Cancelled,
}

/// Read one output stream to EOF, forwarding each incrementally-decoded
/// chunk through the progress handle while accumulating the raw bytes for
/// the final output.
///
/// UTF-8 characters can straddle read-chunk boundaries; the forwarding keeps
/// a tail of not-yet-valid bytes back until the sequence completes (or the
/// stream ends), so no chunk is ever emitted with a split character. The
/// final conversion errors on genuinely invalid UTF-8 — the same semantics
/// the buffered `read_to_string` path had.
async fn read_and_forward<R: tokio::io::AsyncRead + Unpin>(
    stream: Option<R>,
    kind: ToolUpdateKind,
    progress: Option<ToolProgressHandle>,
) -> anyhow::Result<String> {
    use tokio::io::AsyncReadExt;

    let mut all: Vec<u8> = Vec::new();
    let Some(mut stream) = stream else {
        return Ok(String::new());
    };
    let mut buf = [0u8; 8192];
    let mut forwarded = 0usize;
    loop {
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        all.extend_from_slice(&buf[..n]);
        // Forward every byte that is provably part of a complete UTF-8
        // sequence; an incomplete trailing sequence stays pending until the
        // next chunk arrives.
        let unforwarded = &all[forwarded..];
        let valid_up_to = match std::str::from_utf8(unforwarded) {
            Ok(_) => unforwarded.len(),
            Err(error) => error.valid_up_to(),
        };
        if valid_up_to > 0 {
            let chunk = std::str::from_utf8(&unforwarded[..valid_up_to])
                .expect("prefix validated by from_utf8");
            if let Some(progress) = progress.as_ref() {
                progress.send(ToolUpdate {
                    kind,
                    text: Some(chunk.to_string()),
                    percent: None,
                    custom_kind: None,
                    custom_data: None,
                });
            }
            forwarded += valid_up_to;
        }
    }
    Ok(String::from_utf8(all)?)
}

/// Execute `command` through the shell, collect output, and apply the timeout
/// and optional cancellation.
///
/// When `progress` is given, each stdout/stderr chunk is streamed live as a
/// `ToolUpdate` (mirroring the TS engine's foreground `onProcessOutput`:
/// emitter-side, no throttling); the full output is still accumulated for the
/// returned triple.
///
/// Returns `Ok((exit_code, stdout, stderr, timed_out))`; `timed_out` is
/// `true` when the timeout fired and the child was killed. When `cancel` is
/// `Some`, a fired cancel token kills the child and returns `exit_code: -1`
/// with a "cancelled" message.
async fn run_shell_command(
    process: &dyn DynOsProcess,
    command: &str,
    opts: &ExecOptions,
    timeout: Option<Duration>,
    cancel: Option<&CancelToken>,
    progress: Option<&ToolProgressHandle>,
    fingerprint_sink: Option<(&str, &TaskManager)>,
) -> anyhow::Result<(i32, String, String, bool)> {
    #[cfg(unix)]
    let (shell, shell_flag) = ("sh", "-c");
    #[cfg(windows)]
    let (shell, shell_flag) = ("cmd", "/C");

    let mut child = process.exec(shell, &[shell_flag, command], opts).await?;

    // Attach the process identity fingerprint to the registered task row
    // while the child is fresh (ADR-012 a) — the background start path's
    // only child-visible point. The current sole call site passes `Some`;
    // `None` is the reserved spelling for future internal callers that
    // have no task row to address.
    if let Some((task_id, tasks)) = fingerprint_sink {
        capture_fingerprint(child.as_ref(), tasks, task_id);
    }

    // Close stdin immediately so interactive commands (`cat`, `read`, python
    // `input()`) see EOF instead of hanging on the never-written pipe. This
    // is the `Stdio::null()`-equivalent within the OsProcess abstraction
    // (which always pipes): taking (and thereby dropping) the handle closes
    // the pipe.
    child.take_stdin();

    // Take the output pipes as owned handles so stdout and stderr are read
    // concurrently. Reading them sequentially risks a deadlock when one pipe
    // fills its OS buffer while the other is still being drained.
    let stdout_handle = child.take_stdout();
    let stderr_handle = child.take_stderr();
    let stdout_progress = progress.cloned();
    let stderr_progress = progress.cloned();

    let run = async {
        let (stdout, stderr) = tokio::try_join!(
            read_and_forward(stdout_handle, ToolUpdateKind::Stdout, stdout_progress),
            read_and_forward(stderr_handle, ToolUpdateKind::Stderr, stderr_progress),
        )?;
        let exit_code = child.wait().await?;
        Ok::<_, anyhow::Error>((exit_code, stdout, stderr))
    };

    // Race the command (bounded by `timeout`) against cancellation. The
    // losing branch's future is dropped, releasing its borrow of `child`, so
    // the child can be killed afterwards.
    let outcome = match cancel {
        Some(cancel) => tokio::select! {
            _ = cancel.cancelled() => CommandOutcome::Cancelled,
            outcome = with_timeout(run, timeout) => outcome,
        },
        None => with_timeout(run, timeout).await,
    };

    match outcome {
        CommandOutcome::Finished(Ok((exit_code, stdout, stderr))) => {
            Ok((exit_code, stdout, stderr, false))
        }
        CommandOutcome::Finished(Err(error)) => Err(error),
        CommandOutcome::TimedOut { timeout_secs } => {
            // `run` was dropped, releasing its borrow of `child`, so the
            // process can be killed now (SIGTERM, then SIGKILL after 5s on
            // Unix, via `ChildProcess::kill`).
            child.kill().await?;
            Ok((
                -1,
                String::new(),
                format!("command timed out after {timeout_secs}s"),
                true,
            ))
        }
        CommandOutcome::Cancelled => {
            child.kill().await?;
            Ok((-1, String::new(), "command cancelled".to_string(), false))
        }
    }
}

/// Run `run`, applying `timeout` when given. No timeout when `None`.
async fn with_timeout(
    run: impl Future<Output = anyhow::Result<(i32, String, String)>>,
    timeout: Option<Duration>,
) -> CommandOutcome {
    match timeout {
        Some(timeout) => match tokio::time::timeout(timeout, run).await {
            Ok(result) => CommandOutcome::Finished(result),
            Err(_elapsed) => CommandOutcome::TimedOut {
                timeout_secs: timeout.as_secs(),
            },
        },
        None => CommandOutcome::Finished(run.await),
    }
}

/// Combine the separately captured stdout/stderr strings into the text the
/// model sees.
///
/// TS streams both pipes interleaved into one builder (bash.ts:345-355); Rust
/// captures two `String`s. Design decision (ADR-013): stdout first, then
/// stderr, with a newline boundary between them when both are non-empty (no
/// extra blank line when stdout already ends with a newline).
fn combine_captured_output(stdout: &str, stderr: &str) -> String {
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => String::new(),
        (true, false) => stderr.to_string(),
        (false, true) => stdout.to_string(),
        (false, false) if stdout.ends_with('\n') => format!("{stdout}{stderr}"),
        (false, false) => format!("{stdout}\n{stderr}"),
    }
}

/// Append a status line to the captured output the way the TS
/// `ToolResultBuilder` does (result-builder.ts:100-152): an empty body
/// yields the message alone; a body ending in a newline gets the message
/// directly; otherwise a newline separates them.
fn append_status_line(body: &str, message: &str) -> String {
    if body.is_empty() {
        message.to_string()
    } else if body.ends_with('\n') {
        format!("{body}{message}")
    } else {
        format!("{body}\n{message}")
    }
}

/// Format a settled foreground result for the model as plain text — the TS
/// `foregroundCompletionResult` semantics (bash.ts:454-484 +
/// `ToolResultBuilder`, result-builder.ts:100-152): the captured stdout/stderr
/// text plus a status line, never a JSON envelope (ADR-013; audit P2-2 —
/// the record shape of `tool.result` is unchanged, only how its text part is
/// produced).
///
/// - exit 0 → the captured text; `"Command executed successfully."` when
///   nothing was captured.
/// - any other exit → the captured text (or the `"Process exited with
///   code N"` placeholder when empty) plus `"Command failed with exit
///   code: N."`.
///
/// A foreground timeout never reaches this formatter — the run converts to a
/// background task instead (ADR-4); the re-armed deadline settles through the
/// task lifecycle, not the tool result.
fn format_bash_result(exit_code: i32, stdout: String, stderr: String) -> ToolResult {
    let captured = combine_captured_output(&stdout, &stderr);
    let (output, is_error) = if exit_code == 0 {
        if captured.is_empty() {
            ("Command executed successfully.".to_string(), false)
        } else {
            (captured, false)
        }
    } else {
        // TS bash.ts:478: the placeholder is written only when the builder
        // is still empty.
        let body = if captured.is_empty() {
            format!("Process exited with code {exit_code}")
        } else {
            captured
        };
        (
            append_status_line(
                &body,
                &format!("Command failed with exit code: {exit_code}."),
            ),
            true,
        )
    };
    ToolResult {
        output: ToolOutput::text(output),
        is_error,
        note: None,
        stop_turn: false,
        truncated: false,
        injection: None,
    }
}

/// The deadline pinned when no timeout applies (`disable_timeout`): a
/// practically-unreachable instant, so the select arm simply never fires.
const NO_DEADLINE: Duration = Duration::from_secs(60 * 60 * 24 * 365 * 100);

/// Run a foreground command as a registered, detachable foreground task
/// (B7 D-3; `shell_command::run_tracked` is the loop template).
///
/// `foreground_timeout_secs` is `None` when `disable_timeout` was set,
/// otherwise the resolved foreground timeout (> 0 by construction). The turn's
/// `cancel` token kills the command when the turn is cancelled.
/// `background_rearm_secs` is the timeout re-armed after a timed-out foreground
/// run converts to background (`None` = `disable_timeout`, no second deadline).
///
/// - **Natural exit** → the task row settles `Completed{exit_code}` with the
///   accumulated output appended, and the ToolResult text is byte-identical
///   to the pre-B7 untracked payload (the settled-path tests pin it).
/// - **`task.detach`** (the release resolves `Detached`) → the run returns
///   the moved-to-background result immediately, and the STILL-RUNNING child
///   hands over to a spawned continuation that drains the remaining output
///   into the task row and settles it via
///   [`TaskManager::settle_background_run`].
/// - **Turn cancel** → the child is killed and the task settles `Stopped`.
/// - **Foreground timeout** → the child is NOT killed; the run is handed over
///   to the same background continuation as a `task.detach` (ADR-4), and the
///   task row converts to a background task re-armed with
///   `background_rearm_secs` (the continuation enforces that second deadline —
///   kill + settle `Failed` with the timeout reason when it elapses).
/// - **Spawn failure** → the task settles `Failed` and the result is the
///   execution-error spelling.
///
/// The foreground-timeout handover and a concurrent `task.detach` race on the
/// same child; an [`AtomicBool`] first-wins guard lets exactly one path hand
/// the child over (the loser falls through to its non-conversion handling).
///
/// Every path settles the registered row — a foreground run never leaves a
/// dangling `Running` task behind.
// The ten parameters are the plan's B7 R2 interface sketch (the tool's
// execution context plus the tracking faces) — same allowance the tool
// batch executor carries for its context fan-in.
#[allow(clippy::too_many_arguments)]
async fn run_foreground_tracked(
    process: Arc<dyn DynOsProcess>,
    command: String,
    cwd: Option<PathBuf>,
    foreground_timeout_secs: Option<u64>,
    background_rearm_secs: Option<u64>,
    lifetime_exempt: bool,
    cancel: CancelToken,
    progress: ToolProgressHandle,
    tasks: Arc<TaskManager>,
    session_id: String,
    agent_id: String,
    tool_call_id: String,
) -> ToolResult {
    let description = default_task_description(&command);
    let reg = tasks.register_foreground(
        TaskKind::Process {
            command: Some(command.clone()),
            dispatcher: Some(agent_id),
        },
        description.clone(),
        session_id,
        Some(tool_call_id),
    );
    let task_id = reg.task_id.clone();
    // US14.4: the exemption is stamped right at registration, so the very
    // first archive write (and any detach/timeout conversion thereafter)
    // already carries it — a sweeper tick between the registration and a
    // later mutation must never see a non-exempt row.
    if lifetime_exempt {
        tasks.set_lifetime_exempt(&task_id);
    }

    let opts = ExecOptions {
        cwd,
        env: noninteractive_env(),
        timeout_ms: foreground_timeout_secs.map(|secs| secs * 1000),
    };
    #[cfg(unix)]
    let (shell, shell_flag) = ("sh", "-c");
    #[cfg(windows)]
    let (shell, shell_flag) = ("cmd", "/C");
    let mut child = match process.exec(shell, &[shell_flag, &command], &opts).await {
        Ok(child) => child,
        Err(error) => {
            tasks.fail(&task_id, format!("Bash failed to execute: {error}"));
            return ToolResult::error(format!("Bash failed to execute: {error}"));
        }
    };
    child.take_stdin();

    // The identity fingerprint lands on the row while the child is fresh —
    // a later foreground→background conversion (user detach or the timeout,
    // US14.1) then carries it into the archive untouched.
    capture_fingerprint(child.as_ref(), &tasks, &task_id);

    let (chunk_tx, mut chunk_rx) = mpsc::channel::<ShellChunk>(64);
    let stdout_pump = tokio::spawn(pump_stream(
        child.take_stdout(),
        ToolUpdateKind::Stdout,
        chunk_tx.clone(),
    ));
    let stderr_pump = tokio::spawn(pump_stream(
        child.take_stderr(),
        ToolUpdateKind::Stderr,
        chunk_tx,
    ));

    let mut stdout = String::new();
    let mut stderr = String::new();
    // First-wins guard (ADR-4 约束②): the foreground-timeout arm and the
    // `task.detach` release arm race on the same still-running child — exactly
    // one may hand it over. The compare_exchange must succeed BEFORE
    // `detach_foreground` fires the release that wakes the other arm.
    let backgrounded = std::sync::atomic::AtomicBool::new(false);
    let deadline = tokio::time::Instant::now()
        + foreground_timeout_secs.map_or(NO_DEADLINE, Duration::from_secs);
    let timeout_sleep = tokio::time::sleep_until(deadline);
    tokio::pin!(timeout_sleep);

    loop {
        tokio::select! {
            // The foreground timeout elapsed: NOT killed — the run converts to
            // a background task (ADR-4), handed over like a `task.detach` and
            // re-armed with the background timeout. The first-wins guard is set
            // BEFORE `detach_foreground` fires the release, so a concurrent
            // user detach can't double-hand the child over.
            _ = &mut timeout_sleep => {
                if backgrounded
                    .compare_exchange(
                        false,
                        true,
                        std::sync::atomic::Ordering::SeqCst,
                        std::sync::atomic::Ordering::SeqCst,
                    )
                    .is_err()
                {
                    // The `task.detach` release arm already handed the child to
                    // the background continuation — answer without touching the
                    // (now continuation-owned) child.
                    return format_bash_result(-1, String::new(), "command cancelled".to_string());
                }
                tasks.detach_foreground(&task_id);
                return hand_over_to_background(
                    child,
                    chunk_rx,
                    tasks,
                    task_id,
                    description,
                    stdout,
                    stderr,
                    rearmed_deadline(background_rearm_secs),
                    Some("exceeded the foreground limit — automatically moved to background"),
                )
                .await;
            }
            // The turn was cancelled: kill and settle as Stopped.
            _ = cancel.cancelled() => {
                let _ = child.kill().await;
                stdout_pump.abort();
                stderr_pump.abort();
                tasks.stop(&task_id);
                return format_bash_result(
                    -1,
                    String::new(),
                    "command cancelled".to_string(),
                );
            }
            // The foreground release fired — `task.detach` (Detached) or an
            // external terminal settle of the row (Terminal).
            reason = reg.released() => {
                if reason == ForegroundReleaseReason::Detached
                    && tasks.get(&task_id).is_some_and(|task| task.detached)
                {
                    // First-wins guard (ADR-4 约束②): the user's `task.detach`
                    // release races the foreground-timeout conversion — exactly
                    // one may hand the child over. Guard failure means the
                    // timeout arm already converted; the child belongs to the
                    // continuation, so just answer (never kill it here).
                    if backgrounded
                        .compare_exchange(
                            false,
                            true,
                            std::sync::atomic::Ordering::SeqCst,
                            std::sync::atomic::Ordering::SeqCst,
                        )
                        .is_err()
                    {
                        return format_bash_result(
                            -1,
                            String::new(),
                            "command cancelled".to_string(),
                        );
                    }
                    return hand_over_to_background(
                        child,
                        chunk_rx,
                        tasks,
                        task_id,
                        description,
                        stdout,
                        stderr,
                        rearmed_deadline(background_rearm_secs),
                        None,
                    )
                    .await;
                }
                // A Terminal release while the run is still looping: an
                // external stop (task.cancel, session close, the timeout
                // sweeper) already settled the row — kill the child and
                // answer with the cancelled spelling. The row keeps its
                // first-terminal status (our stop below is a no-op, kept
                // as a belt-and-braces settle).
                let _ = child.kill().await;
                stdout_pump.abort();
                stderr_pump.abort();
                tasks.stop(&task_id);
                return format_bash_result(
                    -1,
                    String::new(),
                    "command cancelled".to_string(),
                );
            }
            chunk = chunk_rx.recv() => {
                let Some(ShellChunk { kind, text }) = chunk else {
                    // Both pumps hit EOF: the command finished — wait for
                    // the exit code, append the accumulated output to the
                    // row, and settle Completed. (Select coin-flip note:
                    // when EOF and a fired detach release are both ready,
                    // either arm may win; both converge on a correct
                    // terminal state — the detach arm hands the
                    // already-exited child to the continuation, which
                    // settles with the same exit code.)
                    let exit_code = child.wait().await.unwrap_or(-1);
                    tasks.append_output(&task_id, &stdout);
                    tasks.append_output(&task_id, &stderr);
                    tasks.complete(&task_id, exit_code);
                    return format_bash_result(exit_code, stdout, stderr);
                };
                match kind {
                    ToolUpdateKind::Stdout => stdout.push_str(&text),
                    ToolUpdateKind::Stderr => stderr.push_str(&text),
                    _ => {}
                }
                progress.send(ToolUpdate {
                    kind,
                    text: Some(text),
                    percent: None,
                    custom_kind: None,
                    custom_data: None,
                });
            }
        }
    }
}

/// Hand the still-running child to a background continuation: fold the
/// foreground window's output into the task row, attach the D-63 kill signal,
/// spawn the draining loop (settled by [`TaskManager::settle_background_run`]),
/// and return the moved-to-background result the caller answers the model with.
///
/// The continuation also enforces the re-armed deadline (ADR-4 约束①): when
/// `rearm` is `Some`, an extra select arm kills the child and settles the task
/// `Failed` with the timeout reason once the second deadline elapses. A `None`
/// rearm (`disable_timeout`) armes the [`NO_DEADLINE`] sleep instead, so the
/// run is never killed by timeout.
///
/// `reason` augments the returned message with an explanatory line — `None`
/// for a user-initiated `task.detach`, `Some(..)` for the foreground-timeout
/// conversion. The caller owns the first-wins guard; exactly one handover.
#[allow(clippy::too_many_arguments)]
async fn hand_over_to_background(
    mut child: Box<dyn DynChildProcess>,
    mut chunk_rx: mpsc::Receiver<ShellChunk>,
    tasks: Arc<TaskManager>,
    task_id: String,
    description: String,
    foreground_stdout: String,
    foreground_stderr: String,
    rearm: Option<Duration>,
    reason: Option<&str>,
) -> ToolResult {
    // The foreground window's output rides the task row (the panel and
    // TaskOutput read it there), then the still-running child hands over to
    // the background continuation (the `!` runner's finish_detached shape,
    // plus the kill signal the detached run needs so a TaskStop really kills
    // the OS process — D-63).
    tasks.append_output(&task_id, &foreground_stdout);
    tasks.append_output(&task_id, &foreground_stderr);
    let signal = CancelToken::new();
    tasks.attach_signal(&task_id, signal.clone());
    let tasks_for_run = Arc::clone(&tasks);
    let continuation_task_id = task_id.clone();
    // Re-armed deadline: `None` (= disable_timeout) arms the unreachable
    // NO_DEADLINE sleep so the arm simply never fires — never a real second
    // deadline when `disable_timeout` exempts the run.
    let rearm = rearm.unwrap_or(NO_DEADLINE);
    // Process-liveness heartbeat (ADR-004 batch a gap, the session-f1e24e04
    // incident): a backgrounded command IS alive — a long silent command (a
    // full cargo test emitting no output for minutes) must keep its
    // heartbeat advancing, so neither `is_stale` nor a waiting turn's
    // watchdog feed sees a dead task. Same rule `drive_with_process_heartbeat`
    // (task.rs) applies to the `spawn_with_signal` path; the foreground-timeout
    // hand-over path forgot it. The ticker beats at a quarter of the
    // heartbeat window (four beats per window — a single dropped beat cannot
    // read as stale), with `MissedTickBehavior::Delay` so a busy runtime
    // coalesces missed beats instead of firing a burst. The first tick fires
    // immediately (tokio's `interval` semantics) — harmless: registration
    // already seeded the heartbeat, and a beat on a live process is always
    // truthful. No configured heartbeat window → liveness detection is
    // disabled → no ticker.
    let mut heartbeat_ticker = tasks.heartbeat_interval().map(|interval| {
        let mut ticker = tokio::time::interval(interval / 4);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticker
    });
    tokio::spawn(async move {
        let rearm_deadline = tokio::time::Instant::now() + rearm;
        let rearm_sleep = tokio::time::sleep_until(rearm_deadline);
        tokio::pin!(rearm_sleep);
        // Belt-and-braces for the fire->attach window: a stop that landed
        // between the release fire and the signal attach flips the row
        // terminal without firing the (not-yet-attached) signal — kill the
        // child now instead of letting it run under an already-terminal row
        // (the row keeps its first-terminal status; no settle needed).
        if tasks_for_run
            .get(&continuation_task_id)
            .is_some_and(|task| task.status.is_terminal())
        {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return;
        }
        loop {
            tokio::select! {
                // The process-liveness heartbeat: a pending, silent
                // process is still alive — keep the heartbeat advancing so
                // `is_stale` and the waiting turns' liveness feed stay fed.
                _ = async { match heartbeat_ticker.as_mut() {
                    Some(ticker) => ticker.tick().await,
                    // Liveness detection disabled: this arm never fires.
                    None => std::future::pending().await,
                }} => {
                    tasks_for_run.heartbeat(&continuation_task_id);
                }
                _ = &mut rearm_sleep => {
                    // The re-armed deadline elapsed: kill the child and settle
                    // the task Failed with the timeout reason (the same wording
                    // the foreground-timeout settle used).
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    tasks_for_run.fail(
                        &continuation_task_id,
                        format!("command timed out after {}s", rearm.as_secs()),
                    );
                    return;
                }
                _ = signal.cancelled() => {
                    // The stop's signal fired first — kill the child and
                    // settle the honest Stopped (the cancelled-arm rule of
                    // settle_background_run).
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    tasks_for_run
                        .settle_background_run(
                            &continuation_task_id,
                            Some(signal),
                            Ok((0, String::new(), String::new())),
                        )
                        .await;
                    return;
                }
                chunk = chunk_rx.recv() => {
                    let Some(ShellChunk { text, .. }) = chunk else {
                        // Both pumps hit EOF: the command finished — settle
                        // with its exit code.
                        let exit_code = child.wait().await.unwrap_or(-1);
                        tasks_for_run
                            .settle_background_run(
                                &continuation_task_id,
                                Some(signal),
                                Ok((exit_code, String::new(), String::new())),
                            )
                            .await;
                        return;
                    };
                    tasks_for_run.append_output(&continuation_task_id, &text);
                }
            }
        }
    });
    ToolResult {
        output: ToolOutput::text(moved_to_background_output(
            &task_id,
            &description,
            reason,
            &foreground_stdout,
            &foreground_stderr,
        )),
        is_error: false,
        note: None,
        stop_turn: false,
        truncated: false,
        injection: None,
    }
}

/// Capture the spawned child's process identity fingerprint (ADR-012 a)
/// onto a registered task row.
///
/// Called on BOTH spawn paths — the direct background start
/// ([`start_background_command`]) and the foreground runner
/// ([`run_foreground_tracked`]) — so a foreground→background conversion
/// (user `task.detach` or the foreground timeout, US14.1) carries the
/// identity into the archive without the conversion paths needing to know
/// about it.
///
/// Recording semantics (ADR-012 a/b):
/// - no `pgid` (non-Unix or a process double without a group) → record
///   nothing;
/// - probe alive → record `start_ticks` (0 when the platform cannot read
///   it — the judgment side degrades to "cannot determine");
/// - probe not alive (the child died between spawn and probe) or probe
///   error → record nothing — no archive may point at a fingerprint no
///   live process backs. The load-time reconcile / startup sweep own a
///   fingerprint-less row's verdict.
///
/// This is a best-effort bookkeeping write: a failed capture is logged and
/// otherwise ignored — the command run itself must never fail because
/// identity capture did.
fn capture_fingerprint(child: &dyn DynChildProcess, tasks: &TaskManager, task_id: &str) {
    let pid = child.pid() as i32;
    let Some(pgid) = child.pgid() else {
        tracing::debug!(task_id, pid, "no process group on child — no fingerprint");
        return;
    };
    // Probe semantics (ADR-012 b): a probe that CAN determine aliveness and
    // says not-alive means the child died between spawn and probe (or the
    // caller raced a reaped pid) — record nothing, so no archive ever points
    // at a fingerprint no live process backs. A probe that cannot read start
    // ticks on a LIVE process degrades to `start_ticks: 0` — the judgment
    // side reads 0 as "cannot determine". (Process doubles without a real OS
    // process never get this far — their trait-default `pgid()` gates them
    // out above.)
    let start_ticks = match probe_process(pid) {
        Ok(probe) if probe.alive => probe.start_ticks.unwrap_or(0),
        Ok(_) => {
            tracing::debug!(
                task_id,
                pid,
                "process not alive at capture — no fingerprint"
            );
            return;
        }
        Err(error) => {
            tracing::debug!(task_id, pid, %error, "process probe failed — no fingerprint");
            return;
        }
    };
    tasks.set_fingerprint(
        task_id,
        ProcessFingerprint {
            pid,
            pgid,
            start_ticks,
        },
    );
}

/// Start a background command through
/// [`TaskManager::spawn_background_with_signal`] and return the task id
/// immediately (no waiting for completion).
///
/// The detached run receives its own [`CancelToken`] (D-63): a
/// `TaskStop`/session-close stop now actually kills the OS process — the
/// pre-B1 wiring left stopped background commands running as orphans — and
/// the kill escalates inside `mirri-os` (SIGTERM → 5s → SIGKILL on the
/// process group). The registration consumes a `[task] maxRunningTasks`
/// slot (TS `assertCanRegister` counts every detached entry, bash
/// included): at capacity the command does not start and the producer
/// surfaces the capacity message to the model.
#[allow(clippy::too_many_arguments)]
pub async fn start_background_command(
    process: Arc<dyn DynOsProcess>,
    command: String,
    cwd: Option<PathBuf>,
    timeout_secs: Option<u64>,
    lifetime_exempt: bool,
    description: String,
    tasks: Arc<TaskManager>,
    session_id: String,
    dispatcher: String,
) -> ToolResult {
    let command_for_info = command.clone();
    // The manager clone rides the runner closure (the spawn call itself
    // borrows the Arc receiver).
    let tasks_for_run = Arc::clone(&tasks);
    let task_id = match tasks.spawn_background_with_signal(
        TaskKind::Process {
            command: Some(command_for_info),
            dispatcher: Some(dispatcher),
        },
        description.clone(),
        session_id,
        // The factory receives the minted task id: the fingerprint capture
        // inside the runner addresses the row this spawn just registered.
        |cancel, task_id| {
            Box::pin(async move {
                run_bash_background(
                    process,
                    &command,
                    cwd,
                    timeout_secs,
                    cancel,
                    &tasks_for_run,
                    &task_id,
                )
                .await
            })
        },
    ) {
        Ok(task_id) => task_id,
        Err(message) => {
            return ToolResult {
                output: ToolOutput::text(message),
                is_error: true,
                note: None,
                stop_turn: false,
                truncated: false,
                injection: None,
            };
        }
    };
    // US14.4: stamp the exemption on the just-registered row. The stamp
    // rides AFTER the spawn deliberately — the exempt registration must
    // keep the signal wiring, capacity accounting, and the running child
    // (a bare register would start nothing and stoppable-by-signal
    // nothing). The ordering leaves only a milliseconds-wide window
    // where the row exists but is not yet exempt — observable only
    // under an abnormally small ceiling configuration, and even then
    // only because a sweep pass's directory enumeration can observe the
    // fresh archive progressively.
    if lifetime_exempt {
        tasks.set_lifetime_exempt(&task_id);
    }
    ToolResult {
        output: ToolOutput::text(background_started_output(&task_id, &description)),
        is_error: false,
        note: None,
        stop_turn: false,
        truncated: false,
        injection: None,
    }
}

/// Run a command to completion for a background task.
///
/// Collects stdout/stderr for the task's accumulated output and applies the
/// same optional timeout (killing the process group on timeout). The turn's
/// cancel token deliberately does not propagate — a background task outlives
/// the turn that started it — but the task's OWN cancel token does (D-63):
/// a fired token kills the process (the `run_shell_command` cancel race),
/// so `TaskStop` / session close reach the OS process.
///
/// The spawned child's identity fingerprint is attached to the task row
/// (ADR-012 a, US14.1) right after the spawn — before the (potentially
/// long) command runs — so the archive points at the real process from the
/// start. `task_id` is the pre-registered row id
/// ([`TaskManager::spawn_background_with_signal`] registers before invoking
/// this runner).
async fn run_bash_background(
    process: Arc<dyn DynOsProcess>,
    command: &str,
    cwd: Option<PathBuf>,
    timeout_secs: Option<u64>,
    cancel: CancelToken,
    tasks: &TaskManager,
    task_id: &str,
) -> anyhow::Result<(i32, String, String)> {
    let opts = ExecOptions {
        cwd,
        env: noninteractive_env(),
        timeout_ms: timeout_secs.map(|secs| secs * 1000),
    };
    let (exit_code, stdout, stderr, _timed_out) = run_shell_command(
        process.as_ref(),
        command,
        &opts,
        timeout_secs.map(Duration::from_secs),
        Some(&cancel),
        // Background tasks do not stream live output — their output surface
        // is the TaskManager's persisted log (mirroring the TS engine, which
        // only wires `onProcessOutput` for foreground runs).
        None,
        Some((task_id, tasks)),
    )
    .await?;
    Ok((exit_code, stdout, stderr))
}

/// The model-visible message returned when a background task is started.
fn background_started_output(task_id: &str, description: &str) -> String {
    format!(
        "task_id: {task_id}\n\
         description: {description}\n\
         status: running\n\
         next_step: The task runs in the background — you will be automatically \
         notified when it completes; do NOT wait, poll, or call TaskOutput on it, \
         continue with your current work.\n\
         next_step: Use TaskStop only if the task must be cancelled."
    )
}

/// The model-visible message returned when a foreground run is moved to the
/// background mid-flight (the TS `foreground_detached` result of
/// `BashTool.backgroundStartedResult`, B7 D-3 / ADR-4): task id, description,
/// the next-step guidance, the human shell hint, and — when the foreground
/// window captured anything — a `foreground_output:` section carrying it.
///
/// `reason` optionally leads with an explanatory line: `None` for a
/// user-initiated `task.detach`; `Some(..)` for the foreground-timeout
/// conversion so the model sees WHY the run moved to the background.
fn moved_to_background_output(
    task_id: &str,
    description: &str,
    reason: Option<&str>,
    stdout: &str,
    stderr: &str,
) -> String {
    let reason_head = reason
        .map(|reason| format!("reason: {reason}\n"))
        .unwrap_or_default();
    let metadata = format!(
        "{reason_head}\
         task_id: {task_id}\n\
         description: {description}\n\
         status: running\n\
         automatic_notification: true\n\
         next_step: The task now runs in the background. You will be automatically \
         notified when it completes — do NOT wait, poll, or call TaskOutput on it; \
         continue with your current work.\n\
         human_shell_hint: Tell the human to run /tasks to open the interactive \
         background-task panel."
    );
    let foreground_output = format!("{stdout}{stderr}");
    if foreground_output.is_empty() {
        metadata
    } else {
        format!("{metadata}\n\nforeground_output:\n{foreground_output}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::task::{TaskStatus, WaitResult};
    use crate::tool::ExecutableToolContext;
    use std::sync::Mutex as StdMutex;

    /// The engine-default local process provider, as the registered tool
    /// receives it.
    fn local_process() -> Arc<dyn DynOsProcess> {
        Arc::new(mirri_os::LocalProcess::new())
    }

    /// A deterministic AsyncRead that yields the given byte chunks one read
    /// at a time — lets tests pin exact chunk boundaries (including a UTF-8
    /// sequence split across chunks).
    struct ScriptedRead {
        chunks: Vec<Vec<u8>>,
    }

    impl tokio::io::AsyncRead for ScriptedRead {
        fn poll_read(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            if self.chunks.is_empty() || self.chunks[0].is_empty() {
                return std::task::Poll::Ready(Ok(()));
            }
            // Wake immediately so a subsequent poll can consume the next
            // chunk without a real reactor.
            cx.waker().wake_by_ref();
            let chunk = self.chunks.remove(0);
            buf.put_slice(&chunk);
            std::task::Poll::Ready(Ok(()))
        }
    }

    fn record_progress() -> (
        crate::tool::ToolProgressHandle,
        Arc<StdMutex<Vec<crate::tool::ToolUpdate>>>,
    ) {
        let updates: Arc<StdMutex<Vec<crate::tool::ToolUpdate>>> =
            Arc::new(StdMutex::new(Vec::new()));
        let recorder = Arc::clone(&updates);
        let handle = crate::tool::ToolProgressHandle::with_sink(Arc::new(move |update| {
            recorder.lock().unwrap().push(update);
        }));
        (handle, updates)
    }

    #[tokio::test]
    async fn should_stream_stdout_and_stderr_updates_when_progress_handle_is_wired() {
        // Given a wired progress handle and a command writing to both streams
        let (progress, updates) = record_progress();

        // When the command runs
        let (result, _tasks) = run_tracked(
            local_process(),
            "printf 'out'; printf 'err' >&2",
            Some(30),
            CancelToken::new(),
            progress,
        )
        .await;

        // Then the updates carry both kinds and the final result stays whole
        let updates = updates.lock().unwrap();
        let stdout_text: String = updates
            .iter()
            .filter(|u| u.kind == ToolUpdateKind::Stdout)
            .filter_map(|u| u.text.clone())
            .collect();
        let stderr_text: String = updates
            .iter()
            .filter(|u| u.kind == ToolUpdateKind::Stderr)
            .filter_map(|u| u.text.clone())
            .collect();
        assert_eq!(stdout_text, "out");
        assert_eq!(stderr_text, "err");
        assert!(updates.iter().all(|u| u.percent.is_none()));

        let text = output_text(&result);
        // stdout first, then stderr, with a newline boundary between them.
        assert_eq!(text, "out\nerr");
    }

    #[tokio::test]
    async fn should_stream_every_chunk_when_output_exceeds_the_read_buffer() {
        // Given a wired progress handle and a command emitting 20000 bytes
        let (progress, updates) = record_progress();

        // When the command runs (20000 bytes > 3 x 8192 read chunks)
        let (result, _tasks) = run_tracked(
            local_process(),
            "head -c 20000 /dev/zero | tr '\\0' 'x'",
            Some(30),
            CancelToken::new(),
            progress,
        )
        .await;

        // Then the streamed chunks concatenate to exactly the final output
        let updates = updates.lock().unwrap();
        let streamed: String = updates.iter().filter_map(|u| u.text.clone()).collect();
        let text = output_text(&result);
        assert_eq!(text.len(), 20000);
        assert!(text.chars().all(|c| c == 'x'));
        assert_eq!(streamed, text);
        assert!(updates.len() > 1, "expected multiple streamed chunks");
    }

    #[tokio::test]
    async fn should_not_split_multibyte_characters_across_chunk_boundaries() {
        // Given a stdout stream whose chunks split a multi-byte character
        // ("世" = E4 B8 96) across the second and third chunk
        let (progress, updates) = record_progress();

        // When the reader forwards the chunks
        let output = read_and_forward(
            Some(ScriptedRead {
                chunks: vec![
                    b"abc".to_vec(),
                    vec![0xE4, 0xB8],
                    [vec![0x96], b"def".to_vec()].concat(),
                ],
            }),
            ToolUpdateKind::Stdout,
            Some(progress),
        )
        .await
        .expect("read");

        // Then the final output is whole and each streamed chunk is valid
        // UTF-8 with no replacement character. The middle chunk (an
        // incomplete "世") forwards nothing — its bytes ride along until the
        // sequence completes in the next chunk.
        assert_eq!(output, "abc\u{4e16}def");
        let updates = updates.lock().unwrap();
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0].text.as_deref(), Some("abc"));
        assert_eq!(updates[1].text.as_deref(), Some("\u{4e16}def"));
    }

    #[tokio::test]
    async fn should_error_after_forwarding_the_valid_prefix_when_a_stream_contains_an_invalid_byte()
    {
        // Given a stream whose second chunk carries an invalid UTF-8 byte
        // after a valid prefix
        let (progress, updates) = record_progress();

        // When the reader forwards the chunks
        let result = read_and_forward(
            Some(ScriptedRead {
                chunks: vec![b"abc".to_vec(), vec![0xFF]],
            }),
            ToolUpdateKind::Stdout,
            Some(progress),
        )
        .await;

        // Then the read fails overall (same error semantics as the old
        // read_to_string baseline) while the valid prefix was still
        // streamed as progress before the failure surfaced
        assert!(result.is_err());
        let updates = updates.lock().unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].text.as_deref(), Some("abc"));
    }

    #[tokio::test]
    async fn should_error_when_the_stream_ends_mid_utf8_sequence() {
        // Given a stream that ends with an incomplete UTF-8 sequence
        // ("世" = E4 B8 96, last byte missing)
        let (progress, updates) = record_progress();

        // When the reader forwards the chunks
        let result = read_and_forward(
            Some(ScriptedRead {
                chunks: vec![b"abc".to_vec(), vec![0xE4, 0xB8]],
            }),
            ToolUpdateKind::Stdout,
            Some(progress),
        )
        .await;

        // Then the read fails (the trailing incomplete sequence cannot be
        // decoded, matching the old read_to_string error semantics) while
        // the complete prefix was streamed first
        assert!(result.is_err());
        let updates = updates.lock().unwrap();
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].text.as_deref(), Some("abc"));
    }

    #[tokio::test]
    async fn should_keep_command_silent_when_no_progress_handle_is_wired() {
        // Given a default (no-sink) progress handle
        // When the command runs, the behavior matches the buffered baseline
        let (result, _tasks) = run_tracked(
            local_process(),
            "echo untouched",
            Some(30),
            CancelToken::new(),
            crate::tool::ToolProgressHandle::default(),
        )
        .await;

        // Then the result is unchanged by the unwired handle
        assert!(!result.is_error);
        assert_eq!(output_text(&result), "untouched\n");
    }

    async fn execute_tool(tool: &BashTool, input: serde_json::Value) -> ToolResult {
        let execution = tool.resolve_execution(input).expect("resolve");
        match execution {
            ToolExecution::Runnable { execute, .. } => {
                execute(ExecutableToolContext {
                    wait_registry: None,
                    turn_id: "test-turn".to_string(),
                    tool_call_id: "test-call".to_string(),
                    cancel: CancelToken::new(),
                    progress: crate::tool::ToolProgressHandle::default(),
                })
                .await
            }
            ToolExecution::Error(error) => ToolResult::error(error.message),
        }
    }

    fn output_text(result: &ToolResult) -> String {
        match &result.output {
            ToolOutput::Text(text) => text.clone(),
            ToolOutput::Parts(_) => panic!("expected text output"),
        }
    }

    fn extract_task_id(text: &str) -> String {
        text.lines()
            .find_map(|line| line.strip_prefix("task_id: "))
            .expect("output should contain a task_id line")
            .to_string()
    }

    fn test_tool() -> (Arc<TaskManager>, BashTool) {
        let tasks = Arc::new(TaskManager::new());
        let tool = BashTool::new(
            Arc::clone(&tasks),
            "agent-main".to_string(),
            "session-1".to_string(),
            local_process(),
            None,
        );
        (tasks, tool)
    }

    #[tokio::test]
    async fn should_capture_stdout_when_executing_echo_command() {
        let (result, _tasks) = run_tracked(
            local_process(),
            "echo hello",
            Some(30),
            CancelToken::new(),
            ToolProgressHandle::default(),
        )
        .await;
        assert!(!result.is_error);
        assert_eq!(output_text(&result), "hello\n");
    }

    #[tokio::test]
    async fn should_return_error_result_with_exit_code_when_executing_failing_command() {
        let (result, _tasks) = run_tracked(
            local_process(),
            "exit 3",
            Some(30),
            CancelToken::new(),
            ToolProgressHandle::default(),
        )
        .await;
        assert!(result.is_error);
        let text = output_text(&result);
        assert!(
            text.contains("Command failed with exit code: 3."),
            "the exit code rides the status line, got: {text}"
        );
    }

    #[tokio::test]
    async fn should_move_sleeping_command_to_background_when_executing_with_short_timeout() {
        // A foreground command that outlives its short timeout is NOT
        // killed — it converts to a background task (ADR-4).
        let (result, tasks) = run_tracked(
            local_process(),
            "sleep 30",
            Some(1),
            CancelToken::new(),
            ToolProgressHandle::default(),
        )
        .await;
        assert!(!result.is_error);
        let text = output_text(&result);
        assert!(
            text.contains("automatically moved to background"),
            "the reason line explains the conversion, got: {text}"
        );
        let task_id = extract_task_id(&text);
        // Cleanup: stop the still-running backgrounded task.
        tasks.stop(&task_id);
    }

    /// Given a foreground command converted to a background continuation
    /// (the foreground timeout path) whose process stays alive but emits
    /// NOTHING for far past the heartbeat window, when the continuation
    /// runs under a manager with heartbeat detection enabled, then the
    /// task's heartbeat keeps advancing — a silent-but-alive backgrounded
    /// command must never read as stale (the `drive_with_process_heartbeat`
    /// rule the hand-over continuation path forgot, session-f1e24e04
    /// incident: a silent cargo test's dead heartbeat starved the
    /// `TaskOutput` liveness feed and the parent turn's stall watchdog
    /// killed a healthy turn).
    #[tokio::test]
    async fn should_keep_a_backgrounded_task_heartbeat_advancing_while_its_future_is_pending_and_silent()
     {
        // A blocking process double: stays pending (no exit, no output —
        // the writer half is never touched) until the test finishes it.
        let (process, _writer) = BlockingProcess::with_stdout();
        let tasks =
            Arc::new(TaskManager::new().with_heartbeat_interval(Duration::from_millis(200)));

        // Foreground timeout 1s converts the pending, silent run to the
        // background continuation; the re-armed deadline is far away so
        // the continuation stays looping.
        let result = run_foreground_tracked(
            Arc::clone(&process) as Arc<dyn DynOsProcess>,
            "silent build".to_string(),
            None,
            Some(1),
            Some(600),
            false,
            CancelToken::new(),
            ToolProgressHandle::default(),
            Arc::clone(&tasks),
            "session-1".to_string(),
            "agent-main".to_string(),
            "toolu-bg-hb".to_string(),
        )
        .await;
        assert!(!result.is_error);
        let text = output_text(&result);
        assert!(
            text.contains("automatically moved to background"),
            "the foreground timeout converts the run, got: {text}"
        );
        // The continuation REUSES the foreground task id (no new row is
        // registered) — sample liveness through the id the tool result
        // carries.
        let task_id = extract_task_id(&text);

        // When the continuation has run silent for several heartbeat
        // windows, then the heartbeat kept advancing: the manager's
        // liveness listener fires while the run stays pending (the same
        // signal the TaskOutput wait and the stall watchdog consume).
        // Sample through `wait_for_completion_with` — the production
        // liveness consumer — whose 1s wait window lets the ticker beat
        // many times over (window 200ms / beat 50ms).
        let liveness_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count_clone = std::sync::Arc::clone(&liveness_count);
        tasks.set_liveness_listener(Some(Arc::new(move |_task_id| {
            count_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        })));
        let wait_result = tasks.wait_for_completion_with(&task_id, 1, None).await;
        assert_eq!(
            wait_result,
            crate::task::WaitResult::TimedOut,
            "the silent process must still be pending after the wait window"
        );
        assert!(
            liveness_count.load(std::sync::atomic::Ordering::SeqCst) >= 2,
            "a silent-but-alive backgrounded command must keep its heartbeat advancing \
             (the liveness feed the TaskOutput wait and the stall watchdog consume); \
             liveness reports: {}",
            liveness_count.load(std::sync::atomic::Ordering::SeqCst)
        );

        // Cleanup: finish the process and stop the row.
        process.finish(0);
        tasks.stop(&task_id);
    }

    /// Given a successful command with stdout, When it settles, Then the
    /// model sees the captured stdout as plain text — no JSON envelope
    /// (TS `foregroundCompletionResult` → `builder.ok`, bash.ts:475-476:
    /// with captured output the ok message is not appended,
    /// result-builder.ts:110-121; audit P2-2, ADR-013).
    #[tokio::test]
    async fn should_return_plain_text_output_when_the_command_succeeds() {
        let (result, _tasks) = run_tracked(
            local_process(),
            "echo hello-v3",
            Some(30),
            CancelToken::new(),
            ToolProgressHandle::default(),
        )
        .await;

        assert!(!result.is_error);
        let text = output_text(&result);
        assert_eq!(text, "hello-v3\n");
        assert!(!text.contains("exitCode"), "no JSON keys, got: {text}");
        assert!(!text.contains('{'), "no JSON envelope, got: {text}");
    }

    /// Given a successful command that produces no output, When it settles,
    /// Then the output is the TS placeholder `Command executed successfully.`
    /// (bash.ts:476 — `builder.ok('Command executed successfully.')` appends
    /// the message only when nothing was captured, result-builder.ts:111-121).
    #[tokio::test]
    async fn should_report_success_placeholder_when_the_command_produces_no_output() {
        let (result, _tasks) = run_tracked(
            local_process(),
            "true",
            Some(30),
            CancelToken::new(),
            ToolProgressHandle::default(),
        )
        .await;

        assert!(!result.is_error);
        assert_eq!(output_text(&result), "Command executed successfully.");
    }

    /// Given a failing command (exit 3, no output), When it settles, Then the
    /// output carries the exit-code status lines and the result is an error
    /// (bash.ts:477-481: placeholder `Process exited with code N` when the
    /// builder is empty, then the `Command failed with exit code: N.` label).
    #[tokio::test]
    async fn should_report_the_exit_code_in_plain_text_when_the_command_fails() {
        let (result, _tasks) = run_tracked(
            local_process(),
            "exit 3",
            Some(30),
            CancelToken::new(),
            ToolProgressHandle::default(),
        )
        .await;

        assert!(result.is_error);
        assert_eq!(
            output_text(&result),
            "Process exited with code 3\nCommand failed with exit code: 3."
        );
    }

    #[tokio::test]
    async fn should_run_in_the_given_directory_when_executing_command_with_cwd() {
        let tmp = tempfile::tempdir().unwrap();
        let tasks = Arc::new(TaskManager::new());
        let result = run_foreground_tracked(
            local_process(),
            "pwd".to_string(),
            Some(tmp.path().to_path_buf()),
            Some(30),
            Some(600),
            false,
            CancelToken::new(),
            ToolProgressHandle::default(),
            Arc::clone(&tasks),
            "session-1".to_string(),
            "agent-main".to_string(),
            "toolu-cwd".to_string(),
        )
        .await;
        assert!(!result.is_error);
        // `pwd` reports the canonical path; on macOS the temp dir may live
        // under `/var` while canonicalization resolves it to `/private/var`.
        let expected = tmp
            .path()
            .canonicalize()
            .unwrap_or_else(|_| tmp.path().to_path_buf());
        assert_eq!(output_text(&result), format!("{}\n", expected.display()));
    }

    /// Given the Bash tool bound to the session workdir, When a command runs
    /// without a `cwd` argument, Then it executes in the session workdir —
    /// not the engine process's cwd (TS parity: `effectiveCwd =
    /// args.cwd ?? this.cwd`, bash.ts:322; audit P0-1).
    #[tokio::test]
    async fn should_run_in_the_session_workdir_when_no_cwd_is_given() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("cwd-marker.txt"), "x").unwrap();
        let tasks = Arc::new(TaskManager::new());
        let tool = BashTool::new(
            Arc::clone(&tasks),
            "agent-main".to_string(),
            "session-1".to_string(),
            local_process(),
            Some(tmp.path().to_path_buf()),
        );

        let result = execute_tool(&tool, serde_json::json!({ "command": "ls" })).await;

        assert!(!result.is_error);
        let text = output_text(&result);
        assert!(
            text.contains("cwd-marker.txt"),
            "expected the session workdir listing, got: {text}"
        );
    }

    /// Given the Bash tool bound to a session default workdir, When the model
    /// supplies an explicit `cwd`, Then the model-supplied directory wins
    /// over the session default.
    #[tokio::test]
    async fn should_prefer_the_model_supplied_cwd_over_the_session_default() {
        let tmp_default = tempfile::tempdir().unwrap();
        std::fs::write(tmp_default.path().join("default-marker.txt"), "x").unwrap();
        let tmp_explicit = tempfile::tempdir().unwrap();
        std::fs::write(tmp_explicit.path().join("explicit-marker.txt"), "x").unwrap();
        let tasks = Arc::new(TaskManager::new());
        let tool = BashTool::new(
            Arc::clone(&tasks),
            "agent-main".to_string(),
            "session-1".to_string(),
            local_process(),
            Some(tmp_default.path().to_path_buf()),
        );

        let result = execute_tool(
            &tool,
            serde_json::json!({
                "command": "ls",
                "cwd": tmp_explicit.path().display().to_string(),
            }),
        )
        .await;

        assert!(!result.is_error);
        let text = output_text(&result);
        assert!(
            text.contains("explicit-marker.txt"),
            "expected the model-supplied cwd listing, got: {text}"
        );
        assert!(
            !text.contains("default-marker.txt"),
            "the session default must not win over the model-supplied cwd, got: {text}"
        );
    }

    #[tokio::test]
    async fn should_run_command_in_foreground_when_run_in_background_is_not_set() {
        let (tasks, tool) = test_tool();
        let result = execute_tool(&tool, serde_json::json!({ "command": "echo foreground" })).await;
        assert!(!result.is_error);
        assert!(output_text(&result).contains("foreground"));
        // B7: a foreground run registers a FOREGROUND task row (the
        // detachable-tracking seam) — detached=false, and it settles with
        // the command instead of dangling as Running.
        let listed = tasks.list();
        assert_eq!(listed.len(), 1, "one foreground task row");
        assert!(!listed[0].detached);
        assert_eq!(listed[0].status, TaskStatus::Completed { exit_code: 0 });
    }

    #[tokio::test]
    async fn should_run_command_in_background_when_run_in_background_is_true() {
        let (tasks, tool) = test_tool();
        let result = execute_tool(
            &tool,
            serde_json::json!({
                "command": "echo background hello",
                "run_in_background": true,
                "description": "say hi in the background"
            }),
        )
        .await;

        assert!(!result.is_error);
        let task_id = extract_task_id(&output_text(&result));

        let task = tasks.get(&task_id).expect("task is registered");
        assert_eq!(task.status, TaskStatus::Running);
        assert_eq!(task.description, "say hi in the background");

        assert_eq!(
            tasks.wait_for_completion(&task_id, 10).await,
            WaitResult::Completed
        );
        let task = tasks.get(&task_id).unwrap();
        assert_eq!(task.status, TaskStatus::Completed { exit_code: 0 });
        let output = tasks.output(&task_id).unwrap();
        assert!(
            output.full_output.contains("background hello"),
            "task output should contain the command stdout, was: {}",
            output.full_output
        );
    }

    #[tokio::test]
    async fn should_return_task_id_when_running_in_background() {
        let (tasks, tool) = test_tool();
        let result = execute_tool(
            &tool,
            serde_json::json!({
                "command": "echo hi",
                "run_in_background": true,
                "description": "echo hi"
            }),
        )
        .await;

        assert!(!result.is_error);
        let text = output_text(&result);
        assert!(text.contains("task_id: task-"), "{text}");
        let task_id = extract_task_id(&text);
        assert!(
            tasks.get(&task_id).is_some(),
            "task {task_id} must be tracked"
        );
        assert!(text.contains("status: running"), "{text}");
    }

    #[tokio::test]
    async fn should_use_custom_description_when_provided_for_background_task() {
        let (tasks, tool) = test_tool();
        let result = execute_tool(
            &tool,
            serde_json::json!({
                "command": "echo hello",
                "run_in_background": true,
                "description": "custom background description"
            }),
        )
        .await;

        assert!(!result.is_error);
        let task_id = extract_task_id(&output_text(&result));
        assert_eq!(
            tasks.get(&task_id).unwrap().description,
            "custom background description"
        );
    }

    /// Given a directly backgrounded command, when the run starts, then the
    /// task row carries the process identity fingerprint right away (US14.1
    /// — the startup orphan sweep cannot verify a fingerprint-less archive).
    #[tokio::test]
    async fn should_attach_a_fingerprint_when_a_command_starts_in_background() {
        let (tasks, tool) = test_tool();
        let result = execute_tool(
            &tool,
            serde_json::json!({
                "command": "sleep 30",
                "run_in_background": true,
                "description": "sleep for the test"
            }),
        )
        .await;

        assert!(!result.is_error);
        let task_id = extract_task_id(&output_text(&result));

        // The fingerprint must be attached while the child is still alive —
        // poll briefly rather than assume it beats the (still sleeping) run.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let task = loop {
            let task = tasks.get(&task_id).expect("task registered");
            if task.fingerprint.is_some() {
                break task;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "background process tasks must carry the identity fingerprint (US14.1)"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        };

        // The fingerprint is self-consistent and matches the live child.
        assert!(task.fingerprint.is_some());
        let fp = task.fingerprint.unwrap();
        assert!(fp.pid > 0, "the pid must be the real child pid");
        #[cfg(unix)]
        {
            assert_eq!(fp.pgid, fp.pid, "the spawned child leads its own group");
            let probe = mirri_os::probe_process(fp.pid).expect("probe the live child");
            assert!(probe.alive, "the fingerprinted child is still running");
            assert_eq!(probe.start_ticks, Some(fp.start_ticks));
        }

        // Cleanup: stop the still-running backgrounded task.
        tasks.stop(&task_id);
    }

    /// Given a foreground command that hits its timeout and converts to a
    /// background task, when the conversion happens, then the converted
    /// task's archive carries the fingerprint too (US14.1: the foreground
    /// attach makes every conversion path fingerprinted).
    #[tokio::test]
    async fn should_carry_the_fingerprint_when_a_foreground_timeout_moves_the_run_to_background() {
        let (tasks, tool) = test_tool();
        let result = execute_tool(
            &tool,
            serde_json::json!({ "command": "sleep 30", "timeout": 1 }),
        )
        .await;

        assert!(!result.is_error);
        let text = output_text(&result);
        assert!(
            text.contains("automatically moved to background"),
            "got: {text}"
        );
        let task_id = extract_task_id(&text);

        let task = tasks.get(&task_id).expect("task registered");
        assert!(
            task.fingerprint.is_some(),
            "the converted archive must carry the identity fingerprint (US14.1)"
        );
        let fp = task.fingerprint.unwrap();
        assert!(fp.pid > 0);
        #[cfg(unix)]
        assert_eq!(fp.pgid, fp.pid);

        // Cleanup: stop the still-running backgrounded task.
        tasks.stop(&task_id);
    }

    #[tokio::test]
    async fn should_fall_back_to_command_preview_description_when_background_description_missing() {
        let (tasks, tool) = test_tool();
        let result = execute_tool(
            &tool,
            serde_json::json!({
                "command": "echo hello",
                "run_in_background": true
            }),
        )
        .await;

        assert!(!result.is_error);
        let task_id = extract_task_id(&output_text(&result));
        assert_eq!(tasks.get(&task_id).unwrap().description, "Bash: echo hello");
    }

    #[tokio::test]
    async fn should_not_timeout_when_disable_timeout_is_set() {
        let (_, tool) = test_tool();
        // `timeout: 1` would kill `sleep 2`; disable_timeout must skip the
        // deadline entirely (the command is allowed to finish at ~2s).
        let result = execute_tool(
            &tool,
            serde_json::json!({
                "command": "sleep 2",
                "timeout": 1,
                "disable_timeout": true
            }),
        )
        .await;

        assert!(!result.is_error);
        // `sleep 2` produced no output: the TS success placeholder.
        assert_eq!(output_text(&result), "Command executed successfully.");
    }

    /// Given a foreground command that hits its timeout, when the deadline
    /// passes, then it is NOT killed — it is moved to a background task with
    /// the reason spelled out and the no-polling guidance kept (ADR-4: the
    /// foreground timeout auto-backgrounds instead of killing).
    #[tokio::test]
    async fn should_move_foreground_timeout_to_background_task_instead_of_killing() {
        let (tasks, tool) = test_tool();
        let result = execute_tool(
            &tool,
            serde_json::json!({
                "command": "echo started && sleep 30",
                "timeout": 1
            }),
        )
        .await;

        assert!(!result.is_error);
        let text = output_text(&result);
        let task_id = extract_task_id(&text);
        // The message says why and what to do (auto-notify, no polling).
        assert!(
            text.contains("automatically moved to background"),
            "message should explain the reason, got: {text}"
        );
        assert!(
            text.contains("do NOT wait, poll"),
            "message should keep the no-polling guidance, got: {text}"
        );
        // The task row is detached (a real background task), still running.
        let task = tasks.get(&task_id).expect("task registered");
        assert!(
            task.detached,
            "timed-out foreground run becomes a background task"
        );
        assert_eq!(task.status, crate::task::TaskStatus::Running);

        // Cleanup: stop the still-running task.
        tasks.stop(&task_id);
    }

    /// Given a foreground command that timed out into a background task, when
    /// the user stops the background task, then the run settles `Stopped`
    /// (D-63 — the TaskStop signal really kills the child, not just flips the
    /// row, and the row is never left dangling as `Running`).
    #[tokio::test]
    async fn should_kill_backgrounded_run_when_taskstop_fires() {
        let (tasks, tool) = test_tool();
        let result = execute_tool(
            &tool,
            serde_json::json!({ "command": "sleep 300", "timeout": 1 }),
        )
        .await;
        let task_id = extract_task_id(&output_text(&result));

        // When the user stops the background task (D-63 signal)...
        tasks.stop(&task_id);

        // Then the run settles stopped (not dangling running).
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut status = None;
        while tokio::time::Instant::now() < deadline {
            if let Some(task) = tasks.get(&task_id)
                && task.status.is_terminal()
            {
                status = Some(task.status);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            matches!(status, Some(crate::task::TaskStatus::Stopped)),
            "TaskStop settles the backgrounded run"
        );
    }

    /// Given `disable_timeout` on a foreground run, when the run would
    /// otherwise outlive a deadline, then no deadline is armed at all — the
    /// run is never killed by timeout (disable removes the foreground AND the
    /// would-be re-armed background deadline alike).
    #[tokio::test]
    async fn should_not_arm_second_deadline_when_disable_timeout_is_set() {
        let (_tasks, tool) = test_tool();
        // `timeout: 1` would move `sleep 2` to the background; disable_timeout
        // must skip the deadline entirely so the command completes normally.
        let result = execute_tool(
            &tool,
            serde_json::json!({
                "command": "sleep 2",
                "timeout": 1,
                "disable_timeout": true
            }),
        )
        .await;

        assert!(!result.is_error);
        // `sleep 2` produced no output: the TS success placeholder.
        assert_eq!(output_text(&result), "Command executed successfully.");
    }

    /// Given `exempt_lifetime` on a background run, when the task registers,
    /// then the row carries the lifetime exemption AND the command actually
    /// runs as a stoppable detached background task (the exemption must not
    /// bypass the signal wiring, capacity slot, or the running child —
    /// US14.4 lifts only the age-based ceiling).
    #[tokio::test]
    async fn should_stamp_the_lifetime_exemption_on_a_background_run() {
        let (tasks, tool) = test_tool();
        let result = execute_tool(
            &tool,
            serde_json::json!({
                "command": "sleep 30",
                "run_in_background": true,
                "exempt_lifetime": true
            }),
        )
        .await;

        assert!(!result.is_error, "{}", output_text(&result));
        let task_id = extract_task_id(&output_text(&result));
        // The row is exempt, detached, and Running (the child is live).
        let task = tasks.get(&task_id).expect("task is registered");
        assert!(
            task.lifetime_exempt,
            "the exempt dispatch must stamp the registration"
        );
        assert!(task.detached, "the exempt run is still a background task");
        assert_eq!(task.status, TaskStatus::Running);

        // Cleanup: the stop must REALLY kill the child (the signal wiring
        // is intact) — an exemption must never orphan the process.
        tasks.stop(&task_id);
        assert_eq!(
            tasks.wait_for_completion(&task_id, 5).await,
            crate::task::WaitResult::Completed,
            "the exempt background run must be stoppable (signal wired)"
        );
    }

    /// Given a background run without `exempt_lifetime`, when the task
    /// registers, then the row stays under the ceiling (the default false —
    /// the exemption is an explicit opt-in, never a silent default).
    #[tokio::test]
    async fn should_keep_a_background_run_under_the_ceiling_when_the_flag_is_absent() {
        let (tasks, tool) = test_tool();
        let result = execute_tool(
            &tool,
            serde_json::json!({
                "command": "sleep 30",
                "run_in_background": true
            }),
        )
        .await;

        assert!(!result.is_error, "{}", output_text(&result));
        let task_id = extract_task_id(&output_text(&result));
        assert!(
            !tasks.get(&task_id).unwrap().lifetime_exempt,
            "an ordinary background task must stay under the lifetime ceiling"
        );

        // Cleanup: stop the still-running backgrounded task.
        tasks.stop(&task_id);
    }

    /// Given `exempt_lifetime` on a foreground run that converts to a
    /// background task (the foreground timeout), when the conversion
    /// happens, then the converted row keeps the exemption (the stamp
    /// happened at registration; the conversion carries it untouched).
    #[tokio::test]
    async fn should_carry_the_lifetime_exemption_when_a_foreground_run_converts() {
        let (tasks, tool) = test_tool();
        let result = execute_tool(
            &tool,
            serde_json::json!({
                "command": "sleep 30",
                "timeout": 1,
                "exempt_lifetime": true
            }),
        )
        .await;

        assert!(!result.is_error, "{}", output_text(&result));
        let task_id = extract_task_id(&output_text(&result));
        let task = tasks.get(&task_id).expect("task is registered");
        assert!(
            task.lifetime_exempt,
            "the converted row must keep the exemption stamped at registration"
        );

        // Cleanup: stop the still-running backgrounded task.
        tasks.stop(&task_id);
    }

    /// Given the re-arm resolution, when a timed-out run converts, then the
    /// continuation's deadline is the background semantics (default 600,
    /// explicit ≤86400 honored, disable → no second deadline at all).
    #[test]
    fn should_rearm_the_background_deadline_when_a_run_moves_to_background() {
        use std::time::Duration;
        assert_eq!(rearmed_deadline(Some(600)), Some(Duration::from_secs(600)));
        assert_eq!(
            rearmed_deadline(Some(1200)),
            Some(Duration::from_secs(1200))
        );
        assert_eq!(rearmed_deadline(None), None);
    }

    #[tokio::test]
    async fn should_set_no_color_and_term_dumb_in_environment() {
        let (result, _tasks) = run_tracked(
            local_process(),
            "printf '%s|%s|%s|%s' \"$NO_COLOR\" \"$TERM\" \"$GIT_TERMINAL_PROMPT\" \"$SHELL\"",
            Some(30),
            CancelToken::new(),
            ToolProgressHandle::default(),
        )
        .await;

        assert!(!result.is_error);
        let expected_git = std::env::var("GIT_TERMINAL_PROMPT").unwrap_or_else(|_| "0".to_string());
        let expected_shell = std::env::var("SHELL").unwrap_or_default();
        assert_eq!(
            output_text(&result),
            format!("1|dumb|{expected_git}|{expected_shell}")
        );
    }

    #[tokio::test]
    async fn should_close_stdin_to_prevent_interactive_hang() {
        // `cat` with no arguments blocks forever reading stdin; with stdin
        // closed it sees immediate EOF and exits 0. If stdin were left piped
        // open, this would hang until the 5s timeout (a nonzero exit).
        let (result, _tasks) = run_tracked(
            local_process(),
            "cat",
            Some(5),
            CancelToken::new(),
            ToolProgressHandle::default(),
        )
        .await;

        assert!(!result.is_error);
        // `cat` saw immediate EOF and produced no output: the placeholder.
        assert_eq!(output_text(&result), "Command executed successfully.");
    }

    #[tokio::test]
    async fn should_kill_command_and_report_cancelled_when_cancel_token_fires() {
        let cancel = CancelToken::new();
        let cancel_handle = cancel.clone();
        let (result, _tasks) = tokio::join!(
            run_tracked(
                local_process(),
                "sleep 30",
                Some(60),
                cancel,
                ToolProgressHandle::default(),
            ),
            async {
                tokio::time::sleep(Duration::from_millis(300)).await;
                cancel_handle.cancel();
            }
        )
        .0;

        assert!(result.is_error);
        let text = output_text(&result);
        assert!(text.contains("command cancelled"), "got: {text}");
        assert!(
            text.contains("Command failed with exit code: -1."),
            "got: {text}"
        );
    }

    #[test]
    fn should_default_to_60_second_timeout() {
        // The default timeout constant must be 60 seconds (aligned with TS engine).
        assert_eq!(DEFAULT_TIMEOUT_SECS, 60);

        // When the input omits "timeout", the resolver falls back to the default.
        assert_eq!(resolve_timeout_secs(None), 60);
    }

    #[test]
    fn should_cap_timeout_at_300_seconds_when_timeout_exceeds_max() {
        // The maximum timeout constant must be 300 seconds (aligned with TS engine).
        assert_eq!(MAX_TIMEOUT_SECS, 300);

        // Values above 300 are clamped down to 300.
        assert_eq!(resolve_timeout_secs(Some(99999)), 300);

        // Values at or below 300 pass through unchanged.
        assert_eq!(resolve_timeout_secs(Some(300)), 300);
        assert_eq!(resolve_timeout_secs(Some(45)), 45);
    }

    #[test]
    fn should_use_background_default_timeout_when_background_request_omits_timeout() {
        // Given a background request without an explicit timeout
        let input = parse_input(&serde_json::json!({
            "command": "cargo test --workspace",
            "run_in_background": true
        }))
        .expect("parse");

        // When the effective timeout is resolved
        // Then the background default (600s, TS `DEFAULT_BACKGROUND_TIMEOUT_S`) applies
        assert_eq!(
            effective_timeout_secs(&input),
            EffectiveTimeout::Background(600)
        );
    }

    #[test]
    fn should_honor_long_explicit_timeout_for_background_request() {
        // Given a background request with an explicit timeout above the old
        // foreground cap
        let input = parse_input(&serde_json::json!({
            "command": "cargo test --workspace",
            "run_in_background": true,
            "timeout": 1200
        }))
        .expect("parse");

        // When the effective timeout is resolved
        // Then it passes through (no 300s clamp on background runs)
        assert_eq!(
            effective_timeout_secs(&input),
            EffectiveTimeout::Background(1200)
        );
    }

    #[test]
    fn should_keep_foreground_semantics_unchanged_for_background_timeout_fix() {
        // Given a foreground request without an explicit timeout
        let foreground = parse_input(&serde_json::json!({ "command": "echo hi" })).expect("parse");
        // Then the foreground default (60s) still applies
        assert_eq!(
            effective_timeout_secs(&foreground),
            EffectiveTimeout::Foreground(60)
        );

        // Given a foreground request with an over-cap timeout
        let clamped = parse_input(&serde_json::json!({ "command": "echo hi", "timeout": 600 }))
            .expect("parse");
        // Then it is still clamped to the foreground max (300s)
        assert_eq!(
            effective_timeout_secs(&clamped),
            EffectiveTimeout::Foreground(300)
        );

        // Given a request with disable_timeout set
        let disabled = parse_input(&serde_json::json!({
            "command": "cargo test --workspace",
            "run_in_background": true,
            "disable_timeout": true
        }))
        .expect("parse");
        // Then no deadline is armed at all
        assert_eq!(
            effective_timeout_secs(&disabled),
            EffectiveTimeout::Disabled
        );
    }

    #[tokio::test]
    async fn should_reject_background_timeout_above_86400_seconds() {
        // Given a background request whose timeout exceeds the background cap
        let (tasks, tool) = test_tool();
        let result = execute_tool(
            &tool,
            serde_json::json!({
                "command": "sleep 100000",
                "run_in_background": true,
                "timeout": 86401
            }),
        )
        .await;

        // Then the request is rejected with the cap message (TS parity: the
        // schema refuses rather than clamping)
        assert!(result.is_error);
        assert_eq!(
            output_text(&result),
            "timeout must be ≤ 86400s (background)"
        );
        // And no task was spawned
        assert!(tasks.list().is_empty());
    }
    // -------------------------------------------------------------------------
    // T-13a-3: injected process provider (zero-IO injection)
    // -------------------------------------------------------------------------

    /// A scripted [`mirri_os::DynChildProcess`] double: fixed stdout/stderr
    /// streams and exit code, no real OS process.
    struct ScriptedChild {
        exit_code: i32,
        stdout: Option<mirri_os::DynChildStream>,
        stderr: Option<mirri_os::DynChildStream>,
    }

    impl mirri_os::DynChildProcess for ScriptedChild {
        fn take_stdin(&mut self) {}

        fn take_stdout(&mut self) -> Option<mirri_os::DynChildStream> {
            self.stdout.take()
        }

        fn take_stderr(&mut self) -> Option<mirri_os::DynChildStream> {
            self.stderr.take()
        }

        fn wait(&mut self) -> mirri_os::DynProcessFuture<'_, anyhow::Result<i32>> {
            let exit_code = self.exit_code;
            Box::pin(async move { Ok(exit_code) })
        }

        fn kill(&mut self) -> mirri_os::DynProcessFuture<'_, anyhow::Result<()>> {
            Box::pin(async { Ok(()) })
        }
    }

    /// A scripted [`mirri_os::DynOsProcess`] double recording every exec and
    /// returning a [`ScriptedChild`] with the configured output and exit
    /// code (T-13a-3).
    struct ScriptedProcess {
        exit_code: i32,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
        execs: StdMutex<Vec<String>>,
    }

    impl ScriptedProcess {
        fn new(exit_code: i32, stdout: Vec<u8>, stderr: Vec<u8>) -> Arc<Self> {
            Arc::new(Self {
                exit_code,
                stdout,
                stderr,
                execs: StdMutex::new(Vec::new()),
            })
        }
    }

    impl mirri_os::DynOsProcess for ScriptedProcess {
        fn exec<'a>(
            &'a self,
            cmd: &'a str,
            args: &'a [&'a str],
            _opts: &'a ExecOptions,
        ) -> mirri_os::DynProcessFuture<'a, anyhow::Result<Box<dyn mirri_os::DynChildProcess>>>
        {
            Box::pin(async move {
                self.execs
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(format!("{cmd} {}", args.join(" ")));
                let child: Box<dyn mirri_os::DynChildProcess> = Box::new(ScriptedChild {
                    exit_code: self.exit_code,
                    stdout: Some(Box::new(std::io::Cursor::new(self.stdout.clone()))),
                    stderr: Some(Box::new(std::io::Cursor::new(self.stderr.clone()))),
                });
                Ok(child)
            })
        }
    }

    /// Given the Bash tool holding an injected fake process that exits 1
    /// with scripted stdout/stderr, When a foreground command executes, Then
    /// the result is the plain-text captured output (stdout, then stderr)
    /// plus the failure status line, and the exec went through the injected
    /// provider (T-13a-3).
    #[tokio::test]
    async fn should_report_failure_payload_when_the_injected_process_exits_nonzero() {
        let tasks = Arc::new(TaskManager::new());
        let process = ScriptedProcess::new(1, b"out line\n".to_vec(), b"err line\n".to_vec());
        let tool = BashTool::new(
            Arc::clone(&tasks),
            "agent-main".to_string(),
            "session-1".to_string(),
            Arc::clone(&process) as Arc<dyn DynOsProcess>,
            None,
        );

        let result = execute_tool(
            &tool,
            serde_json::json!({
                "command": "boom"
            }),
        )
        .await;

        assert!(result.is_error, "a nonzero exit is an error result");
        assert_eq!(
            output_text(&result),
            "out line\nerr line\nCommand failed with exit code: 1."
        );

        let execs = process.execs.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            execs.as_slice(),
            ["sh -c boom".to_string()],
            "the exec must go through the injected provider"
        );
    }

    // -------------------------------------------------------------------------
    // B7 R2: the detachable foreground runner (D-3/D-4)
    // -------------------------------------------------------------------------

    /// Run one foreground command through the detachable tracked runner
    /// against the engine-default local process, returning the result plus
    /// the (fresh) task manager the run registered under — the ported
    /// settled-path tests' shared bed (they used to call run_bash_command
    /// directly; the tracked runner is the only foreground path now).
    async fn run_tracked(
        process: Arc<dyn DynOsProcess>,
        command: &str,
        timeout_secs: Option<u64>,
        cancel: CancelToken,
        progress: ToolProgressHandle,
    ) -> (ToolResult, Arc<TaskManager>) {
        let tasks = Arc::new(TaskManager::new());
        let result = run_foreground_tracked(
            process,
            command.to_string(),
            None,
            timeout_secs,
            // The settled-path tests' foreground runs default their
            // background re-arm to the 600s default (irrelevant to a
            // command that completes inside the foreground window).
            timeout_secs.map(|_| 600),
            false,
            cancel,
            progress,
            Arc::clone(&tasks),
            "session-1".to_string(),
            "agent-main".to_string(),
            "toolu-test".to_string(),
        )
        .await;
        (result, tasks)
    }

    /// A blocking [`mirri_os::DynChildProcess`] double: the stdout stream
    /// stays open until the test drops its writer half, and the exit code
    /// stays pending until the test resolves it through the process double
    /// (the "still-running process" the detach tests must keep alive).
    struct BlockingChild {
        stdout: Option<mirri_os::DynChildStream>,
        stderr: Option<mirri_os::DynChildStream>,
        exit: Arc<StdMutex<Option<i32>>>,
        gate: Arc<tokio::sync::Notify>,
        killed: Arc<std::sync::atomic::AtomicBool>,
    }

    impl mirri_os::DynChildProcess for BlockingChild {
        fn take_stdin(&mut self) {}

        fn take_stdout(&mut self) -> Option<mirri_os::DynChildStream> {
            self.stdout.take()
        }

        fn take_stderr(&mut self) -> Option<mirri_os::DynChildStream> {
            self.stderr.take()
        }

        fn wait(&mut self) -> mirri_os::DynProcessFuture<'_, anyhow::Result<i32>> {
            let exit = Arc::clone(&self.exit);
            let gate = Arc::clone(&self.gate);
            Box::pin(async move {
                loop {
                    if let Some(exit_code) = *exit.lock().unwrap_or_else(|e| e.into_inner()) {
                        return Ok(exit_code);
                    }
                    gate.notified().await;
                }
            })
        }

        fn kill(&mut self) -> mirri_os::DynProcessFuture<'_, anyhow::Result<()>> {
            self.killed.store(true, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
    }

    /// A scripted [`mirri_os::DynOsProcess`] double whose child blocks
    /// until the test resolves the exit code — the detachable-runner
    /// counterpart of [`ScriptedProcess`].
    struct BlockingProcess {
        /// The stream slot the (single) exec takes from — the test hands
        /// the duplex reader half in at construction and keeps the writer.
        stdout: StdMutex<Option<mirri_os::DynChildStream>>,
        exit: Arc<StdMutex<Option<i32>>>,
        gate: Arc<tokio::sync::Notify>,
        /// Observable kill: set by the child's `kill` arm, read by the
        /// tests that pin the stop-really-kills contract.
        killed: Arc<std::sync::atomic::AtomicBool>,
    }

    impl BlockingProcess {
        /// The child reads from a duplex reader half the constructor
        /// mints; the returned writer half stays with the test — writing
        /// to it feeds the run's stdout stream, dropping it ends it.
        fn with_stdout() -> (Arc<Self>, tokio::io::DuplexStream) {
            let (writer, reader) = tokio::io::duplex(64);
            (
                Arc::new(Self {
                    stdout: StdMutex::new(Some(Box::new(reader))),
                    exit: Arc::new(StdMutex::new(None)),
                    gate: Arc::new(tokio::sync::Notify::new()),
                    killed: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                }),
                writer,
            )
        }

        /// Resolve the child's exit code (the fake process finishes).
        fn finish(&self, exit_code: i32) {
            *self.exit.lock().unwrap_or_else(|e| e.into_inner()) = Some(exit_code);
            self.gate.notify_one();
        }

        /// Whether the child's kill arm fired.
        fn killed(&self) -> bool {
            self.killed.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl mirri_os::DynOsProcess for BlockingProcess {
        fn exec<'a>(
            &'a self,
            _cmd: &'a str,
            _args: &'a [&'a str],
            _opts: &'a ExecOptions,
        ) -> mirri_os::DynProcessFuture<'a, anyhow::Result<Box<dyn mirri_os::DynChildProcess>>>
        {
            let stdout = self.stdout.lock().unwrap_or_else(|e| e.into_inner()).take();
            let child: Box<dyn mirri_os::DynChildProcess> = Box::new(BlockingChild {
                stdout,
                stderr: None,
                exit: Arc::clone(&self.exit),
                gate: Arc::clone(&self.gate),
                killed: Arc::clone(&self.killed),
            });
            Box::pin(async move { Ok(child) })
        }
    }

    /// Given a foreground Bash run, when it settles naturally, then the task
    /// row lands Completed{exit_code} with the output appended, and the
    /// ToolResult text is the plain-text captured output (TS parity:
    /// `foregroundCompletionResult`).
    #[tokio::test]
    async fn should_settle_the_foreground_task_when_the_command_completes() {
        let (result, tasks) = run_tracked(
            local_process(),
            "echo done",
            Some(30),
            CancelToken::new(),
            ToolProgressHandle::default(),
        )
        .await;

        assert_eq!(output_text(&result), "done\n");

        let task = tasks.list().into_iter().next().expect("one task row");
        assert_eq!(task.status, TaskStatus::Completed { exit_code: 0 });
        assert!(
            !task.detached,
            "a natural foreground settle stays foreground"
        );
        assert_eq!(task.output, "done\n", "the output lands on the task row");
    }

    /// Given a foreground Bash run under a detach, when the release fires,
    /// then the tool returns the moved-to-background result immediately,
    /// the process keeps running, and the task later settles Completed
    /// through settle_background_run.
    #[tokio::test]
    async fn should_return_the_backgrounded_result_and_keep_the_process_running_when_detached() {
        use tokio::io::AsyncWriteExt;

        let (process, mut writer) = BlockingProcess::with_stdout();
        let tasks = Arc::new(TaskManager::new());
        let (progress, updates) = record_progress();
        let run = tokio::spawn(run_foreground_tracked(
            Arc::clone(&process) as Arc<dyn DynOsProcess>,
            "sleep 5".to_string(),
            None,
            Some(60),
            Some(600),
            false,
            CancelToken::new(),
            progress,
            Arc::clone(&tasks),
            "session-1".to_string(),
            "agent-main".to_string(),
            "toolu-1".to_string(),
        ));

        // Poll until the manager holds the running foreground task.
        let task_id = loop {
            if let Some(task) = tasks
                .list()
                .into_iter()
                .find(|task| task.status == TaskStatus::Running && !task.detached)
            {
                break task.id;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };

        writer.write_all(b"partial\n").await.unwrap();
        // Wait until the run consumed the chunk (the progress stream is the
        // observable face) — only then is it safely part of the foreground
        // window the detach result reports.
        let saw_partial = loop {
            let saw = updates
                .lock()
                .unwrap()
                .iter()
                .any(|update| update.text.as_deref() == Some("partial\n"));
            if saw {
                break true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        assert!(saw_partial);
        tasks
            .detach_foreground_by_tool_call("toolu-1")
            .expect("the tool call id addresses the foreground task");
        let result = run.await.unwrap();

        let text = output_text(&result);
        assert!(!result.is_error, "{text}");
        assert!(text.contains(&format!("task_id: {task_id}")), "{text}");
        assert!(text.contains("next_step:"), "{text}");
        assert!(text.contains("foreground_output:"), "{text}");
        assert!(text.contains("partial\n"), "{text}");

        // The task row is Running right after the detach (the process keeps
        // running) and detached=true (the background conversion happened).
        let task = tasks.get(&task_id).expect("task is live");
        assert_eq!(task.status, TaskStatus::Running);
        assert!(task.detached);

        // After the fake process exits 0 the task settles Completed with
        // the foreground output carried over.
        process.finish(0);
        drop(writer);
        assert_eq!(
            tasks.wait_for_completion(&task_id, 5).await,
            WaitResult::Completed
        );
        assert_eq!(
            tasks.get(&task_id).unwrap().status,
            TaskStatus::Completed { exit_code: 0 }
        );
        assert!(
            tasks
                .output(&task_id)
                .unwrap()
                .full_output
                .contains("partial\n"),
            "the foreground output rides the task row, was: {}",
            tasks.output(&task_id).unwrap().full_output
        );
    }

    /// Given a foreground Bash run cancelled by the turn, when the cancel
    /// token fires, then the child is killed AND the task settles (never a
    /// dangling Running row).
    #[tokio::test]
    async fn should_settle_the_task_when_the_turn_cancels_the_foreground_run() {
        // The held-open stdout keeps the select loop in the run phase (an
        // immediate stream EOF would let the natural-exit arm claim the
        // child before the cancellation ever fires).
        let (process, writer) = BlockingProcess::with_stdout();
        let tasks = Arc::new(TaskManager::new());
        let cancel = CancelToken::new();
        let cancel_handle = cancel.clone();
        let run = tokio::spawn(run_foreground_tracked(
            Arc::clone(&process) as Arc<dyn DynOsProcess>,
            "sleep 30".to_string(),
            None,
            Some(60),
            Some(600),
            false,
            cancel,
            ToolProgressHandle::default(),
            Arc::clone(&tasks),
            "session-1".to_string(),
            "agent-main".to_string(),
            "toolu-1".to_string(),
        ));

        let task_id = loop {
            if let Some(task) = tasks
                .list()
                .into_iter()
                .find(|task| task.status == TaskStatus::Running)
            {
                break task.id;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        cancel_handle.cancel();
        let result = run.await.unwrap();
        drop(writer);

        let text = output_text(&result);
        assert!(text.contains("command cancelled"), "got: {text}");
        assert_eq!(
            tasks.get(&task_id).unwrap().status,
            TaskStatus::Stopped,
            "the cancelled run must not leave a dangling Running row"
        );
    }

    /// Given a foreground Bash run stopped through the task face (an
    /// external `task.cancel` / session close settles the row mid-run),
    /// when the Terminal release fires while the run is still looping,
    /// then the child is killed and the run answers the cancelled
    /// spelling — the row keeps its first-terminal status.
    #[tokio::test]
    async fn should_kill_and_answer_cancelled_when_an_external_stop_ends_the_foreground_run() {
        // The held-open stdout keeps the select loop in the run phase (see
        // the cancellation test).
        let (process, writer) = BlockingProcess::with_stdout();
        let tasks = Arc::new(TaskManager::new());
        let run = tokio::spawn(run_foreground_tracked(
            Arc::clone(&process) as Arc<dyn DynOsProcess>,
            "sleep 30".to_string(),
            None,
            Some(60),
            Some(600),
            false,
            CancelToken::new(),
            ToolProgressHandle::default(),
            Arc::clone(&tasks),
            "session-1".to_string(),
            "agent-main".to_string(),
            "toolu-1".to_string(),
        ));

        let task_id = loop {
            if let Some(task) = tasks
                .list()
                .into_iter()
                .find(|task| task.status == TaskStatus::Running)
            {
                break task.id;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        // The same transition the task.cancel dispatch performs.
        tasks.stop(&task_id);
        let result = run.await.unwrap();
        drop(writer);

        let text = output_text(&result);
        assert!(text.contains("command cancelled"), "got: {text}");
        assert_eq!(
            tasks.get(&task_id).unwrap().status,
            TaskStatus::Stopped,
            "the row keeps its first-terminal status"
        );
        assert!(
            process.killed(),
            "the external stop must kill the child, not only flip the row"
        );
    }

    /// Given a foreground command the user detaches mid-run, when the run
    /// hands over to the background continuation, then the converted task's
    /// archive keeps the fingerprint the foreground capture attached (US14.1:
    /// the detach conversion must not lose the identity). Runs against the
    /// real local process provider so the fingerprint is a real one.
    #[tokio::test]
    async fn should_keep_the_fingerprint_when_the_user_detaches_a_foreground_run() {
        let tasks = Arc::new(TaskManager::new());
        let run = tokio::spawn(run_foreground_tracked(
            local_process(),
            "sleep 30".to_string(),
            None,
            Some(60),
            Some(600),
            false,
            CancelToken::new(),
            ToolProgressHandle::default(),
            Arc::clone(&tasks),
            "session-1".to_string(),
            "agent-main".to_string(),
            "toolu-detach".to_string(),
        ));

        // Wait for the foreground row, then fire the user detach (the same
        // transition the task.detach dispatch performs).
        let task_id = loop {
            if let Some(task) = tasks
                .list()
                .into_iter()
                .find(|task| task.status == TaskStatus::Running && !task.detached)
            {
                break task.id;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        tasks
            .detach_foreground_by_tool_call("toolu-detach")
            .expect("the tool call id addresses the foreground task");
        let result = run.await.unwrap();
        assert!(!result.is_error, "{}", output_text(&result));

        // The converted archive carries the fingerprint (a real child was
        // alive at capture time), and the detach did not lose it.
        let task = tasks.get(&task_id).expect("task registered");
        assert!(task.detached, "the detached run is a background task");
        let fingerprint = task
            .fingerprint
            .as_ref()
            .expect("the converted archive carries the fingerprint (US14.1)");
        assert!(fingerprint.pid > 0);
        #[cfg(unix)]
        assert_eq!(fingerprint.pgid, fingerprint.pid);

        // Cleanup: stop the still-running backgrounded task.
        tasks.stop(&task_id);
    }

    /// Given a process double that claims a process group but has no real OS
    /// process behind it, when the fingerprint capture runs, then no
    /// fingerprint is recorded — a probe that cannot confirm a live process
    /// must never attach an identity (ADR-012: recording semantics, the
    /// not-alive branch).
    #[tokio::test]
    async fn should_record_no_fingerprint_when_the_probe_cannot_confirm_the_child() {
        let tasks = Arc::new(TaskManager::new());
        let reg = tasks.register_foreground(
            TaskKind::Process {
                command: Some("fake".to_string()),
                dispatcher: None,
            },
            "fake child".to_string(),
            "session-1".to_string(),
            Some("toolu-fake".to_string()),
        );
        // A double with a claimed group but pid 0 — probe_process's
        // non-positive-pid sentinel answers not-alive.
        struct GroupClaimingDouble;
        impl mirri_os::DynChildProcess for GroupClaimingDouble {
            fn pgid(&self) -> Option<i32> {
                Some(0)
            }
            fn wait(&mut self) -> mirri_os::DynProcessFuture<'_, anyhow::Result<i32>> {
                Box::pin(async { Ok(0) })
            }
            fn kill(&mut self) -> mirri_os::DynProcessFuture<'_, anyhow::Result<()>> {
                Box::pin(async { Ok(()) })
            }
        }
        capture_fingerprint(&GroupClaimingDouble, &tasks, &reg.task_id);
        assert!(
            tasks.get(&reg.task_id).unwrap().fingerprint.is_none(),
            "a probe that cannot confirm a live child must record nothing"
        );
    }

    /// Given a detached Bash run, when the task is stopped afterwards (the
    /// overview's Stop path — task.cancel on a running background row),
    /// then the OS process is REALLY killed (D-63), not only the row
    /// flipped.
    #[tokio::test]
    async fn should_kill_the_process_when_a_detached_task_is_stopped() {
        let (process, writer) = BlockingProcess::with_stdout();
        let tasks = Arc::new(TaskManager::new());
        let run = tokio::spawn(run_foreground_tracked(
            Arc::clone(&process) as Arc<dyn DynOsProcess>,
            "sleep 300".to_string(),
            None,
            Some(60),
            Some(600),
            false,
            CancelToken::new(),
            ToolProgressHandle::default(),
            Arc::clone(&tasks),
            "session-1".to_string(),
            "agent-main".to_string(),
            "toolu-1".to_string(),
        ));

        let task_id = loop {
            if let Some(task) = tasks
                .list()
                .into_iter()
                .find(|task| task.status == TaskStatus::Running && !task.detached)
            {
                break task.id;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        tasks
            .detach_foreground_by_tool_call("toolu-1")
            .expect("the tool call id addresses the foreground task");
        let result = run.await.unwrap();
        assert!(output_text(&result).contains("next_step: The task now runs in the background"));

        // When the detached task is stopped, the process dies too.
        tasks.stop(&task_id);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while !process.killed() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the detached continuation must kill the child on stop"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            tasks.get(&task_id).unwrap().status,
            TaskStatus::Stopped,
            "the stopped row settles Stopped"
        );
        drop(writer);
    }
}
