//! Terminal Pool — session-level PTY terminal management.
//!
//! Each agent session may own multiple interactive terminals (e.g. a shell, a
//! build watcher, a REPL).  The [`TerminalPool`] manages the lifecycle of all
//! terminals belonging to one session:
//!
//! - **Creation** — [`TerminalPool::create_terminal`] spawns a PTY child
//!   process and returns a `terminal_id`.
//! - **I/O** — [`TerminalPool::write`] sends data; output is captured in a
//!   bounded FIFO buffer per terminal.
//! - **History** — [`TerminalPool::get_output_since`] returns all frames with
//!   `seq > since_seq`, enabling late-attach replay.
//! - **Live streaming** — [`TerminalPool::attach_sink`] registers a callback
//!   that receives every new [`TerminalFrame`] in real time.
//! - **Lifecycle** — [`TerminalPool::kill`], [`TerminalPool::close_all`].
//!
//! ## Architecture
//!
//! Each terminal runs a dedicated tokio task that consumes [`PtyEvent`]s from
//! the [`PtyTerminal`], converts them to [`TerminalFrame`]s, appends them to
//! the bounded buffer, and fans them out to all attached sinks.  The task
//! exits when the PTY child process exits.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use mirri_os::{LocalPty, PtyConfig, PtyEvent, PtyProvider, PtySession};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

/// Default bounded output buffer size (number of frames retained per
/// terminal for replay).
pub const DEFAULT_BUFFER_SIZE: usize = 2000;

/// Default maximum number of terminals per pool.
pub const DEFAULT_MAX_TERMINALS: usize = 10;

// ---------------------------------------------------------------------------
// TerminalFrame — the wire-level output unit
// ---------------------------------------------------------------------------

/// A single frame of terminal output or an exit notification.
///
/// Serialized as a tagged enum for the wire protocol:
/// `{"type": "terminal_output", ...}` or `{"type": "terminal_exit", ...}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum TerminalFrame {
    /// Output data from the terminal.  `data` is a UTF-8 string that may
    /// contain ANSI escape codes.
    #[serde(rename = "terminal_output")]
    Output {
        terminal_id: String,
        /// Monotonically increasing sequence number within this terminal.
        seq: u64,
        /// UTF-8 string (may contain ANSI escape codes).
        data: String,
    },
    /// The terminal's child process has exited.
    #[serde(rename = "terminal_exit")]
    Exit {
        terminal_id: String,
        /// Process exit code, if available.
        code: Option<i32>,
    },
}

// ---------------------------------------------------------------------------
// TerminalStatus
// ---------------------------------------------------------------------------

/// The lifecycle status of a terminal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum TerminalStatus {
    /// The child process is running.
    Running,
    /// The child process has exited with the given code.
    Exited(Option<i32>),
}

// ---------------------------------------------------------------------------
// TerminalInfo — public snapshot of a terminal
// ---------------------------------------------------------------------------

/// A serializable snapshot of a terminal's state, returned by
/// [`TerminalPool::list_terminals`].
#[derive(Debug, Clone, Serialize)]
pub struct TerminalInfo {
    pub id: String,
    pub cwd: String,
    pub shell: String,
    pub cols: u16,
    pub rows: u16,
    pub status: TerminalStatus,
    pub created_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
}

// ---------------------------------------------------------------------------
// SinkHandle — opaque ID for detaching a sink
// ---------------------------------------------------------------------------

/// An opaque handle used to detach a previously attached sink via
/// [`TerminalPool::detach_sink`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SinkHandle(pub u64);

/// Type alias for a sink callback.  Takes ownership of a [`TerminalFrame`].
pub type SinkCallback = Arc<dyn Fn(TerminalFrame) + Send + Sync>;

// ---------------------------------------------------------------------------
// BoundedOutputBuffer — FIFO ring buffer with seq tracking
// ---------------------------------------------------------------------------

