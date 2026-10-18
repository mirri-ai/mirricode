//! User-initiated `!` shell command runner — the rust analogue of the TS
//! `AgentShellCommandService` (`packages/agent-core-v2/src/agent/
//! shellCommand/shellCommandService.ts`).
//!
//! [`run_shell_command`] executes a command through the same
//! [`mirri_os::LocalProcess`] machinery the builtin Bash tool uses
//! (non-interactive environment, closed stdin, concurrent stdout/stderr
//! drains) with one TS-parity behavior the Bash tool deliberately lacks: a
//! **foreground timeout that detaches instead of killing**. When the
//! foreground window ([`SHELL_FOREGROUND_TIMEOUT_SECS`], 2 minutes) elapses
//! — or a `task.detach` RPC asks for it — the still-running process is
//! converted into a background task (registered in the session's
//! [`TaskManager`] under the task id the run minted up front, so the
//! `shell.started` event's id survives the conversion), the caller is
//! notified through the `on_backgrounded` hook (the embedded server steers
//! the model with the notice), and the run returns
//! [`ShellCommandResult::backgrounded`] without ever emitting
//! `shell.completed` — the detached run reports through the task lifecycle
//! instead.
//!
//! Streaming: while the command runs in the foreground, every decoded
//! stdout/stderr chunk is published on the session's [`EventBus`] as a
//! [`SessionEvent::ShellOutput`] (the `shell.output` wire event); the
//! settlement publishes [`SessionEvent::ShellCompleted`]. Events are only
//! emitted for runs that carry a client `command_id` (the TS service
//! publishes under the same guard — a run without an id is a
//! fire-and-forget command whose result rides the RPC response alone).
//!
//! Cancellation: `shell.cancel` aborts a foreground run through the
//! [`CancelToken`] owned by the run's [`ForegroundShellCommand`] registry
//! entry. A detached run is no longer cancellable through the command id
//! (the entry leaves the registry at detach, mirroring the TS service's
//! `finally` controller cleanup) — the process then outlives the foreground
//! window exactly like any other background task.
//!
//! Process/IO notes: the runner spawns real child processes through
//! `mirri-os` and drives its detached continuation on `tokio::spawn`, the
//! same in-crate precedent [`TaskManager::spawn`] and
//! [`crate::terminal_pool::TerminalPool`] already set (the zero-IO freeze
//! is adjudicated pending, ROADMAP §4.5 C).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use mirri_os::{DynChildProcess, DynOsProcess, ExecOptions};
use tokio::sync::{Notify, mpsc};
use uuid::Uuid;

use crate::cancel::CancelToken;
use crate::session::SessionEvent;
use crate::session::events::EventBus;
use crate::task::{TaskKind, TaskManager};
use crate::tool::{ToolUpdate, ToolUpdateKind};

/// How long a `!` command may run in the foreground before it is detached
/// to a background task (the TS `SHELL_FOREGROUND_TIMEOUT_S`, 2 minutes).
/// Injected per-run by callers (the embedded server exposes it as a
/// configuration knob so tests can shrink the window).
pub const SHELL_FOREGROUND_TIMEOUT_SECS: u64 = 2 * 60;

/// The result of one foreground shell run — the TS
/// `RunShellCommandResult` shape (`{stdout, stderr, isError, backgrounded}`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShellCommandResult {
    /// The accumulated stdout of the foreground window.
    pub stdout: String,
    /// The accumulated stderr of the foreground window.
    pub stderr: String,
    /// Whether the command failed (non-zero exit, cancellation, or an
    /// execution error).
    pub is_error: bool,
    /// The task id the detached run continues under — present only when the
    /// foreground window elapsed and the command was moved to the background.
    pub backgrounded: Option<String>,
}

/// A live foreground shell command — the registry entry `shell.cancel` and
/// `task.detach` address.
pub struct ForegroundShellCommand {
    /// The client correlation id (or, for id-less runs, the task id).
    pub handle_id: String,
    /// The id the command's background task will carry once detached.
    pub task_id: String,
    /// The command line being run.
    pub command: String,
    /// The owning session.
    pub session_id: String,
    /// Cancels the foreground run (`shell.cancel`).
    cancel: CancelToken,
    /// Wakes the run loop for the foreground→background conversion
    /// (`task.detach`). A permit stored before the loop arms is still
    /// observed — `Notify` keeps one unclaimed `notify_one`.
    detach: Notify,
    /// Whether the foreground→background conversion already happened (the
    /// timeout race and an explicit detach can both fire; the first wins).
    detached: std::sync::atomic::AtomicBool,
}

