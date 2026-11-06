//! Process execution abstractions for the mirri execution environment.
//!
//! This module provides an [`OsProcess`] trait that abstracts command
//! execution behind an async interface, plus [`LocalProcess`] — a concrete
//! implementation backed by `tokio::process` for the host system.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// Options that control how a command is executed.
///
/// Mirrors the TypeScript `ExecOptions` interface.
#[derive(Debug, Clone, Default)]
pub struct ExecOptions {
    /// Working directory. If `None`, the child inherits the parent process
    /// current working directory.
    pub cwd: Option<PathBuf>,
    /// Additional environment variables merged on top of the parent process
    /// environment (later keys win). Note: a key with an *empty* value is set
    /// to an empty string in the child's environment — it does NOT unset the
    /// variable (that would require an explicit env_remove, which the current
    /// merge does not do).
    pub env: HashMap<String, String>,
    /// Timeout in milliseconds. `None` means no timeout.
    ///
    /// Note: the timeout is not enforced by [`OsProcess::exec`] itself; callers
    /// that want a timeout should wrap the resulting [`ChildProcess`] wait
    /// with `tokio::time::timeout` or use [`run_to_completion`].
    pub timeout_ms: Option<u64>,
}

/// A running child process with access to its stdio streams.
///
/// Dropping this struct without calling [`ChildProcess::kill`] or
/// [`ChildProcess::wait`] will leave the child process running as an orphan.
/// Callers should always use `wait()` for normal completion or `kill()` for
/// termination.
pub struct ChildProcess {
    /// Process ID of the child as reported by the OS.
    pid: u32,
    /// The underlying `tokio::process::Child`. `None` once the child has been
    /// waited on or killed.
    child: Option<tokio::process::Child>,
    /// The child's stdin pipe, if piped.
    stdin: Option<tokio::process::ChildStdin>,
    /// The child's stdout pipe, if piped.
    stdout: Option<tokio::process::ChildStdout>,
    /// The child's stderr pipe, if piped.
    stderr: Option<tokio::process::ChildStderr>,
    /// The process group ID assigned to the child on Unix. `None` on
    /// non-Unix platforms or when no process group was set.
    #[cfg(unix)]
    pgid: Option<i32>,
}

impl ChildProcess {
    /// Process ID of the child.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// The process group ID assigned to the child, when one was set.
    ///
    /// `None` on non-Unix platforms or when no process group was assigned.
    /// The bash tool pairs this with `pid()` to build the process identity
    /// fingerprint (ADR-012 a) at spawn time.
    pub fn pgid(&self) -> Option<i32> {
        #[cfg(unix)]
        {
            self.pgid
        }
        #[cfg(not(unix))]
        {
            None
        }
    }

    /// Wait for the child to exit and return the exit code.
    ///
    /// Returns an error if the child was already waited on (i.e. the
    /// underlying handle has been consumed).
    ///
    /// If the process was terminated by a signal the exit code is reported as
    /// `-1` since there is no meaningful numeric code in that case.
    pub async fn wait(&mut self) -> anyhow::Result<i32> {
        let child = self
            .child
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("process already consumed"))?;
        let status = child.wait().await.context("failed to wait for child")?;
        Ok(status.code().unwrap_or(-1))
    }

    /// Kill the child process (and its process group on Unix), then wait for
    /// it to actually exit.
    ///
    /// On Unix this first sends `SIGTERM` to the entire process group. If the
    /// process has not exited within 5 seconds, `SIGKILL` is sent. On non-Unix
    /// platforms the child is killed directly via its handle.
    pub async fn kill(&mut self) -> anyhow::Result<()> {
        let child = match self.child.as_mut() {
            Some(child) => child,
            None => return Ok(()),
        };

        // Try to kill the process group first (Unix only).
        #[cfg(unix)]
        if let Some(pgid) = self.pgid {
            let nix_pid = nix::unistd::Pid::from_raw(pgid);
            // Best-effort SIGTERM to the whole group.
            let _ = nix::sys::signal::killpg(nix_pid, nix::sys::signal::Signal::SIGTERM);
        }

        // Give the process up to 5 seconds to exit gracefully.
        match tokio::time::timeout(std::time::Duration::from_secs(5), child.wait()).await {
            Ok(_) => return Ok(()),
            Err(_) => {
                // Timed out — escalate to SIGKILL on Unix, or the platform
                // kill on non-Unix.
                #[cfg(unix)]
                if let Some(pgid) = self.pgid {
                    let nix_pid = nix::unistd::Pid::from_raw(pgid);
                    let _ = nix::sys::signal::killpg(nix_pid, nix::sys::signal::Signal::SIGKILL);
                }
                let _ = child.start_kill();
                child
                    .wait()
                    .await
                    .context("failed to wait for child after kill")?;
            }
        }

        Ok(())
    }

    /// Get mutable access to stdin, if it was piped.
    pub fn stdin(&mut self) -> Option<&mut tokio::process::ChildStdin> {
        self.stdin.as_mut()
    }

    /// Take ownership of the stdin handle, closing the pipe from this struct's
    /// perspective. This causes the child to see EOF on its stdin once all
    /// buffered data is read.
    ///
    /// Returns `None` if stdin was not piped or already taken.
    pub fn take_stdin(&mut self) -> Option<tokio::process::ChildStdin> {
        self.stdin.take()
    }

    /// Get mutable access to stdout, if it was piped.
    pub fn stdout(&mut self) -> Option<&mut tokio::process::ChildStdout> {
        self.stdout.as_mut()
    }

    /// Get mutable access to stderr, if it was piped.
    pub fn stderr(&mut self) -> Option<&mut tokio::process::ChildStderr> {
        self.stderr.as_mut()
    }

    /// Take ownership of the stdout handle, if it was piped and not already
    /// taken. An owned handle lets callers read stdout and stderr
    /// concurrently (the borrowing accessors take `&mut self`, so their
    /// lifetimes cannot overlap).
    pub fn take_stdout(&mut self) -> Option<tokio::process::ChildStdout> {
        self.stdout.take()
    }

    /// Take ownership of the stderr handle — the stderr counterpart of
    /// [`ChildProcess::take_stdout`].
    pub fn take_stderr(&mut self) -> Option<tokio::process::ChildStderr> {
        self.stderr.take()
    }
}

