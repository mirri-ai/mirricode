//! PTY terminal abstraction — cross-platform pseudo-terminal management.
//!
//! This module wraps the [`portable_pty`] crate behind a clean async-friendly
//! API.  The underlying `portable_pty` reader is blocking, so we spawn a
//! dedicated OS thread to drain output and forward it through a
//! [`tokio::sync::mpsc`] channel.  This keeps the PTY surface simple for the
//! higher-level [`TerminalPool`](../../mirri_core/terminal_pool/struct.TerminalPool.html)
//! which lives entirely in async space.
//!
//! ## Lifecycle
//!
//! 1. [`PtyTerminal::spawn`] creates a PTY pair, starts the child process, and
//!    spawns a reader thread.
//! 2. [`PtyTerminal::write`] sends data to the child's stdin (via the PTY
//!    master writer).
//! 3. [`PtyTerminal::next_event`] returns the next [`PtyEvent`] (output data
//!    or process exit).
//! 4. [`PtyTerminal::resize`] updates the terminal window size.
//! 5. [`PtyTerminal::kill`] terminates the child process.
//! 6. [`PtyTerminal::is_alive`] checks whether the child is still running.

use std::collections::VecDeque;
use std::future::Future;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use portable_pty::{Child, ChildKiller, CommandBuilder, MasterPty, PtySize};
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// Configuration for spawning a PTY child process.
#[derive(Debug, Clone)]
pub struct PtyConfig {
    /// Shell program path (e.g. `"/bin/bash"`, `"/bin/cat"`).
    pub program: String,
    /// Extra arguments passed to the program.
    pub args: Vec<String>,
    /// Working directory for the child process.
    pub cwd: PathBuf,
    /// Additional environment variables merged on top of the parent process
    /// environment.
    pub env: Vec<(String, String)>,
    /// Initial column count.
    pub cols: u16,
    /// Initial row count.
    pub rows: u16,
}

impl Default for PtyConfig {
    fn default() -> Self {
        Self {
            program: "/bin/sh".to_string(),
            args: Vec::new(),
            cwd: PathBuf::from("."),
            env: Vec::new(),
            cols: 80,
            rows: 24,
        }
    }
}

/// Events emitted by a [`PtyTerminal`].
#[derive(Debug, Clone)]
pub enum PtyEvent {
    /// Output data from the terminal (may contain ANSI escape codes).
    Data { bytes: Vec<u8> },
    /// The child process has exited.
    Exit { code: Option<i32> },
}

/// A running PTY terminal session.
///
/// Holds the master end of the PTY, a killer handle for the child process,
/// and a channel receiver for events.  The reader thread runs in the
/// background and forwards output data and exit events through the channel.
///
/// The real child process handle is moved into the reader thread (which is
/// responsible for `wait`-ing on it).  A separate [`ChildKiller`] — obtained
/// via [`ChildKiller::clone_killer`] — is kept here for thread-safe
/// termination.
pub struct PtyTerminal {
    /// The PTY master — used for resizing and size queries.  The writer was
    /// already taken during `spawn` and is stored in `writer`.
    master: Mutex<Box<dyn MasterPty + Send>>,
    /// A separate killer handle cloned from the child.  Allows killing
    /// without interfering with the reader thread's `wait`.
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    /// Receiver for events from the reader thread.
    event_rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<PtyEvent>>,
    /// Flag set when the child has exited.
    exited: Arc<AtomicBool>,
    /// The writer handle — `take_writer` can only be called once, so we cache
    /// it behind a `Mutex`.
    writer: Mutex<Option<Box<dyn Write + Send>>>,
}

// SAFETY: `portable_pty` traits are `Send` (and some are `Sync`).  `PtyTerminal`
// wraps them behind `Mutex`/`tokio::sync::Mutex`, making the whole struct both
// `Send` and `Sync`.
unsafe impl Send for PtyTerminal {}
unsafe impl Sync for PtyTerminal {}