impl ForegroundShellCommand {
    /// Create the registry entry (done by [`run_shell_command`]).
    fn new(handle_id: String, task_id: String, command: String, session_id: String) -> Self {
        Self {
            handle_id,
            task_id,
            command,
            session_id,
            cancel: CancelToken::new(),
            detach: Notify::new(),
            detached: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Cancel the foreground run (user-cancellation semantics — the
    /// `shell.cancel` dispatch calls this).
    pub fn cancel_user(&self) {
        self.cancel.cancel_user();
    }

    /// The run's cancellation token.
    fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }

    /// Whether the foreground→background conversion already happened.
    pub fn is_detached(&self) -> bool {
        self.detached.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Request the foreground→background conversion. Idempotent: the first
    /// caller flips the flag and returns `true`; later callers learn the
    /// conversion already happened.
    pub fn request_detach(&self) -> bool {
        let first = !self
            .detached
            .swap(true, std::sync::atomic::Ordering::SeqCst);
        self.detach.notify_one();
        first
    }

    /// Perform the conversion's task-manager side: register the background
    /// task under the pre-assigned task id. First caller only — a second
    /// call is a no-op returning `None`.
    pub fn convert_to_background(
        &self,
        tasks: &Arc<TaskManager>,
    ) -> Option<crate::task::AgentTask> {
        if !self.request_detach() {
            return None;
        }
        Some(tasks.register_with_id(
            self.task_id.clone(),
            TaskKind::Process {
                command: Some(self.command.clone()),
                dispatcher: None,
            },
            default_task_description(&self.command),
            self.session_id.clone(),
        ))
    }
}

/// Registry of live foreground shell commands, keyed by handle id (the
/// client `command_id` when the run carries one, the task id otherwise).
#[derive(Default)]
pub struct ShellCommandRegistry {
    running: StdMutex<HashMap<String, Arc<ForegroundShellCommand>>>,
}

impl ShellCommandRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    fn insert(&self, command: Arc<ForegroundShellCommand>) {
        self.running
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(command.handle_id.clone(), command);
    }

    fn remove(&self, handle_id: &str) {
        self.running
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(handle_id);
    }

    /// The live entry for a `command_id`, if the run is still in the
    /// foreground window.
    pub fn get(&self, command_id: &str) -> Option<Arc<ForegroundShellCommand>> {
        self.running
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(command_id)
            .cloned()
    }

    /// The live entry whose foreground task id matches, for `task.detach`.
    /// A detached run's entry is removed at conversion, so this only finds
    /// still-foreground commands.
    pub fn detach_by_task(&self, task_id: &str) -> Option<Arc<ForegroundShellCommand>> {
        self.running
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .values()
            .find(|command| command.task_id == task_id)
            .cloned()
    }
}

/// Everything a run needs from its session, beyond the command itself.
pub struct ShellRunTarget {
    /// The owning session id (stamped on the detached background task).
    pub session_id: String,
    /// The session's registry of live foreground commands (cancellation and
    /// detach addressing).
    pub registry: Arc<ShellCommandRegistry>,
    /// The session's background-task manager (the detach conversion target).
    pub tasks: Arc<TaskManager>,
    /// The session's event bus (the `shell.*` event stream).
    pub event_bus: Arc<EventBus>,
    /// The process provider every `!` command executes through (zero-IO
    /// T13a). The engine default is the local one; the composition root
    /// wires its injected provider here.
    pub process: Arc<dyn DynOsProcess>,
}

/// One decoded output chunk, as forwarded by the stream pumps.
pub(crate) struct ShellChunk {
    pub(crate) kind: ToolUpdateKind,
    pub(crate) text: String,
}

/// Run one user-initiated `!` command in the foreground, streaming
/// `shell.*` events, until it settles, is cancelled, or the foreground
/// window elapses.
///
/// - **Settled** → returns the accumulated output; `backgrounded` is `None`.
/// - **Cancelled** (`shell.cancel` through the registry) → the child is
///   killed, a stderr `shell.output` plus an error `shell.completed` are
///   published, and an error result is returned.
/// - **Foreground window elapsed** or **`task.detach` requested** → the
///   process keeps running under a background task registered in
///   `target.tasks` (id = the task id the run minted), `on_backgrounded` is
///   invoked with the task id so the caller can steer the model, no
///   `shell.completed` is published, and `backgrounded` carries the task id.
///
/// `command_id` gates the event stream exactly like the TS service: runs
/// without one publish nothing and are only addressable by their
/// (unshared) task id. A spawn failure returns an error result without
/// events.
pub async fn run_shell_command(
    target: ShellRunTarget,
    command: &str,
    command_id: Option<&str>,
    foreground_timeout: Duration,
    on_backgrounded: impl FnOnce(String) + Send + 'static,
) -> ShellCommandResult {
    // The task id is minted up front so `shell.started` carries the id a
    // detach will register the background task under (the TS foreground
    // task's id survives the conversion the same way).
    let task_id = format!("task-{}", Uuid::new_v4());
    let handle_id = command_id
        .map(str::to_string)
        .unwrap_or_else(|| task_id.clone());
    let live = Arc::new(ForegroundShellCommand::new(
        handle_id.clone(),
        task_id,
        command.to_string(),
        target.session_id.clone(),
    ));
    let registry = Arc::clone(&target.registry);
    registry.insert(Arc::clone(&live));

    let result = run_tracked(
        target,
        command,
        command_id,
        &live,
        foreground_timeout,
        on_backgrounded,
    )
    .await;

    // The entry leaves the registry when the foreground window ends, on
    // every path — settled, cancelled, or detached (mirrors the TS
    // service's `finally` controller cleanup).
    registry.remove(&handle_id);
    result
}

/// The tracked run body — the registry entry is already inserted.
async fn run_tracked(
    target: ShellRunTarget,
    command: &str,
    command_id: Option<&str>,
    live: &Arc<ForegroundShellCommand>,
    foreground_timeout: Duration,
    on_backgrounded: impl FnOnce(String) + Send + 'static,
) -> ShellCommandResult {
    #[cfg(unix)]
    let (shell, shell_flag) = ("sh", "-c");
    #[cfg(windows)]
    let (shell, shell_flag) = ("cmd", "/C");

    let opts = ExecOptions {
        cwd: None,
        env: noninteractive_env(),
        timeout_ms: None,
    };
    let mut child = match target
        .process
        .exec(shell, &[shell_flag, command], &opts)
        .await
    {
        Ok(child) => child,
        // The spawn failure is reported like the TS resolveExecution error
        // path: an error-shaped result, no events (no process ever started).
        Err(error) => {
            return ShellCommandResult {
                stdout: String::new(),
                stderr: format!("Bash failed to execute: {error}"),
                is_error: true,
                backgrounded: None,
            };
        }
    };
    // Closed stdin so interactive commands see EOF instead of hanging.
    child.take_stdin();

    let publish = |event: SessionEvent| target.event_bus.publish(event);
    let publish_output = |kind, text: &str| {
        if let Some(command_id) = command_id {
            publish(SessionEvent::ShellOutput {
                command_id: command_id.to_string(),
                update: ToolUpdate {
                    kind,
                    text: Some(text.to_string()),
                    percent: None,
                    custom_kind: None,
                    custom_data: None,
                },
                task_id: Some(live.task_id.clone()),
            });
        }
    };

    // The foreground process task is observable (to the client) from the
    // moment the process starts — the TS `shell.started` semantics: the
    // event carries the id a detach converts into a background task.
    if let Some(command_id) = command_id {
        publish(SessionEvent::ShellStarted {
            command_id: command_id.to_string(),
            task_id: live.task_id.clone(),
        });
    }

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
    let cancel = live.cancel_token();
    let deadline = tokio::time::Instant::now() + foreground_timeout;
    let timeout_sleep = tokio::time::sleep_until(deadline);
    tokio::pin!(timeout_sleep);

    loop {
        tokio::select! {
            // The foreground window elapsed: detach (the child keeps
            // running under a background task).
            _ = &mut timeout_sleep => {
                live.convert_to_background(&target.tasks);
                let result = finish_detached(
                    child,
                    chunk_rx,
                    &target.tasks,
                    &live.task_id,
                    stdout,
                    stderr,
                );
                on_backgrounded(live.task_id.clone());
                return result;
            }
            // An explicit `task.detach` arrived (a stored permit fires
            // this immediately, so the conversion may already be done).
            _ = live.detach.notified() => {
                if !live.is_detached() {
                    live.convert_to_background(&target.tasks);
                }
                let result = finish_detached(
                    child,
                    chunk_rx,
                    &target.tasks,
                    &live.task_id,
                    stdout,
                    stderr,
                );
                on_backgrounded(live.task_id.clone());
                return result;
            }
            // `shell.cancel` — kill the child and settle as an error.
            _ = cancel.cancelled() => {
                let _ = child.kill().await;
                stdout_pump.abort();
                stderr_pump.abort();
                publish_output(ToolUpdateKind::Stderr, "command cancelled");
                if let Some(command_id) = command_id {
                    publish(SessionEvent::ShellCompleted {
                        command_id: command_id.to_string(),
                        is_error: true,
                        task_id: Some(live.task_id.clone()),
                    });
                }
                return ShellCommandResult {
                    stdout,
                    stderr: format!("{stderr}command cancelled"),
                    is_error: true,
                    backgrounded: None,
                };
            }
            chunk = chunk_rx.recv() => {
                let Some(ShellChunk { kind, text }) = chunk else {
                    // Both pumps are done: the streams hit EOF. Wait for
                    // the exit code and settle the foreground run.
                    let exit_code = child.wait().await.unwrap_or(-1);
                    let is_error = exit_code != 0;
                    // A failure that streamed nothing gets one synthetic
                    // stderr chunk before the completion, so live consumers
                    // see the output too (TS parity).
                    if is_error && stdout.is_empty() && stderr.is_empty() {
                        stderr = format!("command failed with exit code {exit_code}");
                        publish_output(ToolUpdateKind::Stderr, &stderr);
                    }
                    if let Some(command_id) = command_id {
                        publish(SessionEvent::ShellCompleted {
                            command_id: command_id.to_string(),
                            is_error,
                            task_id: Some(live.task_id.clone()),
                        });
                    }
                    return ShellCommandResult {
                        stdout,
                        stderr,
                        is_error,
                        backgrounded: None,
                    };
                };
                match kind {
                    ToolUpdateKind::Stdout => stdout.push_str(&text),
                    ToolUpdateKind::Stderr => stderr.push_str(&text),
                    _ => {}
                }
                if let Some(command_id) = command_id {
                    publish(SessionEvent::ShellOutput {
                        command_id: command_id.to_string(),
                        update: ToolUpdate {
                            kind,
                            text: Some(text),
                            percent: None,
                            custom_kind: None,
                            custom_data: None,
                        },
                        task_id: Some(live.task_id.clone()),
                    });
                }
            }
        }
    }
}

/// The detached branch's shared tail: hand the still-running process to its
/// background continuation (the chunk stream keeps flowing into the task's
/// accumulated output; the exit code settles the task's terminal status)
/// and build the backgrounded result.
///
/// The continuation is detached from the foreground run (its cancellation
/// is the task lifecycle's, not the command id's — mirroring the TS
/// service, which drops the command's AbortController when the run
/// detaches).
fn finish_detached(
    mut child: Box<dyn DynChildProcess>,
    mut chunk_rx: mpsc::Receiver<ShellChunk>,
    tasks: &Arc<TaskManager>,
    task_id: &str,
    stdout: String,
    stderr: String,
) -> ShellCommandResult {
    let tasks = Arc::clone(tasks);
    let task_id = task_id.to_string();
    let task_id_for_result = task_id.clone();
    tokio::spawn(async move {
        while let Some(ShellChunk { text, .. }) = chunk_rx.recv().await {
            tasks.append_output(&task_id, &text);
        }
        let exit_code = child.wait().await.unwrap_or(-1);
        tasks.complete(&task_id, exit_code);
    });
    ShellCommandResult {
        stdout,
        stderr,
        is_error: false,
        backgrounded: Some(task_id_for_result),
    }
}

/// Read one output stream to EOF, forwarding each incrementally-decoded
/// chunk into the channel. UTF-8 characters can straddle read-chunk
/// boundaries; the forwarding keeps a tail of not-yet-valid bytes back
/// until the sequence completes (the same approach the Bash tool's reader
/// uses, so no chunk is ever emitted with a split character).
///
/// `pub(crate)` since B7: the Bash tool's detachable foreground runner
/// (`tools/bash.rs`) reuses the same pump over its injected process
/// provider — the semantics are unchanged.
pub(crate) async fn pump_stream<R: tokio::io::AsyncRead + Unpin>(
    stream: Option<R>,
    kind: ToolUpdateKind,
    chunk_tx: mpsc::Sender<ShellChunk>,
) {
    use tokio::io::AsyncReadExt;

    let Some(mut stream) = stream else {
        return;
    };
    let mut buf = [0u8; 8192];
    let mut pending: Vec<u8> = Vec::new();
    while let Ok(n) = stream.read(&mut buf).await {
        if n == 0 {
            break;
        }
        pending.extend_from_slice(&buf[..n]);
        let valid_up_to = match std::str::from_utf8(&pending) {
            Ok(_) => pending.len(),
            Err(error) => error.valid_up_to(),
        };
        if valid_up_to > 0 {
            let chunk = std::str::from_utf8(&pending[..valid_up_to])
                .expect("prefix validated by from_utf8")
                .to_string();
            pending.drain(..valid_up_to);
            if chunk_tx
                .send(ShellChunk { kind, text: chunk })
                .await
                .is_err()
            {
                // The run loop is gone (killed path); stop pumping.
                break;
            }
        }
    }
}

/// Default background task description: a short preview of the command (the
/// Bash tool's spelling).
fn default_task_description(command: &str) -> String {
    let preview: String = command.chars().take(60).collect();
    if command.chars().count() > 60 {
        format!("Bash: {preview}…")
    } else {
        format!("Bash: {preview}")
    }
}

/// Environment overrides applied to every shell command so tools like `git`
/// and `node` don't open a pager, hang on a prompt, or colour the captured
/// stream — the same set the Bash tool applies.
fn noninteractive_env() -> std::collections::HashMap<String, String> {
    let git_prompt = std::env::var("GIT_TERMINAL_PROMPT").unwrap_or_else(|_| "0".to_string());
    let mut env = std::collections::HashMap::new();
    env.insert("NO_COLOR".to_string(), "1".to_string());
    env.insert("TERM".to_string(), "dumb".to_string());
    env.insert("GIT_TERMINAL_PROMPT".to_string(), git_prompt);
    if let Ok(shell) = std::env::var("SHELL") {
        env.insert("SHELL".to_string(), shell);
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::task::{TaskStatus, WaitResult};

    fn target() -> (ShellRunTarget, Arc<TaskManager>, Arc<EventBus>) {
        let tasks = Arc::new(TaskManager::new());
        let event_bus = Arc::new(EventBus::new(64));
        let target = ShellRunTarget {
            session_id: "session-1".to_string(),
            registry: Arc::new(ShellCommandRegistry::new()),
            tasks: Arc::clone(&tasks),
            event_bus: Arc::clone(&event_bus),
            process: Arc::new(mirri_os::LocalProcess::new()),
        };
        (target, tasks, event_bus)
    }

    /// Given a run of `echo hi` with a command id, when it completes, then
    /// the result carries the stdout, no error, and no background task, and
    /// the event stream brackets the run (started → output → completed).
    #[tokio::test]
    async fn should_return_stdout_when_echo_command_completes_in_foreground() {
        let (target, tasks, event_bus) = target();
        let mut events = event_bus.subscribe();

        let result = run_shell_command(
            target,
            "echo hi",
            Some("cmd-1"),
            Duration::from_secs(30),
            |_| {},
        )
        .await;

        assert_eq!(result.stdout, "hi\n");
        assert!(!result.is_error);
        assert_eq!(result.backgrounded, None);
        assert!(tasks.list().is_empty(), "foreground runs register no tasks");

        assert!(matches!(
            events.recv().await.unwrap(),
            SessionEvent::ShellStarted { .. }
        ));
        let mut saw_output = false;
        let mut saw_completed = false;
        while let Ok(event) = events.try_recv() {
            match event {
                SessionEvent::ShellOutput { update, .. } => {
                    saw_output = saw_output || update.text.as_deref() == Some("hi\n");
                }
                SessionEvent::ShellCompleted { is_error, .. } => {
                    saw_completed = true;
                    assert!(!is_error);
                }
                _ => {}
            }
        }
        assert!(saw_output, "echo stdout must stream as shell.output");
        assert!(saw_completed, "the run must publish shell.completed");
    }

    /// Given a slow command under a tiny foreground window, when the window
    /// elapses, then the run detaches: no completed event, the background
    /// task is registered under the started event's task id, the model
    /// notification fires, and the task settles when the process exits.
    #[tokio::test]
    async fn should_detach_to_background_task_when_foreground_timeout_elapses() {
        let (target, tasks, event_bus) = target();
        let mut events = event_bus.subscribe();
        let notified = Arc::new(StdMutex::new(Vec::<String>::new()));
        let notified_for_task = Arc::clone(&notified);

        let result = run_shell_command(
            target,
            "sleep 1 && echo done",
            Some("cmd-2"),
            Duration::from_millis(200),
            move |task_id| {
                notified_for_task.lock().unwrap().push(task_id);
            },
        )
        .await;

        let task_id = result.backgrounded.expect("timeout detaches the run");
        assert!(!result.is_error);
        assert_eq!(
            notified.lock().unwrap().as_slice(),
            std::slice::from_ref(&task_id),
            "the backgrounded hook must fire with the task id"
        );

        // The task is registered under the id shell.started carried.
        let started_task_id = loop {
            if let SessionEvent::ShellStarted { task_id, .. } = events.recv().await.unwrap() {
                break task_id;
            }
        };
        assert_eq!(started_task_id, task_id);
        let task = tasks.get(&task_id).expect("detached task registered");
        assert_eq!(task.status, TaskStatus::Running);
        assert!(matches!(task.kind, TaskKind::Process { .. }));

        // The task settles when the process exits, and no shell.completed
        // ever arrives for the detached run.
        assert_eq!(
            tasks.wait_for_completion(&task_id, 15).await,
            WaitResult::Completed
        );
        assert_eq!(
            tasks.get(&task_id).unwrap().status,
            TaskStatus::Completed { exit_code: 0 }
        );
        assert!(
            tasks.output(&task_id).unwrap().full_output.contains("done"),
            "the detached output lands on the task, was: {}",
            tasks.output(&task_id).unwrap().full_output
        );
        while let Ok(event) = events.try_recv() {
            assert!(
                !matches!(event, SessionEvent::ShellCompleted { .. }),
                "detached runs never emit shell.completed"
            );
        }
    }

    /// Given a slow command and its registry entry, when shell.cancel fires,
    /// then the run settles as an error without a background task.
    #[tokio::test]
    async fn should_cancel_foreground_run_when_shell_cancel_fires() {
        let (target, tasks, event_bus) = target();
        let mut events = event_bus.subscribe();
        let registry = Arc::clone(&target.registry);

        let run = tokio::spawn(run_shell_command(
            target,
            "sleep 30",
            Some("cmd-3"),
            Duration::from_secs(60),
            |_| {},
        ));
        // Wait for the started event, then cancel through the registry (the
        // same path the embedded `shell.cancel` dispatch takes).
        loop {
            if let SessionEvent::ShellStarted { .. } = events.recv().await.unwrap() {
                break;
            }
        }
        registry
            .get("cmd-3")
            .expect("run is registered")
            .cancel_user();
        let result = run.await.unwrap();

        assert!(result.is_error);
        assert_eq!(result.backgrounded, None);
        assert!(
            tasks.list().is_empty(),
            "a cancelled run must not leave a background task"
        );
    }

    /// Given a slow command under a long foreground window, when task.detach
    /// addresses its task id, then the run detaches through the same
    /// conversion the timeout takes (one background task, no completed
    /// event).
    #[tokio::test]
    async fn should_detach_to_background_task_when_task_detach_arrives() {
        let (target, tasks, event_bus) = target();
        let mut events = event_bus.subscribe();
        let registry = Arc::clone(&target.registry);

        let run = tokio::spawn(run_shell_command(
            target,
            "sleep 2",
            Some("cmd-4"),
            Duration::from_secs(60),
            |_| {},
        ));
        let task_id = loop {
            if let SessionEvent::ShellStarted { task_id, .. } = events.recv().await.unwrap() {
                break task_id;
            }
        };
        // The same conversion the embedded `task.detach` dispatch performs.
        let entry = registry
            .detach_by_task(&task_id)
            .expect("detach addresses the task id");
        assert_eq!(
            entry.convert_to_background(&tasks).map(|task| task.id),
            Some(task_id.clone())
        );
        let result = run.await.unwrap();

        assert_eq!(result.backgrounded.as_deref(), Some(task_id.as_str()));
        assert!(tasks.get(&task_id).is_some(), "the converted task is live");
        while let Ok(event) = events.try_recv() {
            assert!(
                !matches!(event, SessionEvent::ShellCompleted { .. }),
                "a detached run never emits shell.completed"
            );
        }
    }
}
