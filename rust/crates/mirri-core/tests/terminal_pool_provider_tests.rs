//! T-13c3-1 — TerminalPool behaviour through a fake [`PtyProvider`].
//!
//! These tests prove the pool is genuinely provider-injected: a scripted
//! fake emits frames without any child process, and the pool's buffering,
//! sink fan-out, replay, and lifecycle behaviour is identical to the real
//! PTY path (zero-IO T13c-3).

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

/// Serializes the env-mutating shell-path tests (they write the process
/// environment; without the gate, the two tests run in parallel and one's
/// `set_var` pollutes the other's spawn — same protection the
/// `--test-threads=1` precedent gave them, now robust to parallel runs).
static ENV_LOCK: StdMutex<()> = StdMutex::new(());

use mirri_core::terminal_pool::{
    DEFAULT_BUFFER_SIZE, DEFAULT_MAX_TERMINALS, TerminalFrame, TerminalPool, TerminalStatus,
};
use mirri_os::{DynPtyFuture, PtyConfig, PtyEvent, PtyProvider, PtySession};

/// A scripted PTY session double.
///
/// Events flow through an unbounded channel (fed by the test) so an empty
/// queue *awaits* the next event instead of reporting the channel closed —
/// the same semantics a real `PtyTerminal` has while the reader thread
/// blocks. Every `write` / `resize` / `kill` the pool delegates to it is
/// recorded.
struct ScriptedPtySession {
    /// Test-side feed for scripted events.
    event_tx: tokio::sync::mpsc::UnboundedSender<PtyEvent>,
    /// The event loop's receive end, awaiting the next scripted event.
    event_rx: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<PtyEvent>>,
    /// Bytes written through `PtySession::write`.
    writes: StdMutex<Vec<Vec<u8>>>,
    /// `(cols, rows)` pairs passed to `PtySession::resize`.
    resizes: StdMutex<Vec<(u16, u16)>>,
    /// Whether `PtySession::kill` was invoked.
    killed: StdMutex<bool>,
}

impl ScriptedPtySession {
    fn new() -> Self {
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            event_tx,
            event_rx: tokio::sync::Mutex::new(event_rx),
            writes: StdMutex::new(Vec::new()),
            resizes: StdMutex::new(Vec::new()),
            killed: StdMutex::new(false),
        }
    }

    fn push_event(&self, event: PtyEvent) {
        self.event_tx
            .send(event)
            .expect("scripted event receiver should be alive");
    }
}

impl PtySession for ScriptedPtySession {
    fn next_event(&self) -> DynPtyFuture<'_, Option<PtyEvent>> {
        Box::pin(async move {
            let mut rx = self.event_rx.lock().await;
            rx.recv().await
        })
    }

    fn write(&self, data: &[u8]) -> anyhow::Result<()> {
        self.writes.lock().unwrap().push(data.to_vec());
        Ok(())
    }

    fn resize(&self, cols: u16, rows: u16) -> anyhow::Result<()> {
        self.resizes.lock().unwrap().push((cols, rows));
        Ok(())
    }

    fn kill(&self) -> anyhow::Result<()> {
        *self.killed.lock().unwrap() = true;
        Ok(())
    }
}

/// A scripted PTY provider double: every `spawn` returns a new
/// [`ScriptedPtySession`], and each spawn config is recorded for assertions.
struct ScriptedPtyProvider {
    /// The sessions created so far (one per spawn).
    sessions: StdMutex<Vec<Arc<ScriptedPtySession>>>,
    /// The configs each spawn was called with.
    configs: StdMutex<Vec<PtyConfig>>,
}

impl ScriptedPtyProvider {
    fn new() -> Self {
        Self {
            sessions: StdMutex::new(Vec::new()),
            configs: StdMutex::new(Vec::new()),
        }
    }

    /// The session created by the Nth spawn call.
    fn session(&self, index: usize) -> Arc<ScriptedPtySession> {
        Arc::clone(&self.sessions.lock().unwrap()[index])
    }
}

impl PtyProvider for ScriptedPtyProvider {
    fn spawn(&self, config: &PtyConfig) -> anyhow::Result<Arc<dyn PtySession>> {
        self.configs.lock().unwrap().push(config.clone());
        let session = Arc::new(ScriptedPtySession::new());
        self.sessions.lock().unwrap().push(Arc::clone(&session));
        Ok(session)
    }
}