impl PtyTerminal {
    /// Create and start a PTY process.
    ///
    /// This opens a new PTY pair, builds a [`CommandBuilder`] from the
    /// provided [`PtyConfig`], spawns the child process, and starts a
    /// background reader thread that forwards output and exit events through
    /// a channel.
    pub fn spawn(config: &PtyConfig) -> Result<Self> {
        let pty_system = portable_pty::native_pty_system();

        let pty_size = PtySize {
            rows: config.rows,
            cols: config.cols,
            pixel_width: 0,
            pixel_height: 0,
        };

        let pair = pty_system
            .openpty(pty_size)
            .context("failed to open PTY pair")?;

        // Build the command.
        let mut cmd = CommandBuilder::new(&config.program);
        for arg in &config.args {
            cmd.arg(arg);
        }
        if config.cwd.is_dir() {
            cmd.cwd(&config.cwd);
        } else {
            debug!(
                "PTY config cwd {:?} is not a directory, inheriting process cwd",
                config.cwd
            );
        }
        for (key, value) in &config.env {
            cmd.env(key, value);
        }

        // Spawn the child process on the slave side.
        let child = pair
            .slave
            .spawn_command(cmd)
            .context("failed to spawn PTY child process")?;

        // Drop the slave so that the master gets EOF when the child exits.
        drop(pair.slave);

        // Clone a killer for independent thread-safe killing.
        let killer = child.clone_killer();

        // Get the reader before we take the writer (order matters: try_clone_reader
        // can be called multiple times, take_writer only once).
        let reader = pair
            .master
            .try_clone_reader()
            .context("failed to clone PTY reader")?;

        // Take the writer (can only be done once per master).
        let writer = pair
            .master
            .take_writer()
            .context("failed to take PTY writer")?;

        let (event_tx, event_rx) = mpsc::unbounded_channel::<PtyEvent>();
        let exited = Arc::new(AtomicBool::new(false));
        let child_for_exit = Arc::new(Mutex::new(child));
        let exited_clone = Arc::clone(&exited);
        let event_tx_clone = event_tx.clone();

        // Spawn a dedicated OS thread for the blocking reader.
        // We use std::thread (not tokio::task::spawn_blocking) because the
        // reader loop runs for the entire lifetime of the PTY and we don't
        // want to hold a blocking task slot in the tokio runtime.
        std::thread::Builder::new()
            .name("pty-reader".to_string())
            .spawn(move || {
                Self::reader_loop(reader, event_tx_clone, child_for_exit, exited_clone);
            })
            .context("failed to spawn PTY reader thread")?;

        Ok(Self {
            master: Mutex::new(pair.master),
            killer: Mutex::new(killer),
            event_rx: tokio::sync::Mutex::new(event_rx),
            exited,
            writer: Mutex::new(Some(writer)),
        })
    }

    /// The reader thread loop — reads output data in a blocking fashion and
    /// forwards it through the channel.  When the reader hits EOF (child
    /// exited), it waits for the child and sends an `Exit` event.
    fn reader_loop(
        mut reader: Box<dyn Read + Send>,
        event_tx: mpsc::UnboundedSender<PtyEvent>,
        child: Arc<Mutex<Box<dyn Child + Send + Sync>>>,
        exited: Arc<AtomicBool>,
    ) {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    // EOF — child has closed its end.
                    break;
                }
                Ok(n) => {
                    let data = buf[..n].to_vec();
                    if event_tx.send(PtyEvent::Data { bytes: data }).is_err() {
                        // Receiver dropped — terminal pool is shutting down.
                        break;
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {
                    // Retry on EINTR.
                    continue;
                }
                Err(e) => {
                    warn!("PTY reader error: {e}");
                    break;
                }
            }
        }

        // Reader hit EOF — wait for the child to exit and report the code.
        let exit_code = {
            let mut child_guard = match child.lock() {
                Ok(g) => g,
                Err(e) => {
                    warn!("PTY child mutex poisoned: {e}");
                    return;
                }
            };
            // We can't call `wait` through a `&MutexGuard` because `wait`
            // takes `&mut self`.  The `Child` trait requires `&mut self`, but
            // `MutexGuard` implements `DerefMut`, so we can get `&mut` via
            // `&mut *guard`.
            let child_ref: &mut (dyn Child + Send + Sync) = &mut **child_guard;
            match child_ref.wait() {
                Ok(status) => {
                    let code = status.exit_code();
                    if status.success() {
                        Some(0i32)
                    } else {
                        Some(code as i32)
                    }
                }
                Err(e) => {
                    warn!("PTY child wait error: {e}");
                    None
                }
            }
        };