/// Async process execution abstraction used by the agent engine.
///
/// All methods are async via RPITIT (return-position impl trait in trait),
/// so no `async-trait` macro is required. Implementors must be `Send + Sync`.
///
/// The `async_fn_in_trait` clippy lint is allowed because this trait is
/// intended for internal use within the runtime where the implementors
/// (`LocalProcess`, and future test doubles) produce `Send` futures via
/// `tokio`.
#[allow(async_fn_in_trait)]
pub trait OsProcess: Send + Sync {
    /// Execute a command with the given options.
    ///
    /// Returns a [`ChildProcess`] with piped stdin/stdout/stderr.
    async fn exec(
        &self,
        cmd: &str,
        args: &[&str],
        opts: &ExecOptions,
    ) -> anyhow::Result<ChildProcess>;

    /// Execute a command with additional environment variables merged on top
    /// of `opts.env`.
    ///
    /// This is a convenience method equivalent to cloning `opts`, merging
    /// `env` into `opts.env`, and calling [`OsProcess::exec`].
    async fn exec_with_env(
        &self,
        cmd: &str,
        args: &[&str],
        env: HashMap<String, String>,
        opts: &ExecOptions,
    ) -> anyhow::Result<ChildProcess>;
}

/// [`OsProcess`] implementation backed by the host system via
/// `tokio::process`.
#[derive(Debug, Default)]
pub struct LocalProcess;

// ---------------------------------------------------------------------------
// Spawn hygiene helpers (batch A2 / #182): every production `Command::new`
// must go through these, so Windows children never flash a console window
// and Unix children always lead their own process group.
// ---------------------------------------------------------------------------

/// The Windows `CREATE_NO_WINDOW` process-creation flag: the child runs
/// without a console window. Declared unconditionally so the value itself is
/// testable on every platform (guards against a typo'd literal).
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Apply the platform spawn hygiene to a std `Command`:
/// - Windows: `CREATE_NO_WINDOW`, so GUI hosts never flash a console.
/// - Unix: `process_group(0)`, so the child leads its own group and the
///   parent's signals (Ctrl+C) do not leak into it — mirroring the bash
///   tool's group semantics.
fn configure_spawn_hygiene(command: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // process_group(0) makes the child the leader of a new process
        // group whose PGID equals the child's PID.
        command.process_group(0);
    }
}

/// Create a pre-configured `std::process::Command`: Windows sets
/// `CREATE_NO_WINDOW`; Unix sets `process_group(0)`. The caller keeps the
/// native `Command` — chain `.args()`/`.env()`/`.current_dir()` freely.
pub fn new_command(program: &str) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    configure_spawn_hygiene(&mut command);
    command
}

/// The `tokio::process::Command` counterpart of [`new_command`] — identical
/// platform configuration, converted from the std command.
pub fn new_tokio_command(program: &str) -> tokio::process::Command {
    new_command(program).into()
}

impl LocalProcess {
    /// Create a new `LocalProcess`.
    pub fn new() -> Self {
        Self
    }

    /// Build a `tokio::process::Command` from the given command, arguments,
    /// and options.
    fn build_command(cmd: &str, args: &[&str], opts: &ExecOptions) -> tokio::process::Command {
        let mut command = new_tokio_command(cmd);
        command.args(args);

        if let Some(cwd) = &opts.cwd {
            command.current_dir(cwd);
        }

        // Merge environment variables on top of the parent environment.
        // We do NOT call env_clear() — the child inherits the parent env and
        // opts.env overrides individual entries.
        command.envs(&opts.env);

        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        command
    }
}

impl OsProcess for LocalProcess {
    async fn exec(
        &self,
        cmd: &str,
        args: &[&str],
        opts: &ExecOptions,
    ) -> anyhow::Result<ChildProcess> {
        let mut command = Self::build_command(cmd, args, opts);
        let mut child = command
            .spawn()
            .with_context(|| format!("failed to spawn command: {cmd}"))?;

        let pid = child
            .id()
            .ok_or_else(|| anyhow::anyhow!("spawned child has no PID"))?;

        // Capture the process group ID before we move the stdio handles out.
        // On Unix, process_group(0) sets the child's PGID to its own PID.
        #[cfg(unix)]
        let pgid: Option<i32> = Some(pid as i32);

        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        Ok(ChildProcess {
            pid,
            child: Some(child),
            stdin,
            stdout,
            stderr,
            #[cfg(unix)]
            pgid,
        })
    }

    async fn exec_with_env(
        &self,
        cmd: &str,
        args: &[&str],
        env: HashMap<String, String>,
        opts: &ExecOptions,
    ) -> anyhow::Result<ChildProcess> {
        let mut merged = opts.env.clone();
        for (key, value) in env {
            merged.insert(key, value);
        }
        let merged_opts = ExecOptions {
            cwd: opts.cwd.clone(),
            env: merged,
            timeout_ms: opts.timeout_ms,
        };
        // Disambiguate from `DynOsProcess::exec` (LocalProcess implements
        // both traits): this impl's `exec_with_env` delegates to its own
        // RPITIT `exec`.
        OsProcess::exec(self, cmd, args, &merged_opts).await
    }
}