/// A bounded FIFO buffer of [`TerminalFrame`]s.
///
/// New frames are appended to the tail.  When the buffer exceeds
/// `max_size`, the oldest frame is evicted from the head.  Each `Output`
/// frame is assigned a monotonically increasing sequence number starting
/// at **1**, so `since_seq = 0` (or [`TerminalPool::last_seq`] on a fresh
/// terminal) means "replay everything" — a fresh attach never loses the
/// very first frame.
struct BoundedOutputBuffer {
    frames: VecDeque<TerminalFrame>,
    max_size: usize,
    next_seq: u64,
}

impl BoundedOutputBuffer {
    fn new(max_size: usize) -> Self {
        Self {
            frames: VecDeque::with_capacity(max_size.min(256)),
            max_size,
            next_seq: 1,
        }
    }

    /// Allocate the next sequence number and increment the counter.
    fn alloc_seq(&mut self) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        seq
    }

    /// Push a frame, evicting the oldest if over capacity.
    fn push(&mut self, frame: TerminalFrame) {
        self.frames.push_back(frame);
        while self.frames.len() > self.max_size {
            self.frames.pop_front();
        }
    }

    /// Return all `Output` frames with `seq > since_seq`.
    fn output_since(&self, since_seq: u64) -> Vec<TerminalFrame> {
        self.frames
            .iter()
            .filter(|f| match f {
                TerminalFrame::Output { seq, .. } => *seq > since_seq,
                TerminalFrame::Exit { .. } => false,
            })
            .cloned()
            .collect()
    }

    /// The highest sequence number assigned so far.
    fn last_seq(&self) -> u64 {
        if self.next_seq == 0 {
            0
        } else {
            self.next_seq - 1
        }
    }
}

// ---------------------------------------------------------------------------
// TerminalRecord — internal per-terminal state
// ---------------------------------------------------------------------------

/// Internal state for a single terminal in the pool.
struct TerminalRecord {
    id: String,
    cwd: PathBuf,
    shell: String,
    cols: u16,
    rows: u16,
    status: Mutex<TerminalStatus>,
    created_at: DateTime<Utc>,
    closed_at: Mutex<Option<DateTime<Utc>>>,
    /// Bounded output buffer — protected by a `Mutex` so the event task can
    /// write while callers read.
    buffer: Mutex<BoundedOutputBuffer>,
    /// Attached sinks — the event task fans out frames to all of these.
    sinks: Mutex<HashMap<u64, SinkCallback>>,
    /// The next sink handle ID to assign.
    next_sink_id: AtomicU64,
    /// The underlying PTY session.  Wrapped in `Option` so `close_all` can
    /// take ownership and kill it.
    pty: Mutex<Option<Arc<dyn PtySession>>>,
    /// Handle to the spawned event task, so we can abort it on close.
    task_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl TerminalRecord {
    /// Generate a new unique sink handle.
    fn alloc_sink_handle(&self) -> SinkHandle {
        let id = self.next_sink_id.fetch_add(1, Ordering::SeqCst);
        SinkHandle(id)
    }

    /// Snapshot the terminal's public info.
    async fn info(&self) -> TerminalInfo {
        let status = self.status.lock().await.clone();
        let closed_at = *self.closed_at.lock().await;
        TerminalInfo {
            id: self.id.clone(),
            cwd: self.cwd.to_string_lossy().to_string(),
            shell: self.shell.clone(),
            cols: self.cols,
            rows: self.rows,
            status,
            created_at: self.created_at,
            closed_at,
        }
    }

    /// Record one output frame and fan it out to all attached sinks.
    ///
    /// The buffer append and the sink fan-out happen under **one** dual-lock
    /// critical section (buffer, then sinks — the same order
    /// [`TerminalPool::attach_sink_with_replay`] takes), so a concurrent
    /// attach-with-replay can never observe the frame half-recorded: each
    /// frame is either in the replay snapshot (appended before the attach)
    /// or delivered to the sink (fanned out after the sink's installation),
    /// never both and never neither. Allocating the seq and pushing the frame
    /// under a single buffer acquisition also closes the window where
    /// [`TerminalPool::last_seq`] reported a seq whose frame was not yet
    /// queryable.
    async fn emit_output(&self, data: String) {
        // Acquire the sinks guard while the buffer lock is still held — the
        // dual-lock protocol (buffer, then sinks). Both guards stay alive
        // across the fan-out below.
        let (sinks_guard, frame) = {
            let mut buf = self.buffer.lock().await;
            let seq = buf.alloc_seq();
            let frame = TerminalFrame::Output {
                terminal_id: self.id.clone(),
                seq,
                data,
            };
            buf.push(frame.clone());
            (self.sinks.lock().await, frame)
        };
        for sink in sinks_guard.values() {
            sink(frame.clone());
        }
    }
}

// ---------------------------------------------------------------------------
// TerminalPool
// ---------------------------------------------------------------------------

/// Session-level terminal pool — manages the lifecycle of all PTY terminals
/// belonging to one agent session.
///
/// Each terminal is identified by a unique `terminal_id` (format:
/// `"term_<uuid>"`).  The pool enforces a maximum terminal count and
/// maintains a bounded output buffer per terminal for late-attach replay.
pub struct TerminalPool {
    terminals: RwLock<HashMap<String, Arc<TerminalRecord>>>,
    max_terminals: usize,
    buffer_size: usize,
    /// PTY creation provider (zero-IO T13c-3). Defaults to
    /// [`LocalPty`] (the host's native PTY system); tests inject scripted
    /// fakes through [`TerminalPool::with_provider`].
    pty_provider: Arc<dyn PtyProvider>,
}

impl TerminalPool {
    /// Create a new `TerminalPool` with the given limits.
    ///
    /// - `max_terminals` — maximum number of concurrent terminals.
    /// - `buffer_size` — maximum number of output frames retained per terminal
    ///   for replay via [`TerminalPool::get_output_since`].
    ///
    /// PTY creation goes through the default [`LocalPty`] provider.
    pub fn new(max_terminals: usize, buffer_size: usize) -> Self {
        Self::with_provider(max_terminals, buffer_size, Arc::new(LocalPty::new()))
    }