        exited.store(true, Ordering::SeqCst);
        let _ = event_tx.send(PtyEvent::Exit { code: exit_code });
    }

    /// Write data to the PTY's stdin (master writer).
    ///
    /// Returns an error if the writer has been taken or the write fails.
    pub fn write(&self, data: &[u8]) -> Result<()> {
        let mut writer_guard = self
            .writer
            .lock()
            .map_err(|e| anyhow::anyhow!("writer lock poisoned: {e}"))?;
        let writer = writer_guard
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("PTY writer already taken"))?;
        writer.write_all(data).context("failed to write to PTY")?;
        writer.flush().context("failed to flush PTY writer")
    }

    /// Resize the terminal window.
    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let size = PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        };
        let master = self
            .master
            .lock()
            .map_err(|e| anyhow::anyhow!("master lock poisoned: {e}"))?;
        master.resize(size).context("failed to resize PTY")
    }

    /// Get the current window size as known by the kernel.
    pub fn get_size(&self) -> Result<(u16, u16)> {
        let master = self
            .master
            .lock()
            .map_err(|e| anyhow::anyhow!("master lock poisoned: {e}"))?;
        let size = master.get_size().context("failed to get PTY size")?;
        Ok((size.cols, size.rows))
    }

    /// Receive the next event (output data or exit), or `None` if the channel
    /// has been closed (all senders dropped, which happens when the reader
    /// thread exits).
    ///
    /// This is an async method because it awaits the underlying
    /// [`mpsc::UnboundedReceiver`].
    pub async fn next_event(&self) -> Option<PtyEvent> {
        let mut rx = self.event_rx.lock().await;
        rx.recv().await
    }

    /// Try to receive an event without blocking.
    ///
    /// Returns `Some(event)` if an event is available, `None` otherwise.
    pub fn try_next_event(&self) -> Option<PtyEvent> {
        self.event_rx
            .try_lock()
            .ok()
            .and_then(|mut rx| rx.try_recv().ok())
    }

    /// Drain all pending events that are immediately available.
    ///
    /// Returns an empty vector if no events are buffered.
    pub async fn drain_events(&self) -> Vec<PtyEvent> {
        let mut events = VecDeque::new();
        let mut rx = self.event_rx.lock().await;
        while let Ok(event) = rx.try_recv() {
            events.push_back(event);
        }
        events.into()
    }

    /// Kill the PTY child process.
    ///
    /// This sends a termination signal to the child.  The reader thread will
    /// observe EOF and send an [`PtyEvent::Exit`] event.
    pub fn kill(&self) -> Result<()> {
        let mut killer = self
            .killer
            .lock()
            .map_err(|e| anyhow::anyhow!("killer lock poisoned: {e}"))?;
        killer.kill().context("failed to kill PTY child process")
    }

    /// Check whether the child process is still running.
    ///
    /// This uses an atomic flag set by the reader thread when it observes
    /// exit.  It is not a real `waitpid` poll — once the flag is set it stays
    /// set.
    pub fn is_alive(&self) -> bool {
        !self.exited.load(Ordering::SeqCst)
    }
}

impl Drop for PtyTerminal {
    fn drop(&mut self) {
        // Best-effort kill on drop to avoid orphaned processes.
        if self.is_alive()
            && let Err(e) = self.kill()
        {
            debug!("Best-effort kill during PtyTerminal drop failed: {e}");
        }
    }
}

// ---------------------------------------------------------------------------
// Provider / session traits (zero-IO T13c-3)
// ---------------------------------------------------------------------------