/// Run a command to completion and collect its output.
///
/// This is a convenience function that spawns the command, reads stdout and
/// stderr fully, waits for exit, and returns `(exit_code, stdout, stderr)`.
///
/// Generic over the [`OsProcess`] implementation because RPITIT traits are
/// not directly dyn-safe.
pub async fn run_to_completion<P: OsProcess>(
    process: &P,
    cmd: &str,
    args: &[&str],
    opts: &ExecOptions,
) -> anyhow::Result<(i32, String, String)> {
    let mut child = process.exec(cmd, args, opts).await?;

    let mut stdout = String::new();
    let mut stderr = String::new();

    if let Some(out) = child.stdout() {
        use tokio::io::AsyncReadExt;
        out.read_to_string(&mut stdout)
            .await
            .context("failed to read child stdout")?;
    }
    if let Some(err) = child.stderr() {
        use tokio::io::AsyncReadExt;
        err.read_to_string(&mut stderr)
            .await
            .context("failed to read child stderr")?;
    }

    let code = child.wait().await?;

    Ok((code, stdout, stderr))
}

// ---------------------------------------------------------------------------
// Process identity probing and group termination (orphan governance E14 /
// ADR-012)
// ---------------------------------------------------------------------------

/// The process identity fingerprint persisted with a process-kind
/// background task (ADR-012 a): `pid + pgid + start time`. Start time makes
/// the identity unique under pid reuse — the OS never has two distinct
/// processes on the same pid at the same start instant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProcessFingerprint {
    pub pid: i32,
    pub pgid: i32,
    /// Raw system-clock start-time counter. The cross-platform semantics are
    /// defined by the reader: on macOS this is wall-clock nanoseconds since
    /// the Unix epoch at process start, on Linux the kernel `starttime`
    /// field (clock ticks since boot). Both are monotonically increasing in
    /// process start order, which is all identity comparison needs.
    pub start_ticks: u64,
}

/// Probe result for [`probe_process`].
#[derive(Debug)]
pub struct ProcessProbe {
    pub alive: bool,
    /// Raw monotonic start-time counter; `None` where the platform cannot
    /// read it (non-Unix) — callers degrade to "cannot determine".
    pub start_ticks: Option<u64>,
}