    /// Create a new `TerminalPool` with an explicit PTY provider.
    ///
    /// Same limits as [`TerminalPool::new`]; `pty_provider` is the injected
    /// PTY creation surface (scripted fakes in tests, [`LocalPty`] in
    /// production — the default [`TerminalPool::new`] constructor).
    pub fn with_provider(
        max_terminals: usize,
        buffer_size: usize,
        pty_provider: Arc<dyn PtyProvider>,
    ) -> Self {
        Self {
            terminals: RwLock::new(HashMap::new()),
            max_terminals,
            buffer_size,
            pty_provider,
        }
    }

    /// Create a new terminal with default limits.
    pub fn with_defaults() -> Self {
        Self::new(DEFAULT_MAX_TERMINALS, DEFAULT_BUFFER_SIZE)
    }

    /// Detect the user's default shell.
    ///
    /// Thin delegation to [`mirri_os::shell::default_shell`]: priority is the
    /// explicit argument > `MIRRI_SHELL_PATH` (canonical) >
    /// `MIRRICODE_SHELL_PATH` (legacy alias, same pattern as the config env
    /// layer) > `$SHELL` > platform fallback (Windows: pwsh → powershell →
    /// cmd; otherwise `/bin/sh`). The decision logic lives in mirri-os so it
    /// is testable platform-independently (D-02).
    fn detect_shell(shell: Option<&str>) -> String {
        mirri_os::shell::default_shell(shell)
    }