/// A boxed, `Send` future as returned by the dyn-compatible
/// [`PtySession::next_event`].
///
/// [`PtyTerminal::next_event`] itself is an `async fn` (RPITIT), which is not
/// dyn-compatible; this erased return type lets sessions be injected as
/// `Arc<dyn PtySession>` (the same pattern `DynOsProcess` uses for
/// [`crate::process::OsProcess`]).
pub type DynPtyFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A running PTY session behind an object-safe trait.
///
/// The surface is the minimal set the engine's
/// [`TerminalPool`](../../mirri_core/terminal_pool/struct.TerminalPool.html)
/// consumes: event consumption, stdin writes, resize, and kill. Implementors
/// must be `Send + Sync` so a session can be shared between the pool's event
/// task and its callers.
pub trait PtySession: Send + Sync {
    /// Receive the next [`PtyEvent`] (output data or exit), or `None` once the
    /// event channel has closed.
    fn next_event(&self) -> DynPtyFuture<'_, Option<PtyEvent>>;

    /// Write data to the PTY's stdin (the master writer).
    fn write(&self, data: &[u8]) -> Result<()>;

    /// Resize the terminal window.
    fn resize(&self, cols: u16, rows: u16) -> Result<()>;

    /// Kill the PTY child process.
    fn kill(&self) -> Result<()>;
}

impl PtySession for PtyTerminal {
    fn next_event(&self) -> DynPtyFuture<'_, Option<PtyEvent>> {
        Box::pin(PtyTerminal::next_event(self))
    }

    fn write(&self, data: &[u8]) -> Result<()> {
        PtyTerminal::write(self, data)
    }

    fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        PtyTerminal::resize(self, cols, rows)
    }

    fn kill(&self) -> Result<()> {
        PtyTerminal::kill(self)
    }
}

/// Provider that spawns PTY sessions.
///
/// The engine's terminal pool is injected with an `Arc<dyn PtyProvider>` so
/// PTY creation can be faked in tests (zero-IO T13c-3). [`LocalPty`] is the
/// production implementation wrapping [`PtyTerminal::spawn`].
pub trait PtyProvider: Send + Sync {
    /// Create and start a PTY session for the given config.
    fn spawn(&self, config: &PtyConfig) -> Result<Arc<dyn PtySession>>;
}

/// [`PtyProvider`] implementation backed by the host's native PTY system.
#[derive(Debug, Default)]
pub struct LocalPty;

impl LocalPty {
    /// Create a new `LocalPty`.
    pub fn new() -> Self {
        Self
    }
}