/// Probe whether `pid` is alive and, on Unix, its start time.
///
/// A zombie (exited but not yet reaped) is reported as not alive: it keeps
/// its `/proc` slot on Linux (and its group id), but it can neither run
/// nor be signaled, so callers must never treat it as a live target.
pub fn probe_process(pid: i32) -> std::io::Result<ProcessProbe> {
    if pid <= 0 {
        return Ok(ProcessProbe {
            alive: false,
            start_ticks: None,
        });
    }
    #[cfg(unix)]
    {
        read_unix_probe(pid)
    }
    #[cfg(windows)]
    {
        read_windows_probe(pid)
    }
    #[cfg(not(any(unix, windows)))]
    {
        Ok(ProcessProbe {
            alive: false,
            start_ticks: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Windows probe internals (#191). The Win32 calls live behind `#[cfg(windows)]`;
// everything derivable from plain data — the STILL_ACTIVE pin, the permission
// classification, and the start-ticks conversion — is a pure function compiled
// and unit-tested on every platform.
// ---------------------------------------------------------------------------

/// `STILL_ACTIVE`, the `GetExitCodeProcess` sentinel for "has not exited
/// yet" — pinned by its documented Win32 value so the `==` below (and its
/// off-Windows unit tests) cannot drift through a typo.
pub const STILL_ACTIVE: u32 = 259;

/// `ERROR_ACCESS_DENIED` (`winerror.h`), pinned by value: OpenProcess failing
/// with it means the process exists but this handle lacks the rights to open
/// it — semantically "alive but unattributable".
pub const ERROR_ACCESS_DENIED: u32 = 5;

/// Classify the exit code a `GetExitCodeProcess` call reported for an open
/// process handle: `STILL_ACTIVE` means the process has not exited yet; any
/// other value is the real exit code (the process is gone).
pub fn process_is_still_active(exit_code: u32) -> bool {
    exit_code == STILL_ACTIVE
}

/// Classify an `OpenProcess` failure: `ERROR_ACCESS_DENIED` means the process
/// exists but the caller lacks the rights to open it — semantically alive but
/// unattributable; every other error (e.g. `ERROR_INVALID_PARAMETER`, the
/// no-such-process answer) means the pid has no live process.
pub fn open_failure_is_access_denied(last_error: u32) -> bool {
    last_error == ERROR_ACCESS_DENIED
}

/// FILETIME half-words → start ticks, the Windows counterpart of the Unix
/// readers' contract: a monotonically increasing per-process identity value
/// that a fingerprint captured at spawn time and a later probe agree on.
///
/// The raw `GetProcessTimes` CreationTime (100 ns units since 1601-01-01) is
/// converted to nanoseconds since the 1970 epoch, matching the macOS
/// reader's `start_ticks` magnitude (wall-clock ns at process start): both
/// are then distinct across processes and stable for a given process, which
/// is all the orphan sweep's reuse detection compares. The conversion
/// (epoch shift then ×100 into ns) is exact, and u64 ns overflows only in
/// year 2554.
pub fn filetime_to_start_ticks(filetime: (u32, u32)) -> Option<u64> {
    let raw = (u64::from(filetime.1) << 32) | u64::from(filetime.0);
    // Windows-epoch (1601) → Unix-epoch (1970) delta, in 100 ns units.
    const EPOCH_DELTA: u64 = 116_444_736_000_000_000;
    let unix_100ns = raw.checked_sub(EPOCH_DELTA)?;
    Some(unix_100ns * 100)
}

/// Probe a pid on Windows (#191): `OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION)`
/// → `GetExitCodeProcess == STILL_ACTIVE`, with `GetProcessTimes` supplying
/// the CreationTime start ticks.
///
/// Error semantics, aligned with the Unix readers (both consumers — the ACP
/// desktop probe and the orphan sweep — read `Err` / not-alive as fail-closed
/// and never act on a pid they cannot attribute):
///
/// - `OpenProcess` with `ERROR_ACCESS_DENIED`: the process exists but this
///   handle cannot open it. Reported as alive with `start_ticks: None` —
///   the desktop probe proceeds to its healthz check (which fails closed on
///   its own for a stranger), and the sweep's `start_ticks_match` yields
///   `Unverifiable` → `Undeterminable`, so an unopenable process is never
///   terminated and its archive is never settled. Reporting it dead would
///   be the bug this task fixes (mistakenly settling Running archives).
/// - any other `OpenProcess` failure: not alive, like the Unix readers'
///   read failure.
/// - `GetExitCodeProcess` failure on an open handle: not alive (the handle
///   is valid, so the failure can only reflect a race the liveness answer
///   must resolve conservatively — the handle is about to be closed).
/// - `GetProcessTimes` failure: alive, no start ticks — the Unix readers
///   return liveness-only in the same shape on the other non-macOS/Linux
///   Unixes.
#[cfg(windows)]
fn read_windows_probe(pid: i32) -> std::io::Result<ProcessProbe> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    // SAFETY: plain Win32 call with a numeric pid; the returned handle is
    // null-checked before any use.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32) };
    if handle.is_null() {
        let error = std::io::Error::last_os_error();
        if open_failure_is_access_denied(error.raw_os_error().unwrap_or(0) as u32) {
            return Ok(ProcessProbe {
                alive: true,
                start_ticks: None,
            });
        }
        return Ok(ProcessProbe {
            alive: false,
            start_ticks: None,
        });
    }

    // SAFETY: `handle` is a valid process handle owned by this function.
    let mut exit_code: u32 = 0;
    let exit_ok = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
    // SAFETY: same valid handle.
    let mut creation = windows_sys::Win32::Foundation::FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut _exit_time = windows_sys::Win32::Foundation::FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut _kernel = windows_sys::Win32::Foundation::FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut _user = windows_sys::Win32::Foundation::FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let times_ok = unsafe {
        GetProcessTimes(
            handle,
            &mut creation,
            &mut _exit_time,
            &mut _kernel,
            &mut _user,
        )
    };
    // SAFETY: we own `handle` and will not use it again.
    unsafe { CloseHandle(handle) };

    if exit_ok == 0 || !process_is_still_active(exit_code) {
        return Ok(ProcessProbe {
            alive: false,
            start_ticks: None,
        });
    }
    let start_ticks = if times_ok != 0 {
        filetime_to_start_ticks((creation.dwLowDateTime, creation.dwHighDateTime))
    } else {
        None
    };
    Ok(ProcessProbe {
        alive: true,
        start_ticks,
    })
}

#[cfg(unix)]
fn read_unix_probe(pid: i32) -> std::io::Result<ProcessProbe> {
    #[cfg(target_os = "macos")]
    {
        use libproc::libproc::bsd_info::BSDInfo;
        use libproc::proc_pid::pidinfo;
        match pidinfo::<BSDInfo>(pid, 0) {
            Ok(info) => {
                let ticks = info.pbi_start_tvsec * 1_000_000_000 + info.pbi_start_tvusec * 1_000;
                Ok(ProcessProbe {
                    alive: true,
                    start_ticks: Some(ticks),
                })
            }
            Err(_) => Ok(ProcessProbe {
                alive: false,
                start_ticks: None,
            }),
        }
    }
    #[cfg(target_os = "linux")]
    {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => {
                // field 22 (1-based) sits after the comm field "(...)" —
                // split from the last ')' to avoid spaces inside comm. After
                // ')' the fields restart at 3 (state), so starttime is
                // index 19 (22 - 3).
                let after_comm = stat.rsplit_once(')').map(|(_, rest)| rest);
                let state = after_comm.and_then(|rest| rest.split_whitespace().next());
                // A zombie (Z) or dead (X) entry still occupies its /proc
                // slot until the parent reaps it, but it can neither run
                // nor be signaled: probe it as not alive so an orphan sweep
                // (and its group re-verification) never waits on a corpse.
                if matches!(state, Some("Z" | "X" | "x")) {
                    return Ok(ProcessProbe {
                        alive: false,
                        start_ticks: None,
                    });
                }
                let field22 = after_comm
                    .and_then(|rest| rest.split_whitespace().nth(19))
                    .and_then(|v| v.parse::<u64>().ok());
                Ok(ProcessProbe {
                    alive: true,
                    start_ticks: field22,
                })
            }
            Err(_) => Ok(ProcessProbe {
                alive: false,
                start_ticks: None,
            }),
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        // kill(pid, 0) liveness only — no start time on other Unixes
        let alive = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGNULL,
        )
        .is_ok();
        Ok(ProcessProbe {
            alive,
            start_ticks: None,
        })
    }
}

/// Enumerate live process ids whose process group is `pgid` (Unix; empty
/// vec elsewhere or for the non-positive sentinel). Zombies are excluded:
/// they still carry the group id on Linux but are not live members. Used
/// by the orphan sweep's group-residue branch and the pre-termination
/// re-verification.
///
/// The non-Unix empty vec is deliberate (issue #195): "no process-group
/// concept" means "no members", which is the truth, and callers
/// (`orphan_sweep.rs`) map both `Ok(vec![])` and errors to the same
/// "nothing to terminate" outcome — so unlike `terminate_group`, this
/// stays `Ok` rather than failing with Unsupported.
pub fn list_group_members(pgid: i32) -> std::io::Result<Vec<i32>> {
    if pgid <= 0 {
        return Ok(Vec::new());
    }
    #[cfg(unix)]
    {
        let mut members = Vec::new();
        // macOS has no /proc — iterate via libproc's pid list, then check
        // each candidate's pgid with a single getpgid call per pid.
        #[cfg(target_os = "macos")]
        {
            use libproc::processes::{ProcFilter, pids_by_type};
            let pids = match pids_by_type(ProcFilter::All) {
                Ok(pids) => pids,
                Err(err) => {
                    tracing::debug!(
                        %err,
                        "failed to enumerate process table for group membership, falling back to empty list"
                    );
                    Vec::new()
                }
            };
            for pid in pids {
                let pid = pid as i32;
                if pid <= 0 {
                    continue;
                }
                if nix::unistd::getpgid(Some(nix::unistd::Pid::from_raw(pid)))
                    .is_ok_and(|group| group.as_raw() == pgid)
                {
                    members.push(pid);
                }
            }
        }
        #[cfg(target_os = "linux")]
        {
            for entry in std::fs::read_dir("/proc")? {
                let entry = entry?;
                let name = entry.file_name();
                let Some(pid) = name.to_str().and_then(|s| s.parse::<i32>().ok()) else {
                    continue;
                };
                let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
                    continue;
                };
                // field 5 (1-based) = pgrp, after the comm "(...)". After
                // ')' the fields restart at 3 (state), so pgrp is index 2
                // (5 - 3).
                let Some((_, rest)) = stat.rsplit_once(')') else {
                    continue;
                };
                let mut fields = rest.split_whitespace();
                // Zombies/dead entries keep the group id in /proc until
                // reaped, but they are not live members: skip them so
                // termination waits and group-residue verdicts never hang
                // on a corpse.
                if matches!(fields.next(), Some("Z" | "X" | "x")) {
                    continue;
                }
                if let Some(pgrp) = fields.nth(1)
                    && pgrp.parse::<i32>() == Ok(pgid)
                {
                    members.push(pid);
                }
            }
        }
        Ok(members)
    }
    #[cfg(not(unix))]
    {
        Ok(Vec::new())
    }
}

/// Terminate a process group by its bare pgid, with the ADR-012 e
/// re-verification and the two-tier escalation (SIGTERM → grace → SIGKILL).
/// `pgid <= 0` is the trust-boundary sentinel: signal nothing, return Ok.
/// Only macOS/Linux actually signal; other platforms fail with
/// `ErrorKind::Unsupported` rather than pretending success.
pub fn terminate_group(pgid: i32) -> std::io::Result<()> {
    if pgid <= 0 {
        tracing::warn!(pgid, "refusing to signal non-positive process group");
        return Ok(());
    }
    #[cfg(unix)]
    {
        // Re-verify: the group must still have live members.
        if list_group_members(pgid)?.is_empty() {
            return Ok(()); // already dead — ESRCH-equivalent
        }
        let nix_pgid = nix::unistd::Pid::from_raw(pgid);
        let _ = nix::sys::signal::killpg(nix_pgid, nix::sys::signal::Signal::SIGTERM);
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(100));
            if list_group_members(pgid)?.is_empty() {
                return Ok(());
            }
        }
        let _ = nix::sys::signal::killpg(nix_pgid, nix::sys::signal::Signal::SIGKILL);
        Ok(())
    }
    #[cfg(not(unix))]
    {
        // No process-group signal on non-Unix — fail loudly (issue #195):
        // callers see the Unsupported error instead of a silent no-op that
        // would leave an orphaned group running while reporting success.
        Err(unsupported_platform_error("terminate_group"))
    }
}