    /// Create a new terminal in the pool.
    ///
    /// Spawns a PTY child process running `shell` (or the detected default)
    /// in `cwd`, with initial dimensions `cols` × `rows`.
    ///
    /// Returns the `terminal_id` (format: `"term_<uuid>"`) on success.
    ///
    /// # Errors
    /// - If the pool is at capacity (`max_terminals` reached).
    /// - If the PTY fails to spawn.
    pub async fn create_terminal(
        &self,
        cwd: PathBuf,
        shell: Option<String>,
        cols: u16,
        rows: u16,
    ) -> anyhow::Result<String> {
        let shell_resolved = Self::detect_shell(shell.as_deref());

        // Check capacity.
        {
            let terminals = self.terminals.read().await;
            if terminals.len() >= self.max_terminals {
                anyhow::bail!("terminal pool at capacity (max {})", self.max_terminals);
            }
        }

        let terminal_id = format!("term_{}", Uuid::new_v4());
        let created_at = Utc::now();

        // Build PTY config.
        let pty_config = PtyConfig {
            program: shell_resolved.clone(),
            args: Vec::new(),
            cwd: cwd.clone(),
            env: Vec::new(),
            cols,
            rows,
        };

        let pty: Arc<dyn PtySession> = self.pty_provider.spawn(&pty_config)?;

        let record = Arc::new(TerminalRecord {
            id: terminal_id.clone(),
            cwd: cwd.clone(),
            shell: shell_resolved,
            cols,
            rows,
            status: Mutex::new(TerminalStatus::Running),
            created_at,
            closed_at: Mutex::new(None),
            buffer: Mutex::new(BoundedOutputBuffer::new(self.buffer_size)),
            sinks: Mutex::new(HashMap::new()),
            next_sink_id: AtomicU64::new(0),
            pty: Mutex::new(Some(Arc::clone(&pty))),
            task_handle: Mutex::new(None),
        });

        // Spawn the event consumer task.
        let record_for_task = Arc::clone(&record);
        let pty_for_task = Arc::clone(&pty);
        let task = tokio::spawn(async move {
            Self::event_loop(pty_for_task, record_for_task).await;
        });

        *record.task_handle.lock().await = Some(task);

        // Insert into pool.
        self.terminals
            .write()
            .await
            .insert(terminal_id.clone(), record);

        Ok(terminal_id)
    }

    /// The per-terminal event loop — consumes [`PtyEvent`]s, converts them to
    /// [`TerminalFrame`]s, appends to the bounded buffer, and fans out to
    /// sinks.
    async fn event_loop(pty: Arc<dyn PtySession>, record: Arc<TerminalRecord>) {
        loop {
            let event = pty.next_event().await;
            match event {
                Some(PtyEvent::Data { bytes }) => {
                    // Convert bytes to UTF-8 string (lossy — terminal output
                    // may contain partial UTF-8 sequences).
                    let data = String::from_utf8_lossy(&bytes).into_owned();
                    // Record the frame and fan out under one dual-lock
                    // critical section (see `TerminalRecord::emit_output`).
                    record.emit_output(data).await;
                }
                Some(PtyEvent::Exit { code }) => {
                    // Update status.
                    {
                        let mut status = record.status.lock().await;
                        *status = TerminalStatus::Exited(code);
                    }
                    {
                        let mut closed_at = record.closed_at.lock().await;
                        *closed_at = Some(Utc::now());
                    }

                    let frame = TerminalFrame::Exit {
                        terminal_id: record.id.clone(),
                        code,
                    };

                    // Push exit frame to buffer too (though get_output_since
                    // only returns Output frames).
                    {
                        let mut buf = record.buffer.lock().await;
                        buf.push(frame.clone());
                    }

                    // Fan out to sinks.
                    let sinks = record.sinks.lock().await;
                    for sink in sinks.values() {
                        sink(frame.clone());
                    }

                    // Event loop is done — the PTY has exited.
                    break;
                }
                None => {
                    // Channel closed (reader thread exited without sending
                    // Exit).  Mark as exited with unknown code.
                    let mut status = record.status.lock().await;
                    *status = TerminalStatus::Exited(None);
                    break;
                }
            }
        }
    }

    /// Get a terminal record by ID, returning an error if not found.
    async fn get_terminal(&self, terminal_id: &str) -> anyhow::Result<Arc<TerminalRecord>> {
        let terminals = self.terminals.read().await;
        terminals
            .get(terminal_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("terminal not found: {terminal_id}"))
    }