impl PtyProvider for LocalPty {
    fn spawn(&self, config: &PtyConfig) -> Result<Arc<dyn PtySession>> {
        let terminal = PtyTerminal::spawn(config)?;
        Ok(Arc::new(terminal))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn should_produce_output_and_exit_when_spawning_echo_program() {
        // Use /bin/echo as a simple program that writes output and exits.
        let config = PtyConfig {
            program: "/bin/echo".to_string(),
            args: vec!["hello-pty".to_string()],
            cwd: PathBuf::from("/tmp"),
            cols: 80,
            rows: 24,
            env: Vec::new(),
        };

        let pty = PtyTerminal::spawn(&config).expect("failed to spawn PTY");

        // Collect events until we see an Exit.
        let mut got_data = false;
        let mut exit_code = None;
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);

        loop {
            tokio::select! {
                event = pty.next_event() => {
                    match event {
                        Some(PtyEvent::Data { bytes }) => {
                            let text = String::from_utf8_lossy(&bytes);
                            if text.contains("hello-pty") {
                                got_data = true;
                            }
                        }
                        Some(PtyEvent::Exit { code }) => {
                            exit_code = code;
                            break;
                        }
                        None => break,
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    panic!("timed out waiting for PTY events");
                }
            }
        }

        assert!(
            got_data,
            "should have received output containing 'hello-pty'"
        );
        assert_eq!(exit_code, Some(0), "echo should exit with code 0");
        assert!(!pty.is_alive(), "PTY should report not alive after exit");
    }

    #[tokio::test]
    async fn should_echo_output_when_writing_to_cat_program() {
        // Use /bin/cat which echoes stdin to stdout.
        let config = PtyConfig {
            program: "/bin/cat".to_string(),
            args: Vec::new(),
            cwd: PathBuf::from("/tmp"),
            cols: 80,
            rows: 24,
            env: Vec::new(),
        };

        let pty = PtyTerminal::spawn(&config).expect("failed to spawn PTY");

        // Give the process a moment to start.
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Write data to the PTY.
        pty.write(b"test-input\n").expect("failed to write to PTY");

        // Collect events — cat should echo the data back.
        let mut received = String::new();
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(3);

        loop {
            tokio::select! {
                event = pty.next_event() => {
                    match event {
                        Some(PtyEvent::Data { bytes }) => {
                            received.push_str(&String::from_utf8_lossy(&bytes));
                            if received.contains("test-input") {
                                break;
                            }
                        }
                        Some(PtyEvent::Exit { .. }) | None => break,
                    }
                }
                _ = tokio::time::sleep_until(deadline) => {
                    panic!("timed out waiting for cat echo");
                }
            }
        }

        assert!(
            received.contains("test-input"),
            "cat should have echoed 'test-input', got: {received}"
        );

        // Kill cat to clean up.
        pty.kill().expect("failed to kill cat");
    }

    #[test]
    fn should_have_sensible_defaults_when_constructing_default_pty_config() {
        let config = PtyConfig::default();
        assert_eq!(config.program, "/bin/sh");
        assert_eq!(config.cols, 80);
        assert_eq!(config.rows, 24);
        assert!(config.args.is_empty());
    }

    #[tokio::test]
    async fn should_not_error_when_resizing_pty() {
        let config = PtyConfig {
            program: "/bin/cat".to_string(),
            args: Vec::new(),
            cwd: PathBuf::from("/tmp"),
            cols: 80,
            rows: 24,
            env: Vec::new(),
        };

        let pty = PtyTerminal::spawn(&config).expect("failed to spawn PTY");

        // Resize should succeed.
        pty.resize(120, 40).expect("resize should succeed");

        let (cols, rows) = pty.get_size().expect("get_size should succeed");
        assert_eq!(cols, 120);
        assert_eq!(rows, 40);

        pty.kill().expect("failed to kill cat");
    }

    /// Given a `LocalPty` provider, when spawning a short-lived program
    /// through the `Arc<dyn PtySession>` trait object, then events flow
    /// through the erased surface exactly as through the concrete type
    /// (zero-IO T13c-3).
    #[tokio::test]
    async fn should_stream_events_through_the_session_trait_when_spawned_via_provider() {
        let provider = LocalPty::new();
        let config = PtyConfig {
            program: "/bin/echo".to_string(),
            args: vec!["hello-provider".to_string()],
            cwd: PathBuf::from("/tmp"),
            cols: 80,
            rows: 24,
            env: Vec::new(),
        };

        let session: Arc<dyn PtySession> = provider
            .spawn(&config)
            .expect("LocalPty::spawn should succeed");

        // The trait object must be usable from a spawned task (Send + Sync).
        let session_for_task = Arc::clone(&session);
        let task = tokio::spawn(async move {
            let mut received = String::new();
            let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
            loop {
                tokio::select! {
                    event = session_for_task.next_event() => match event {
                        Some(PtyEvent::Data { bytes }) => {
                            received.push_str(&String::from_utf8_lossy(&bytes));
                        }
                        Some(PtyEvent::Exit { code }) => return (received, code),
                        None => return (received, None),
                    },
                    _ = tokio::time::sleep_until(deadline) => panic!("timed out on trait events"),
                }
            }
        });
        let (received, code) = task.await.expect("task should finish");

        assert!(
            received.contains("hello-provider"),
            "trait-object events should carry the program output, got: {received}"
        );
        assert_eq!(code, Some(0), "echo should exit 0 through the trait");
    }

    /// Given a session spawned via the provider, when writing and resizing
    /// through the trait object, then both delegate to the concrete PTY.
    #[test]
    fn should_delegate_write_resize_and_kill_through_the_session_trait() {
        let provider = LocalPty::new();
        let config = PtyConfig {
            program: "/bin/cat".to_string(),
            args: Vec::new(),
            cwd: PathBuf::from("/tmp"),
            cols: 80,
            rows: 24,
            env: Vec::new(),
        };

        let session: Arc<dyn PtySession> = provider
            .spawn(&config)
            .expect("LocalPty::spawn should succeed");

        session
            .write(b"probe\n")
            .expect("trait write should succeed");
        session
            .resize(100, 30)
            .expect("trait resize should succeed");
        session.kill().expect("trait kill should succeed");
    }
}
