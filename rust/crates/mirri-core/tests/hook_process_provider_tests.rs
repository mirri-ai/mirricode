//! T-13c3-2 / T-13c3-3 — hook runner behaviour through a fake
//! [`DynOsProcess`] provider.
//!
//! These tests prove the hook runner is genuinely provider-injected: the
//! exit-code semantic table (0 → Pass, 2 → Block, other → Pass), the env
//! merge, the stdin JSON piping, and the timeout kill branch are all
//! exercised without any real child process (zero-IO T13c-3).

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use mirri_core::hook_engine::{HookBlockSource, HookDef, HookEventType, HookOutcome, run_hook};
use mirri_os::{
    DynChildProcess, DynChildStdin, DynChildStream, DynOsProcess, DynProcessFuture, ExecOptions,
};
use tokio::io::AsyncWrite;

// ---------------------------------------------------------------------------
// Fakes
// ---------------------------------------------------------------------------

/// An async writer that records everything written into a shared buffer
/// (the fake child's stdin pipe).
#[derive(Clone)]
struct StdinRecorder {
    buffer: Arc<StdMutex<Vec<u8>>>,
}

impl StdinRecorder {
    fn new() -> Self {
        Self {
            buffer: Arc::new(StdMutex::new(Vec::new())),
        }
    }
}