/// Build the shared "this operation has no implementation on this platform"
/// error. Used by the non-Unix branches that must fail honestly instead of
/// silently succeeding (issue #195). Also compiled under `cfg(test)` so the
/// pure-function test below can pin its contract on every platform.
#[cfg(any(not(unix), test))]
pub(crate) fn unsupported_platform_error(op: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        format!("{op}: no process-group signaling on this platform"),
    )
}

// ---------------------------------------------------------------------------
// Object-safe (dyn-compatible) process abstractions (zero-IO T13a)
// ---------------------------------------------------------------------------

/// A boxed, `Send` future as returned by the object-safe [`DynOsProcess`]
/// and [`DynChildProcess`] traits.
///
/// The [`OsProcess`] trait itself uses RPITIT (`async fn` in trait), which is
/// not dyn-compatible; the `Dyn*` traits erase the same surface behind boxed
/// futures so providers can be injected as `Arc<dyn DynOsProcess>`.
pub type DynProcessFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A boxed async reader, as produced by [`DynChildProcess::take_stdout`] and
/// [`DynChildProcess::take_stderr`].
pub type DynChildStream = Box<dyn tokio::io::AsyncRead + Send + Unpin>;

/// A boxed async writer, as produced by [`DynChildProcess::take_stdin_writer`].
pub type DynChildStdin = Box<dyn tokio::io::AsyncWrite + Send + Unpin>;