/// Poll the async `predicate` until it holds or the deadline passes,
/// panicking on timeout.
async fn wait_until<F, Fut>(predicate: F, what: &str)
where
    F: Fn() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !predicate().await {
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out waiting for {what}");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A pool over the scripted provider with default limits.
fn pool_with(provider: Arc<ScriptedPtyProvider>) -> TerminalPool {
    TerminalPool::with_provider(
        DEFAULT_MAX_TERMINALS,
        DEFAULT_BUFFER_SIZE,
        provider as Arc<dyn PtyProvider>,
    )
}

/// Given a pool over a fake provider that emits scripted frames, when the
/// frames arrive, then they are buffered with monotonically increasing seq
/// numbers and replayed by `get_output_since` in order (buffering/replay
/// behaviour unchanged, T-13c3-1).
#[tokio::test]
async fn should_buffer_scripted_frames_in_seq_order_when_fake_provider_emits_events() {
    // Given a pool backed by a scripted fake provider
    let provider = Arc::new(ScriptedPtyProvider::new());
    let pool = pool_with(Arc::clone(&provider));
    let id = pool
        .create_terminal(PathBuf::from("/tmp"), Some("/bin/fake".to_string()), 80, 24)
        .await
        .expect("create should succeed");

    // The spawn config carries the resolved shell, cwd, and dimensions.
    let configs = provider.configs.lock().unwrap().clone();
    assert_eq!(configs.len(), 1, "exactly one session should be spawned");
    assert_eq!(configs[0].program, "/bin/fake");
    assert_eq!(configs[0].cwd, PathBuf::from("/tmp"));
    assert_eq!(configs[0].cols, 80);
    assert_eq!(configs[0].rows, 24);

    let session = provider.session(0);

    // When the fake emits two data frames
    session.push_event(PtyEvent::Data {
        bytes: b"frame-alpha\n".to_vec(),
    });
    session.push_event(PtyEvent::Data {
        bytes: b"frame-beta\n".to_vec(),
    });

    wait_until(
        || async {
            pool.get_output_since(&id, 0)
                .await
                .map(|f| f.len() >= 2)
                .unwrap_or(false)
        },
        "both scripted frames to be buffered",
    )
    .await;

    // Then the buffer holds both frames with ordered seq numbers
    let frames = pool
        .get_output_since(&id, 0)
        .await
        .expect("get_output_since should succeed");
    let seqs: Vec<u64> = frames
        .iter()
        .filter_map(|f| match f {
            TerminalFrame::Output { seq, .. } => Some(*seq),
            _ => None,
        })
        .collect();
    let data: Vec<&str> = frames
        .iter()
        .filter_map(|f| match f {
            TerminalFrame::Output { data, .. } => Some(data.as_str()),
            _ => None,
        })
        .collect();
    assert!(data.contains(&"frame-alpha\n"), "got {data:?}");
    assert!(data.contains(&"frame-beta\n"), "got {data:?}");
    assert_eq!(seqs, vec![1, 2], "seq numbers should start at 1 and ascend");

    // And replay from a mid-stream seq returns only the newer frames.
    let newer = pool
        .get_output_since(&id, 1)
        .await
        .expect("replay should succeed");
    let newer_data: String = newer
        .iter()
        .filter_map(|f| match f {
            TerminalFrame::Output { data, .. } => Some(data.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        newer_data.contains("frame-beta") && !newer_data.contains("frame-alpha"),
        "replay since seq 1 should return only frame-beta, got {newer_data:?}"
    );

    pool.close_all().await.unwrap();
}

/// Given a pool over a fake provider, when a sink is attached with replay
/// mid-stream, then the replay snapshot and the live delivery partition the
/// frame stream exactly (sink/replay behaviour unchanged, T-13c3-1).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn should_partition_frames_between_replay_and_sink_when_fake_provider_emits_mid_stream() {
    // Given a pool backed by a scripted fake provider with one frame emitted
    let provider = Arc::new(ScriptedPtyProvider::new());
    let pool = pool_with(Arc::clone(&provider));
    let id = pool
        .create_terminal(PathBuf::from("/tmp"), Some("/bin/fake".to_string()), 80, 24)
        .await
        .expect("create should succeed");
    let session = provider.session(0);
    session.push_event(PtyEvent::Data {
        bytes: b"before-attach\n".to_vec(),
    });
    // Wait until the pre-attach frame is actually buffered so the attach's
    // replay snapshot deterministically includes it (the dual-lock protocol
    // partitions frames either way; this pins the interleaving under test).
    wait_until(
        || async {
            pool.get_output_since(&id, 0)
                .await
                .map(|f| !f.is_empty())
                .unwrap_or(false)
        },
        "pre-attach frame to be buffered",
    )
    .await;

    let sink_seqs = Arc::new(StdMutex::new(Vec::<u64>::new()));
    let sink_seqs_for_sink = Arc::clone(&sink_seqs);

    // When attaching with replay from seq 0 while another frame streams in
    let (handle, replay) = pool
        .attach_sink_with_replay(
            &id,
            0,
            Box::new(move |frame: TerminalFrame| {
                if let TerminalFrame::Output { seq, .. } = frame {
                    sink_seqs_for_sink.lock().unwrap().push(seq);
                }
            }),
        )
        .await
        .expect("attach should succeed");
    session.push_event(PtyEvent::Data {
        bytes: b"after-attach\n".to_vec(),
    });

    wait_until(
        || async { sink_seqs.lock().unwrap().as_slice() == [2] },
        "sink delivery of the post-attach frame",
    )
    .await;

    // Then the replay snapshot carries the pre-attach frame only, and the
    // sink saw the post-attach frame — no duplication, no loss.
    let replay_seqs: Vec<u64> = replay
        .iter()
        .filter_map(|f| match f {
            TerminalFrame::Output { seq, .. } => Some(*seq),
            _ => None,
        })
        .collect();
    assert_eq!(replay_seqs, vec![1], "replay should hold only seq 1");
    let sink = sink_seqs.lock().unwrap().clone();
    assert_eq!(sink, vec![2], "sink should have seen only seq 2");

    pool.detach_sink(&id, handle)
        .await
        .expect("detach should succeed");
    pool.close_all().await.unwrap();
}

/// Given a pool over a fake provider that emits an Exit event, when the
/// event is consumed, then the terminal's status becomes Exited with the
/// event's code and `closed_at` is set (lifecycle behaviour unchanged).
#[tokio::test]
async fn should_mark_terminal_exited_when_fake_session_emits_exit_event() {
    // Given a pool with one scripted terminal
    let provider = Arc::new(ScriptedPtyProvider::new());
    let pool = pool_with(Arc::clone(&provider));
    let id = pool
        .create_terminal(PathBuf::from("/tmp"), Some("/bin/fake".to_string()), 80, 24)
        .await
        .expect("create should succeed");
    let session = provider.session(0);

    // When the fake session emits a data frame and an exit event
    session.push_event(PtyEvent::Data {
        bytes: b"last words\n".to_vec(),
    });
    session.push_event(PtyEvent::Exit { code: Some(3) });

    wait_until(
        || async {
            pool.list_terminals()
                .await
                .iter()
                .any(|t| t.id == id && t.status == TerminalStatus::Exited(Some(3)))
        },
        "Exited(Some(3)) status",
    )
    .await;

    // Then closed_at is stamped and the buffered frames remain queryable.
    let terminals = pool.list_terminals().await;
    let info = terminals.iter().find(|t| t.id == id).unwrap();
    assert!(info.closed_at.is_some(), "closed_at should be set on exit");

    let frames = pool
        .get_output_since(&id, 0)
        .await
        .expect("get_output_since should succeed");
    assert_eq!(frames.len(), 1, "the data frame stays buffered after exit");

    pool.close_all().await.unwrap();
}

/// Given a pool over a fake provider, when `MIRRI_SHELL_PATH` is set and no
/// explicit shell is passed, then the spawned config's program carries the
/// override (D-02 — the canonical shell-path env reaches the PTY spawn).
///
/// Mutates the process environment, so it holds `ENV_LOCK` from the set
/// through the assert and cleanup (a test that panics while holding the
/// lock poisons it for the sibling test — acceptable: a poisoned lock would
/// surface the race loudly instead of silently flaking).
#[tokio::test]
#[allow(clippy::await_holding_lock)] // ENV_LOCK must span the env-window awaits: the std Mutex serializes process-global env mutation with the sibling test
async fn should_pass_mirri_shell_path_override_into_spawned_pty_config() {
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // Clean first in case a prior run leaked the var (panic before remove).
    unsafe { std::env::remove_var("MIRRI_SHELL_PATH") };
    unsafe { std::env::set_var("MIRRI_SHELL_PATH", "/opt/mirri-test-sh") };
    let provider = Arc::new(ScriptedPtyProvider::new());
    let pool = pool_with(Arc::clone(&provider));
    let _id = pool
        .create_terminal(PathBuf::from("/tmp"), None, 80, 24)
        .await
        .expect("create should succeed");
    let configs = provider.configs.lock().unwrap().clone();
    assert_eq!(configs[0].program, "/opt/mirri-test-sh");
    unsafe { std::env::remove_var("MIRRI_SHELL_PATH") };
    pool.close_all().await.unwrap();
}

/// Given a pool over a fake provider, when `MIRRICODE_SHELL_PATH` (legacy
/// alias) is set and neither an explicit shell nor the canonical env is
/// present, then the spawned config's program carries the alias value
/// (D-02 — the legacy alias stays honoured).
///
/// Mutates the process environment, so it holds `ENV_LOCK` from the set
/// through the assert and cleanup (same guard as the canonical-env test —
/// the two must never run their env windows concurrently).
#[tokio::test]
#[allow(clippy::await_holding_lock)] // ENV_LOCK must span the env-window awaits: the std Mutex serializes process-global env mutation with the sibling test
async fn should_pass_mirricode_shell_path_alias_into_spawned_pty_config() {
    let _env_guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    unsafe { std::env::remove_var("MIRRI_SHELL_PATH") };
    unsafe { std::env::remove_var("MIRRICODE_SHELL_PATH") };
    unsafe { std::env::set_var("MIRRICODE_SHELL_PATH", "/opt/legacy-test-sh") };
    let provider = Arc::new(ScriptedPtyProvider::new());
    let pool = pool_with(Arc::clone(&provider));
    let _id = pool
        .create_terminal(PathBuf::from("/tmp"), None, 80, 24)
        .await
        .expect("create should succeed");
    let configs = provider.configs.lock().unwrap().clone();
    assert_eq!(configs[0].program, "/opt/legacy-test-sh");
    unsafe { std::env::remove_var("MIRRICODE_SHELL_PATH") };
    pool.close_all().await.unwrap();
}

/// Given a pool over a fake provider, when write/resize/kill are called,
/// then each is delegated to the injected session (the pool holds the
/// session purely through the `Arc<dyn PtySession>` trait object,
/// T-13c3-1).
#[tokio::test]
async fn should_delegate_write_resize_and_kill_to_the_injected_session() {
    // Given a pool with one scripted terminal that stays alive
    let provider = Arc::new(ScriptedPtyProvider::new());
    let pool = pool_with(Arc::clone(&provider));
    let id = pool
        .create_terminal(PathBuf::from("/tmp"), Some("/bin/fake".to_string()), 80, 24)
        .await
        .expect("create should succeed");
    let session = provider.session(0);

    // When the pool's I/O surface is exercised
    pool.write(&id, "ls -la\n")
        .await
        .expect("write should succeed");
    pool.resize(&id, 120, 40)
        .await
        .expect("resize should succeed");
    pool.kill(&id).await.expect("kill should succeed");

    // Then the session double observed every call
    assert_eq!(
        session.writes.lock().unwrap().as_slice(),
        [b"ls -la\n".to_vec()].as_slice(),
        "write should reach the injected session"
    );
    assert_eq!(
        session.resizes.lock().unwrap().as_slice(),
        [(120u16, 40u16)].as_slice(),
        "resize should reach the injected session"
    );
    assert!(
        *session.killed.lock().unwrap(),
        "kill should reach the injected session"
    );

    pool.close_all().await.unwrap();
}