impl AsyncWrite for StdinRecorder {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        self.buffer.lock().unwrap().extend_from_slice(buf);
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// A scripted child double: fixed stdout/stderr, a recorded exit code, and
/// a flag that flips when `kill` is invoked.
struct ScriptedHookChild {
    exit_code: i32,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdin: Option<StdinRecorder>,
    killed: Arc<AtomicBool>,
}

impl ScriptedHookChild {
    fn new(exit_code: i32, stdout: &str, stderr: &str) -> Self {
        Self {
            exit_code,
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
            stdin: Some(StdinRecorder::new()),
            killed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// A handle onto the stdin recorder (valid before the runner takes it).
    fn stdin_recorder(&self) -> StdinRecorder {
        self.stdin.clone().expect("stdin not yet taken")
    }
}

impl DynChildProcess for ScriptedHookChild {
    fn take_stdin_writer(&mut self) -> Option<DynChildStdin> {
        self.stdin
            .take()
            .map(|recorder| Box::new(recorder) as DynChildStdin)
    }

    fn take_stdout(&mut self) -> Option<DynChildStream> {
        Some(Box::new(std::io::Cursor::new(std::mem::take(
            &mut self.stdout,
        ))))
    }

    fn take_stderr(&mut self) -> Option<DynChildStream> {
        Some(Box::new(std::io::Cursor::new(std::mem::take(
            &mut self.stderr,
        ))))
    }

    fn wait(&mut self) -> DynProcessFuture<'_, anyhow::Result<i32>> {
        let code = self.exit_code;
        Box::pin(async move {
            // A tiny delay so the runner's concurrent IO tasks get scheduled.
            tokio::time::sleep(Duration::from_millis(10)).await;
            Ok(code)
        })
    }

    fn kill(&mut self) -> DynProcessFuture<'_, anyhow::Result<()>> {
        self.killed.store(true, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

/// A slow child double: `wait` never resolves until `kill` flips the flag —
/// the shape a child sleeping past the hook timeout has from the runner's
/// perspective.
struct SlowHookChild {
    killed: Arc<AtomicBool>,
    killed_handle: Arc<AtomicBool>,
}

impl SlowHookChild {
    fn new() -> Self {
        let killed = Arc::new(AtomicBool::new(false));
        Self {
            killed_handle: Arc::clone(&killed),
            killed,
        }
    }
}

impl DynChildProcess for SlowHookChild {
    fn wait(&mut self) -> DynProcessFuture<'_, anyhow::Result<i32>> {
        let killed = Arc::clone(&self.killed_handle);
        Box::pin(async move {
            while !killed.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            // A killed child reports signal termination — the erased trait's
            // convention for "no meaningful exit code".
            Ok(-1)
        })
    }

    fn kill(&mut self) -> DynProcessFuture<'_, anyhow::Result<()>> {
        self.killed.store(true, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

/// A recorded exec invocation: (program, args, merged env).
type RecordedExec = (String, Vec<String>, HashMap<String, String>);

/// A scripted process provider double: hands out one pre-built child per
/// `exec` and records every exec invocation.
struct ScriptedHookProcess {
    children: StdMutex<Vec<Box<dyn DynChildProcess>>>,
    execs: StdMutex<Vec<RecordedExec>>,
}

impl ScriptedHookProcess {
    fn with_child(child: Box<dyn DynChildProcess>) -> Self {
        Self {
            children: StdMutex::new(vec![child]),
            execs: StdMutex::new(Vec::new()),
        }
    }

    fn recorded_execs(&self) -> Vec<RecordedExec> {
        self.execs.lock().unwrap().clone()
    }
}

impl DynOsProcess for ScriptedHookProcess {
    fn exec<'a>(
        &'a self,
        cmd: &'a str,
        args: &'a [&'a str],
        opts: &'a ExecOptions,
    ) -> DynProcessFuture<'a, anyhow::Result<Box<dyn DynChildProcess>>> {
        Box::pin(async move {
            self.execs.lock().unwrap().push((
                cmd.to_string(),
                args.iter().map(|a| a.to_string()).collect(),
                opts.env.clone(),
            ));
            self.children
                .lock()
                .unwrap()
                .pop()
                .ok_or_else(|| anyhow::anyhow!("no scripted child available"))
        })
    }
}

fn hook_with(command: &str) -> HookDef {
    HookDef {
        event: HookEventType::PreToolUse,
        command: command.to_string(),
        pattern: None,
        timeout_ms: 5_000,
        env: None,
        cwd: None,
        fail_closed: false,
    }
}

// ---------------------------------------------------------------------------
// T-13c3-2 — exit-code semantic table through a fake provider
// ---------------------------------------------------------------------------

/// Given a fake child exiting 2 with a stderr explanation, when the hook
/// runs, then the outcome is a Block carrying stderr as both the reason and
/// the display message (exit-code semantic table unchanged, T-13c3-2).
#[tokio::test]
async fn should_block_with_stderr_reason_when_fake_child_exits_two() {
    // Given a fake provider whose child exits 2 with stderr
    let child = ScriptedHookChild::new(2, "", "blocked by policy");
    let stdin_probe = child.stdin_recorder();
    let process = ScriptedHookProcess::with_child(Box::new(child));

    let mut hook = hook_with("some-hook-command");
    hook.env = Some(HashMap::from([(
        "HOOK_VAR".to_string(),
        "from-hook".to_string(),
    )]));

    // When the hook runs with extra env layered on top
    let extra = HashMap::from([
        ("EXTRA_VAR".to_string(), "from-extra".to_string()),
        ("HOOK_VAR".to_string(), "extra-wins".to_string()),
    ]);
    let outcome = run_hook(&process, &hook, &serde_json::json!({"k": "v"}), &extra).await;

    // Then the outcome blocks per the exit-2 row of the semantic table
    match outcome {
        HookOutcome::Block {
            reason,
            message,
            source,
            ..
        } => {
            assert!(
                matches!(source, HookBlockSource::ExitCode2),
                "exit 2 → ExitCode2 source"
            );
            assert_eq!(reason, "blocked by policy");
            assert_eq!(message.as_deref(), Some("blocked by policy"));
        }
        other => panic!("expected Block, got {other:?}"),
    }

    // And the exec reached the provider with the shell command and the
    // merged env (process env < hook env < extra env — extra wins).
    let execs = process.recorded_execs();
    assert_eq!(execs.len(), 1, "exactly one exec should be recorded");
    assert_eq!(execs[0].0, "/bin/sh", "hook commands run via /bin/sh");
    assert_eq!(
        execs[0].1,
        vec!["-c".to_string(), "some-hook-command".to_string()],
        "the hook command is the -c payload"
    );
    assert_eq!(
        execs[0].2.get("HOOK_VAR").map(String::as_str),
        Some("extra-wins"),
        "extra_env should override hook env"
    );
    assert_eq!(
        execs[0].2.get("EXTRA_VAR").map(String::as_str),
        Some("from-extra"),
        "extra_env should pass through"
    );

    // And the JSON input was piped to the child's stdin.
    let written = stdin_probe.buffer.lock().unwrap().clone();
    let text = String::from_utf8_lossy(&written);
    assert!(
        text.contains("\"k\"") && text.contains("\"v\""),
        "stdin should carry the serialized JSON input, got {text:?}"
    );
}

/// Given fake children exiting 0 and 1, when the hooks run, then both
/// outcomes are non-blocking Passes carrying the captured stdout (the
/// non-2 rows of the exit-code semantic table, T-13c3-2).
#[tokio::test]
async fn should_pass_when_fake_child_exits_zero_or_other_non_two() {
    for (code, expect_stdout) in [(0, true), (1, false)] {
        // Given a fake provider whose child exits `code`
        let child = ScriptedHookChild::new(code, "captured output", "");
        let process = ScriptedHookProcess::with_child(Box::new(child));

        // When the hook runs
        let outcome = run_hook(
            &process,
            &hook_with("some-hook-command"),
            &serde_json::json!({}),
            &HashMap::new(),
        )
        .await;

        // Then the outcome is a Pass whose result carries the captured
        // stdout and the exit code
        match outcome {
            HookOutcome::Pass(result) => {
                assert_eq!(result.exit_code, Some(code), "exit code {code} passes");
                if expect_stdout {
                    assert_eq!(
                        result.stdout, "captured output",
                        "stdout capture should survive the provider hop"
                    );
                }
            }
            other => panic!("expected Pass for exit {code}, got {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// T-13c3-3 — timeout kill branch through a fake slow child
// ---------------------------------------------------------------------------

/// Given a fake child whose wait never resolves until killed, when the
/// hook's timeout elapses, then the child is killed and the failure follows
/// the fail-closed rule (timeout branch unchanged, T-13c3-3).
#[tokio::test]
async fn should_kill_the_slow_child_when_hook_times_out() {
    // Given a slow fake child and a fail-open hook with a 100ms timeout
    let child = SlowHookChild::new();
    let killed_after_run = child.killed_handle.clone();
    let process = ScriptedHookProcess::with_child(Box::new(child));
    let mut hook = hook_with("sleep forever");
    hook.timeout_ms = 100;

    // When the hook runs against the never-exiting child
    let started = std::time::Instant::now();
    let outcome = run_hook(&process, &hook, &serde_json::json!({}), &HashMap::new()).await;

    // Then the outcome is the timeout error…
    match outcome {
        HookOutcome::Error(message) => assert!(
            message.contains("timed out"),
            "expected a timeout message, got {message}"
        ),
        other => panic!("expected Error (timeout), got {other:?}"),
    }
    // …the child was killed (the M13c3-1 assertion: no kill → red)…
    assert!(
        killed_after_run.load(Ordering::SeqCst),
        "the timed-out child must be killed"
    );
    // …and the runner returned instead of hanging on the child's wait.
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the timeout branch must reap the child, took {:?}",
        started.elapsed()
    );
}

/// Given the same slow child but a fail-closed hook, when the timeout
/// elapses, then the execution failure blocks (the fail-closed row of the
/// timeout branch, T-13c3-3).
#[tokio::test]
async fn should_block_with_execution_failure_when_slow_hook_times_out_fail_closed() {
    // Given a slow fake child and a fail-closed hook with a 100ms timeout
    let child = SlowHookChild::new();
    let process = ScriptedHookProcess::with_child(Box::new(child));
    let mut hook = hook_with("sleep forever");
    hook.timeout_ms = 100;
    hook.fail_closed = true;

    // When the hook runs
    let outcome = run_hook(&process, &hook, &serde_json::json!({}), &HashMap::new()).await;

    // Then the timeout blocks with the ExecutionFailure source
    match outcome {
        HookOutcome::Block { reason, source, .. } => {
            assert!(
                matches!(source, HookBlockSource::ExecutionFailure),
                "timeout + fail_closed → ExecutionFailure"
            );
            assert!(reason.contains("timed out"), "got {reason}");
        }
        other => panic!("expected Block, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// spawn-failure branch
// ---------------------------------------------------------------------------

/// Given a provider with no scripted child, when the hook runs, then the
/// spawn failure follows the fail-closed rule with the original message
/// prefix (spawn-failure branch unchanged).
#[tokio::test]
async fn should_report_execution_failure_when_provider_cannot_spawn() {
    // Given a provider that has no child to hand out
    let process = ScriptedHookProcess::with_child(Box::new(ScriptedHookChild::new(0, "", "")));
    // Drain the single child so the next exec fails.
    let _ = run_hook(
        &process,
        &hook_with("first"),
        &serde_json::json!({}),
        &HashMap::new(),
    )
    .await;

    // When the next hook runs against the exhausted provider
    let outcome = run_hook(
        &process,
        &hook_with("second"),
        &serde_json::json!({}),
        &HashMap::new(),
    )
    .await;

    // Then the failure is recorded with the spawn-failure message
    match outcome {
        HookOutcome::Error(message) => assert!(
            message.contains("Failed to spawn hook command"),
            "got {message}"
        ),
        other => panic!("expected Error, got {other:?}"),
    }
}

/// Given a hook engine constructed with a fake process provider, when
/// `trigger` runs a matching hook whose fake child exits 2, then the merged
/// trigger result is blocked — proving the provider is wired through the
/// engine's construction path (the same path `SessionScope` uses).
#[tokio::test]
async fn should_block_trigger_when_engine_injected_with_fake_process_exiting_two() {
    // Given an engine whose injected provider's child exits 2 with stderr
    let process = ScriptedHookProcess::with_child(Box::new(ScriptedHookChild::new(
        2,
        "",
        "denied by fake provider",
    )));
    let engine = mirri_core::hook_engine::HookEngine::with_process(Arc::new(process));
    engine.register(HookDef::new(HookEventType::PreToolUse, "scripted-command"));

    // When the trigger fires
    let result = engine
        .trigger(
            HookEventType::PreToolUse,
            serde_json::json!({"tool_name": "Read"}),
            &HashMap::new(),
        )
        .await;

    // Then the merged result is blocked with the exit-2 reason
    assert!(
        result.blocked,
        "the fake exit-2 hook should block the trigger"
    );
    assert_eq!(
        result.block_reason.as_deref(),
        Some("denied by fake provider"),
        "the block reason should carry the fake child's stderr"
    );
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------