/// Object-safe (dyn-compatible) mirror of a running child process.
///
/// This is the erased view of [`ChildProcess`] returned by
/// [`DynOsProcess::exec`]. The default bodies let test doubles implement
/// only the surface they exercise; [`ChildProcess`] overrides all of them.
pub trait DynChildProcess: Send {
    /// Process ID of the child as reported by the OS. Defaults to `0`.
    fn pid(&self) -> u32 {
        0
    }

    /// The process group ID assigned to the child, when one was set
    /// (`None` on non-Unix platforms or for doubles that model no group).
    /// The bash tool reads it right after spawn to build the process
    /// identity fingerprint (ADR-012 a).
    fn pgid(&self) -> Option<i32> {
        None
    }

    /// Take (and thereby close) the child's stdin so interactive commands
    /// see EOF instead of hanging. The default is a no-op.
    fn take_stdin(&mut self) {}

    /// Take ownership of the child's stdin **writer**, if piped, so callers
    /// can pipe input into the child while it runs (the hook runner's use
    /// case). Unlike [`DynChildProcess::take_stdin`] (which just closes the
    /// pipe), this hands the writing end back to the caller. The default is
    /// `None`.
    fn take_stdin_writer(&mut self) -> Option<DynChildStdin> {
        None
    }

    /// Take ownership of the child's stdout stream, if piped.
    fn take_stdout(&mut self) -> Option<DynChildStream> {
        None
    }

    /// Take ownership of the child's stderr stream, if piped.
    fn take_stderr(&mut self) -> Option<DynChildStream> {
        None
    }

    /// Wait for the child to exit and return the exit code.
    fn wait(&mut self) -> DynProcessFuture<'_, anyhow::Result<i32>>;

    /// Kill the child process (and its process group on Unix), then wait for
    /// it to actually exit.
    fn kill(&mut self) -> DynProcessFuture<'_, anyhow::Result<()>>;
}

impl DynChildProcess for ChildProcess {
    fn pid(&self) -> u32 {
        ChildProcess::pid(self)
    }

    fn pgid(&self) -> Option<i32> {
        ChildProcess::pgid(self)
    }

    fn take_stdin(&mut self) {
        let _ = ChildProcess::take_stdin(self);
    }

    fn take_stdin_writer(&mut self) -> Option<DynChildStdin> {
        ChildProcess::take_stdin(self).map(|writer| Box::new(writer) as DynChildStdin)
    }

    fn take_stdout(&mut self) -> Option<DynChildStream> {
        ChildProcess::take_stdout(self).map(|stream| Box::new(stream) as DynChildStream)
    }

    fn take_stderr(&mut self) -> Option<DynChildStream> {
        ChildProcess::take_stderr(self).map(|stream| Box::new(stream) as DynChildStream)
    }

    fn wait(&mut self) -> DynProcessFuture<'_, anyhow::Result<i32>> {
        Box::pin(ChildProcess::wait(self))
    }

    fn kill(&mut self) -> DynProcessFuture<'_, anyhow::Result<()>> {
        Box::pin(ChildProcess::kill(self))
    }
}

/// Object-safe (dyn-compatible) mirror of [`OsProcess`].
///
/// The RPITIT trait cannot be used as `dyn OsProcess`, so provider
/// injection (zero-IO T13a) goes through this erased twin. [`LocalProcess`]
/// implements both; test doubles implement only this trait.
pub trait DynOsProcess: Send + Sync {
    /// Execute a command with the given options.
    fn exec<'a>(
        &'a self,
        cmd: &'a str,
        args: &'a [&'a str],
        opts: &'a ExecOptions,
    ) -> DynProcessFuture<'a, anyhow::Result<Box<dyn DynChildProcess>>>;

    /// Execute a command with additional environment variables merged on top
    /// of `opts.env`.
    ///
    /// The default merges `env` into a clone of `opts` and delegates to
    /// [`DynOsProcess::exec`] (the same semantics [`OsProcess::exec_with_env`]
    /// gives its implementors).
    fn exec_with_env<'a>(
        &'a self,
        cmd: &'a str,
        args: &'a [&'a str],
        env: HashMap<String, String>,
        opts: &'a ExecOptions,
    ) -> DynProcessFuture<'a, anyhow::Result<Box<dyn DynChildProcess>>> {
        Box::pin(async move {
            let mut merged = opts.env.clone();
            for (key, value) in env {
                merged.insert(key, value);
            }
            let merged_opts = ExecOptions {
                cwd: opts.cwd.clone(),
                env: merged,
                timeout_ms: opts.timeout_ms,
            };
            self.exec(cmd, args, &merged_opts).await
        })
    }
}