    /// Write data to a terminal's stdin.
    pub async fn write(&self, terminal_id: &str, data: &str) -> anyhow::Result<()> {
        let record = self.get_terminal(terminal_id).await?;
        let pty_guard = record.pty.lock().await;
        let pty = pty_guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("terminal already closed: {terminal_id}"))?;
        pty.write(data.as_bytes())?;
        Ok(())
    }

    /// Resize a terminal.
    pub async fn resize(&self, terminal_id: &str, cols: u16, rows: u16) -> anyhow::Result<()> {
        let record = self.get_terminal(terminal_id).await?;
        let pty_guard = record.pty.lock().await;
        let pty = pty_guard
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("terminal already closed: {terminal_id}"))?;
        pty.resize(cols, rows)?;
        Ok(())
    }

    /// Kill a terminal's child process.
    ///
    /// The terminal record remains in the pool (with status `Exited`) until
    /// [`TerminalPool::close_all`] or explicit removal.
    pub async fn kill(&self, terminal_id: &str) -> anyhow::Result<()> {
        let record = self.get_terminal(terminal_id).await?;
        let pty_guard = record.pty.lock().await;
        if let Some(pty) = pty_guard.as_ref() {
            pty.kill()?;
        }
        Ok(())
    }

    /// Get all output frames with `seq > since_seq` for a terminal.
    ///
    /// Returns frames in chronological order.  Only `Output` frames are
    /// returned (not `Exit` frames).
    pub async fn get_output_since(
        &self,
        terminal_id: &str,
        since_seq: u64,
    ) -> anyhow::Result<Vec<TerminalFrame>> {
        let record = self.get_terminal(terminal_id).await?;
        let buf = record.buffer.lock().await;
        Ok(buf.output_since(since_seq))
    }

    /// Attach a sink callback to receive real-time [`TerminalFrame`]s from a
    /// terminal.
    ///
    /// Returns a [`SinkHandle`] that can be used with
    /// [`TerminalPool::detach_sink`] to remove the callback.
    pub async fn attach_sink(
        &self,
        terminal_id: &str,
        sink: Box<dyn Fn(TerminalFrame) + Send + Sync>,
    ) -> anyhow::Result<SinkHandle> {
        let record = self.get_terminal(terminal_id).await?;
        let handle = record.alloc_sink_handle();
        let mut sinks = record.sinks.lock().await;
        sinks.insert(handle.0, Arc::from(sink));
        Ok(handle)
    }

    /// Attach a sink and take its replay snapshot in **one atomic step**.
    ///
    /// The buffer snapshot (`seq > since_seq`) and the sink installation
    /// happen under the same dual-lock critical section (buffer, then sinks —
    /// the same order the event loop's frame emission holds), so a frame
    /// being emitted concurrently is either included in the returned replay
    /// (appended before this call acquired the locks) or delivered to the
    /// newly installed sink (fanned out after), never both and never
    /// neither. Callers that replay the returned frames before consuming
    /// the sink's live frames get gap-free, duplicate-free scrollback — the
    /// "reconnect loses no scrollback" contract (F-P3-1).
    ///
    /// Returns the sink handle plus the frames to replay.
    pub async fn attach_sink_with_replay(
        &self,
        terminal_id: &str,
        since_seq: u64,
        sink: Box<dyn Fn(TerminalFrame) + Send + Sync>,
    ) -> anyhow::Result<(SinkHandle, Vec<TerminalFrame>)> {
        let record = self.get_terminal(terminal_id).await?;
        let handle = record.alloc_sink_handle();
        // Buffer, then sinks — the dual-lock order `emit_output` keeps.
        let buf = record.buffer.lock().await;
        let mut sinks = record.sinks.lock().await;
        let replay = buf.output_since(since_seq);
        sinks.insert(handle.0, Arc::from(sink));
        Ok((handle, replay))
    }

    /// Detach a previously attached sink.
    pub async fn detach_sink(&self, terminal_id: &str, handle: SinkHandle) -> anyhow::Result<()> {
        let record = self.get_terminal(terminal_id).await?;
        let mut sinks = record.sinks.lock().await;
        sinks.remove(&handle.0);
        Ok(())
    }

    /// List all terminals in the pool with their current state.
    pub async fn list_terminals(&self) -> Vec<TerminalInfo> {
        let terminals = self.terminals.read().await;
        let mut infos = Vec::with_capacity(terminals.len());
        for record in terminals.values() {
            infos.push(record.info().await);
        }
        infos
    }

    /// Get the current sequence number for a terminal (the next frame will
    /// have `seq = last_seq + 1`).
    pub async fn last_seq(&self, terminal_id: &str) -> anyhow::Result<u64> {
        let record = self.get_terminal(terminal_id).await?;
        let buf = record.buffer.lock().await;
        Ok(buf.last_seq())
    }

    /// Close all terminals in the pool, killing their child processes and
    /// aborting event tasks.
    pub async fn close_all(&self) -> anyhow::Result<()> {
        let terminals = {
            let mut map = self.terminals.write().await;
            std::mem::take(&mut *map)
        };

        for (_, record) in terminals {
            // Abort the event task.
            if let Some(handle) = record.task_handle.lock().await.take() {
                handle.abort();
            }

            // Kill the PTY.
            {
                let pty_guard = record.pty.lock().await;
                if let Some(pty) = pty_guard.as_ref() {
                    let _ = pty.kill();
                }
            }

            // Mark as closed.
            {
                let mut closed_at = record.closed_at.lock().await;
                if closed_at.is_none() {
                    *closed_at = Some(Utc::now());
                }
            }
            {
                let mut status = record.status.lock().await;
                if *status == TerminalStatus::Running {
                    *status = TerminalStatus::Exited(None);
                }
            }
        }

        Ok(())
    }

    /// The current number of terminals in the pool.
    pub async fn len(&self) -> usize {
        self.terminals.read().await.len()
    }

    /// Whether the pool has no terminals.
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }
}