/// [`DynOsProcess`] implementation delegating to [`LocalProcess`]'s
/// [`OsProcess`] methods (the RPITIT futures are `Send`, so they box
/// directly).
impl DynOsProcess for LocalProcess {
    fn exec<'a>(
        &'a self,
        cmd: &'a str,
        args: &'a [&'a str],
        opts: &'a ExecOptions,
    ) -> DynProcessFuture<'a, anyhow::Result<Box<dyn DynChildProcess>>> {
        Box::pin(async move {
            <Self as OsProcess>::exec(self, cmd, args, opts)
                .await
                .map(|child| Box::new(child) as Box<dyn DynChildProcess>)
        })
    }

    fn exec_with_env<'a>(
        &'a self,
        cmd: &'a str,
        args: &'a [&'a str],
        env: HashMap<String, String>,
        opts: &'a ExecOptions,
    ) -> DynProcessFuture<'a, anyhow::Result<Box<dyn DynChildProcess>>> {
        Box::pin(async move {
            <Self as OsProcess>::exec_with_env(self, cmd, args, env, opts)
                .await
                .map(|child| Box::new(child) as Box<dyn DynChildProcess>)
        })
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::process::CommandExt;

    /// The flag value is a Windows ABI constant — pinned by the documented
    /// value of CREATE_NO_WINDOW, on every platform.
    #[test]
    fn should_pin_the_create_no_window_flag_value_when_declared() {
        assert_eq!(super::CREATE_NO_WINDOW, 0x0800_0000);
    }

    /// Given a command built by `new_command`, when the child is spawned on
    /// Unix, then it leads its own process group (PGID == PID).
    #[test]
    #[cfg(unix)]
    fn should_spawn_as_its_own_group_leader_when_created_via_new_command() {
        let mut child = super::new_command("sleep")
            .arg("0.1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let pid = child.id() as i32;
        let pgid = nix::unistd::getpgid(Some(nix::unistd::Pid::from_raw(pid)))
            .expect("getpgid of the child")
            .as_raw();
        assert_eq!(pgid, pid, "the child must lead its own process group");
        let _ = child.wait();
    }

    /// Given a command built by `new_command`, when the child has already
    /// exited, then the group has no live members (the leader exited as its
    /// group's only member at that point).
    #[test]
    #[cfg(unix)]
    fn should_place_the_child_in_a_group_disjoint_from_the_parent_when_spawned() {
        let mut child = super::new_command("true").spawn().expect("spawn true");
        let pid = child.id() as i32;
        let _ = child.wait();
        let parent_pgid = nix::unistd::getpgid(None).expect("parent pgid").as_raw();
        assert_ne!(
            pid, parent_pgid,
            "the child group must be disjoint from the parent's group"
        );
    }

    #[tokio::test]
    async fn should_report_the_own_group_as_pgid_when_a_child_is_spawned() {
        use super::{ChildProcess, DynChildProcess, LocalProcess, OsProcess};

        let process = LocalProcess::new();
        let opts = super::ExecOptions {
            cwd: None,
            env: std::collections::HashMap::new(),
            timeout_ms: None,
        };
        let mut child: ChildProcess = OsProcess::exec(&process, "true", &[], &opts)
            .await
            .expect("spawn true");

        // On Unix the spawned child leads its own group (process_group(0)),
        // so the pgid equals the pid; the erased view must agree with the
        // concrete accessor.
        let dyn_child = &child as &dyn DynChildProcess;
        #[cfg(unix)]
        {
            let pid = child.pid() as i32;
            assert_eq!(
                child.pgid(),
                Some(pid),
                "the group leader's pgid is its pid"
            );
            assert_eq!(dyn_child.pgid(), Some(pid));
        }
        #[cfg(not(unix))]
        {
            assert_eq!(child.pgid(), None);
            assert_eq!(dyn_child.pgid(), None);
        }
        let _ = child.wait().await;
    }

    /// Non-Unix probes historically stubbed to not-alive; now that Windows
    /// has a real implementation, this liveness expectation holds wherever
    /// a real probe exists — but on other non-Unix platforms it is still a
    /// stub, so the test stays Unix-gated.
    #[test]
    #[cfg(any(unix, windows))]
    fn should_report_a_live_process_as_alive_when_probed_by_pid() {
        // Given the current process
        // When probed by its own pid
        // Then the probe reports alive
        let pid = std::process::id() as i32;
        let probe = super::probe_process(pid).expect("probing own pid must succeed");
        assert!(probe.alive, "the current process must probe alive");
        // On Windows the probe also reads the CreationTime start ticks, so
        // the fingerprint identity comparison has data to work with.
        if cfg!(windows) {
            assert!(
                probe.start_ticks.is_some(),
                "the own process's creation time must be readable on Windows"
            );
        }
    }

    #[test]
    fn should_report_a_dead_pid_as_not_alive_when_probed() {
        // spawn a short-lived child, wait it out, then probe its pid slot
        let mut child = std::process::Command::new("true") // hygiene-allow: D: spawn+wait 后探活，无时间依赖
            .spawn()
            .expect("spawn true");
        let pid = child.id() as i32;
        let _ = child.wait();
        let probe = super::probe_process(pid).expect("probe must not error");
        assert!(!probe.alive, "exited process must not probe alive");
    }

    /// A killed but unreaped child lingers as a zombie; it can neither run
    /// nor be signaled, so it must probe as not alive and must not count as
    /// a live group member. On Linux it keeps a readable `/proc` slot (and
    /// its group id) until reaped, which is what this guards against.
    #[test]
    #[cfg(unix)]
    fn should_report_a_zombie_child_as_not_alive_when_probed() {
        // Given a group-leading child killed but not yet reaped by `wait`
        let mut child = std::process::Command::new("sleep")
            .arg("60")
            .process_group(0)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn zombie-to-be");
        let pid = child.id() as i32;
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGKILL,
        )
        .expect("kill the test child");

        // The child is never reaped here, so once the signal lands it can
        // only be a zombie: poll the probe (rather than `try_wait`, which
        // would reap it) for the liveness flip.
        let mut probe = super::probe_process(pid).expect("probe must not error");
        for _ in 0..100 {
            if !probe.alive {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            probe = super::probe_process(pid).expect("probe must not error");
        }

        // Then the zombie is not alive and is not a live group member
        assert!(!probe.alive, "a zombie must not probe alive");
        assert!(
            !super::list_group_members(pid)
                .expect("enumerate group")
                .contains(&pid),
            "a zombie must not be listed as a live group member"
        );

        let _ = child.wait();
    }

    #[test]
    fn should_round_trip_a_fingerprint_through_serde_when_persisted() {
        let fp = super::ProcessFingerprint {
            pid: 4242,
            pgid: 4242,
            start_ticks: 123456789,
        };
        let json = serde_json::to_string(&fp).unwrap();
        let back: super::ProcessFingerprint = serde_json::from_str(&json).unwrap();
        assert_eq!(fp, back);
    }

    #[test]
    fn should_report_a_nonpositive_pid_as_not_alive_when_probed() {
        for pid in [0, -1] {
            let probe = super::probe_process(pid).expect("sentinel probe must not error");
            assert!(!probe.alive, "pid {pid} must not probe alive");
            assert!(
                probe.start_ticks.is_none(),
                "pid {pid} must carry no start ticks"
            );
        }

        // The Windows probe's pure helpers, pinned on every platform (the
        // Win32 calls are Windows-only, but their logic constants are data).
        // STILL_ACTIVE is the documented GetExitCodeProcess sentinel for a
        // not-yet-exited process; only it means alive.
        assert!(super::process_is_still_active(super::STILL_ACTIVE));
        assert!(!super::process_is_still_active(0));
        assert!(!super::process_is_still_active(258));
        assert!(!super::process_is_still_active(260));
        // ERROR_ACCESS_DENIED is the documented OpenProcess "exists but not
        // ours to open" error; every other error code reads as no-such-pid.
        assert!(super::open_failure_is_access_denied(super::ERROR_ACCESS_DENIED));
        assert!(!super::open_failure_is_access_denied(87)); // ERROR_INVALID_PARAMETER
        // The FILETIME → start-ticks conversion must be exact and must map
        // the epoch origins the way the doc comment pins: 100 ns units
        // since 1601 in, nanoseconds since 1970 out. 0x19db1de_d53e8000 is
        // the FILETIME of the 1970 epoch itself.
        assert_eq!(super::filetime_to_start_ticks((0xd53e_8000, 0x19d_b1de)), Some(0));
        assert_eq!(
            super::filetime_to_start_ticks((0, 0)),
            None,
            "100 ns before the 1970 epoch must not underflow into a huge tick"
        );
        // One second past the epoch: the epoch FILETIME + 10_000_000 × 100 ns.
        assert_eq!(
            super::filetime_to_start_ticks((0xd5d7_1680, 0x19d_b1de)),
            Some(1_000_000_000)
        );
        // 2020-01-01T00:00:00Z = 15_778_368_00 s since the Unix epoch.
        assert_eq!(
            super::filetime_to_start_ticks((0x6905_0000, 0x1d5_c036)),
            Some(1_577_836_800_000_000_000)
        );
    }

    #[test]
    fn should_return_an_empty_member_list_for_a_nonpositive_group_when_enumerated() {
        let members = super::list_group_members(0).expect("sentinel enumeration must not error");
        assert!(members.is_empty(), "pgid 0 must enumerate no members");
    }

    #[test]
    #[cfg(unix)]
    fn should_list_the_own_process_group_when_members_enumerated() {
        let pgid = nix::unistd::getpgid(None).expect("getpgid").as_raw();
        let members = super::list_group_members(pgid).expect("enumerate group");
        assert!(
            members.contains(&(std::process::id() as i32)),
            "own pid must be in own group"
        );
    }

    #[test]
    fn should_refuse_to_signal_a_nonpositive_group_when_terminating() {
        // The sentinel: a polluted archive could carry pgid<=0 — never signal.
        assert!(super::terminate_group(0).is_ok());
        assert!(super::terminate_group(-1).is_ok());
        // and neither may have signaled anything (no crash, no panic)
    }

    #[test]
    fn should_build_an_unsupported_error_naming_the_operation() {
        let err = super::unsupported_platform_error("terminate_group");
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::Unsupported,
            "the error must carry ErrorKind::Unsupported"
        );
        assert!(
            err.to_string().contains("terminate_group"),
            "the message must name the operation, got: {err}"
        );
    }

    #[test]
    #[cfg(not(unix))]
    fn should_return_an_unsupported_error_when_terminating_a_group_on_non_unix() {
        // HONEST LIMITATION: this branch only compiles/runs on non-Unix
        // targets (Windows). The repo's CI is ubuntu-only, so this test is
        // never executed there — real coverage requires a Windows machine
        // (or a windows cross `cargo test`, which needs a full MSVC
        // runtime). The pure-function test above pins the shared logic on
        // every platform; only the `#[cfg]` dispatch below is unexercised
        // by CI.
        let err = super::terminate_group(12345).expect_err("non-Unix must fail, not no-op");
        assert_eq!(err.kind(), std::io::ErrorKind::Unsupported);
    }

    #[test]
    #[cfg(unix)]
    fn should_terminate_an_empty_or_dead_group_when_terminating() {
        // A group whose leader exited and has no members: ESRCH path → Ok.
        let mut child = std::process::Command::new("true") // hygiene-allow: D: spawn+wait，ESRCH 路径确定性
            .process_group(0) // std::os::unix::process::CommandExt
            .spawn()
            .expect("spawn group leader");
        let pgid = child.id() as i32;
        let _ = child.wait();
        assert!(super::terminate_group(pgid).is_ok());
    }
}