impl Default for TerminalPool {
    fn default() -> Self {
        Self::with_defaults()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// A hand-built terminal record with no PTY — frames are injected
    /// directly through [`TerminalRecord::emit_output`], the same path the
    /// event loop takes, so the attach/replay lock protocol can be
    /// exercised without child-process timing.
    fn ptyless_record(id: &str) -> Arc<TerminalRecord> {
        Arc::new(TerminalRecord {
            id: id.to_string(),
            cwd: PathBuf::from("/tmp"),
            shell: "/bin/cat".to_string(),
            cols: 80,
            rows: 24,
            status: Mutex::new(TerminalStatus::Running),
            created_at: Utc::now(),
            closed_at: Mutex::new(None),
            buffer: Mutex::new(BoundedOutputBuffer::new(DEFAULT_BUFFER_SIZE)),
            sinks: Mutex::new(HashMap::new()),
            next_sink_id: AtomicU64::new(0),
            pty: Mutex::new(None),
            task_handle: Mutex::new(None),
        })
    }

    /// Given a terminal emitting frames while a caller repeatedly attaches
    /// with replay and detaches, when each attach lands anywhere inside the
    /// 100-frame window, then every view is consistent — no frame is both
    /// in an attach's replay snapshot and delivered to its sink (zero
    /// duplication), and the final held view covers the whole stream exactly
    /// once (zero loss) (F-P3-1).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn should_deliver_every_frame_exactly_once_when_attaching_with_replay_mid_stream() {
        // Vary the attach cadence across rounds so different interleavings
        // of the writer and the attaching task are exercised.
        for cycle_pause_us in [0u64, 50, 200, 1000] {
            let pool = TerminalPool::with_defaults();
            let record = ptyless_record("term_race");
            pool.terminals
                .write()
                .await
                .insert("term_race".to_string(), Arc::clone(&record));

            let writer = tokio::spawn(async move {
                for i in 1..=100u64 {
                    record.emit_output(format!("frame-{i}")).await;
                    // Yield between frames so the attaching task can land
                    // between emits.
                    tokio::task::yield_now().await;
                }
            });

            // Each attach/detach cycle is one interleaving sample: the
            // per-cycle invariant (replay ∩ sink = ∅) is exactly what the
            // dual-lock protocol guarantees. The final attach is held until
            // the writer completes, so its view must cover everything.
            let mut final_replay = Vec::new();
            let mut final_sink = Vec::new();
            for cycle in 0..50u64 {
                let sink_seqs = Arc::new(std::sync::Mutex::new(Vec::<u64>::new()));
                let sink_seqs_for_sink = Arc::clone(&sink_seqs);
                let (handle, replay) = pool
                    .attach_sink_with_replay(
                        "term_race",
                        0,
                        Box::new(move |frame: TerminalFrame| {
                            if let TerminalFrame::Output { seq, .. } = frame {
                                sink_seqs_for_sink.lock().unwrap().push(seq);
                            }
                        }),
                    )
                    .await
                    .expect("attach with replay should succeed");
                if cycle == 49 {
                    // Let the writer drain through the final sink.
                    writer.await.expect("writer task should finish");
                    final_replay = replay;
                    final_sink = sink_seqs.lock().unwrap().clone();
                    pool.detach_sink("term_race", handle)
                        .await
                        .expect("detach should succeed");
                    break;
                }
                let replay_seqs: Vec<u64> = replay
                    .iter()
                    .filter_map(|f| match f {
                        TerminalFrame::Output { seq, .. } => Some(*seq),
                        _ => None,
                    })
                    .collect();
                let sink_seqs = sink_seqs.lock().unwrap().clone();
                // Zero duplication within this view: the replay snapshot
                // and the sink's live delivery never share a frame.
                for seq in &sink_seqs {
                    assert!(
                        !replay_seqs.contains(seq),
                        "seq {seq} delivered twice within one view (replay + sink), \
                         cycle_pause_us={cycle_pause_us}"
                    );
                }
                pool.detach_sink("term_race", handle)
                    .await
                    .expect("detach should succeed");
                if cycle_pause_us > 0 {
                    tokio::time::sleep(Duration::from_micros(cycle_pause_us)).await;
                }
            }

            // Zero loss: the final held view covers 1..=100 exactly once.
            let final_replay_seqs: Vec<u64> = final_replay
                .iter()
                .filter_map(|f| match f {
                    TerminalFrame::Output { seq, .. } => Some(*seq),
                    _ => None,
                })
                .collect();
            let mut union: Vec<u64> = final_sink
                .iter()
                .chain(final_replay_seqs.iter())
                .copied()
                .collect();
            union.sort_unstable();
            let expected: Vec<u64> = (1..=100).collect();
            assert_eq!(
                union, expected,
                "the final view must cover 1..=100 exactly once each, \
                 cycle_pause_us={cycle_pause_us}"
            );
        }
    }

    #[tokio::test]
    async fn should_return_no_terminals_when_listing_empty_pool() {
        let pool = TerminalPool::with_defaults();
        let terminals = pool.list_terminals().await;
        assert!(terminals.is_empty(), "new pool should have no terminals");
    }

    #[tokio::test]
    async fn should_return_prefixed_terminal_id_when_creating_terminal() {
        let pool = TerminalPool::with_defaults();
        let id = pool
            .create_terminal(PathBuf::from("/tmp"), None, 80, 24)
            .await
            .expect("create should succeed");
        assert!(
            id.starts_with("term_"),
            "terminal ID should start with 'term_', got: {id}"
        );
        pool.close_all().await.unwrap();
    }

    #[tokio::test]
    async fn should_error_when_creating_terminal_in_pool_at_capacity() {
        let pool = TerminalPool::new(1, DEFAULT_BUFFER_SIZE);
        pool.create_terminal(PathBuf::from("/tmp"), None, 80, 24)
            .await
            .expect("first create should succeed");

        let result = pool
            .create_terminal(PathBuf::from("/tmp"), None, 80, 24)
            .await;
        assert!(result.is_err(), "second create should fail at capacity");
        pool.close_all().await.unwrap();
    }
}
