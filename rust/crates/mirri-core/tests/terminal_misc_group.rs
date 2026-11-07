// layer: T2
// Domain group binary for the terminal_misc domain — consolidated from per-file
// test binaries per 16-test-execution-strategy.md §4.4 (pure file moves).

mod terminal_pool_ {
//! Tests for the `TerminalPool` — session-level PTY terminal management.
//!
//! These tests exercise the full lifecycle of terminals in a pool:
//! - Creation and initial state
//! - Writing data and receiving output frames
//! - Resizing terminals
//! - Killing terminals and verifying exit status
//! - Getting output since a specific sequence number
//! - Attaching and detaching real-time sinks
//! - Listing terminal info
//! - `close_all` cleanup
//! - Bounded buffer eviction
//! - Multiple terminals coexisting
//!
//! PTY tests require real child processes.  We use `/bin/cat` (echoes stdin
//! to stdout, stays alive until killed) and `/bin/echo` (writes output and
//! exits) as simple test programs.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use mirri_core::terminal_pool::{
    DEFAULT_BUFFER_SIZE, SinkHandle, TerminalFrame, TerminalPool, TerminalStatus,
};

// ---------------------------------------------------------------------------
// Helper: wait for at least N output frames from a terminal.
// ---------------------------------------------------------------------------

/// Poll `get_output_since(0)` until at least `min_frames` frames are available
/// or the deadline passes.
async fn wait_for_output(
    pool: &TerminalPool,
    terminal_id: &str,
    min_frames: usize,
    timeout: Duration,
) -> Vec<TerminalFrame> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let frames = pool
            .get_output_since(terminal_id, 0)
            .await
            .expect("get_output_since should succeed");
        if frames.len() >= min_frames {
            return frames;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "timed out waiting for {min_frames} frames, got {} frames: {frames:?}",
                frames.len()
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Wait for a terminal to reach the given status, or panic after timeout.
async fn wait_for_status(
    pool: &TerminalPool,
    terminal_id: &str,
    expected: TerminalStatus,
    timeout: Duration,
) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let terminals = pool.list_terminals().await;
        if let Some(info) = terminals.iter().find(|t| t.id == terminal_id)
            && info.status == expected
        {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out waiting for terminal status {expected:?}");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_return_empty_vec_when_pool_is_empty() {
    // Given a newly created terminal pool
    let pool = TerminalPool::with_defaults();

    // When listing terminals
    let terminals = pool.list_terminals().await;

    // Then the list is empty
    assert!(
        terminals.is_empty(),
        "a new pool should have zero terminals"
    );
    assert!(pool.is_empty().await);
    assert_eq!(pool.len().await, 0);
}

#[tokio::test]
async fn should_return_term_prefixed_id_when_terminal_created() {
    // Given an empty pool
    let pool = TerminalPool::with_defaults();

    // When creating a terminal
    let id = pool
        .create_terminal(PathBuf::from("/tmp"), None, 80, 24)
        .await
        .expect("create should succeed");

    // Then the ID has the expected format
    assert!(
        id.starts_with("term_"),
        "terminal ID should start with 'term_', got: {id}"
    );
    assert!(
        id.len() > "term_".len(),
        "terminal ID should have a UUID suffix"
    );

    // Cleanup
    pool.close_all().await.unwrap();
}

#[tokio::test]
async fn should_show_running_status_and_metadata_when_terminal_listed() {
    // Given a pool with one terminal using /bin/echo
    let pool = TerminalPool::with_defaults();
    let id = pool
        .create_terminal(
            PathBuf::from("/tmp"),
            Some("/bin/echo".to_string()),
            100,
            30,
        )
        .await
        .expect("create should succeed");

    // When listing terminals
    let terminals = pool.list_terminals().await;

    // Then there is exactly one with the right metadata
    assert_eq!(terminals.len(), 1, "should have one terminal");
    let info = &terminals[0];
    assert_eq!(info.id, id);
    assert_eq!(info.cols, 100);
    assert_eq!(info.rows, 30);
    assert_eq!(info.shell, "/bin/echo");
    assert_eq!(info.status, TerminalStatus::Running);
    assert!(info.closed_at.is_none());

    // Cleanup
    pool.close_all().await.unwrap();
}

#[tokio::test]
async fn should_produce_output_frames_when_cat_terminal_written_to() {
    // Given a pool with a /bin/cat terminal
    let pool = TerminalPool::with_defaults();
    let id = pool
        .create_terminal(PathBuf::from("/tmp"), Some("/bin/cat".to_string()), 80, 24)
        .await
        .expect("create should succeed");

    // Wait for cat to start (it may output nothing initially)
    tokio::time::sleep(Duration::from_millis(200)).await;

    // When writing data to the terminal
    pool.write(&id, "hello-world\n")
        .await
        .expect("write should succeed");

    // Then output frames containing the written text should appear
    let frames = wait_for_output(&pool, &id, 1, Duration::from_secs(5)).await;
    let combined: String = frames
        .iter()
        .filter_map(|f| match f {
            TerminalFrame::Output { data, .. } => Some(data.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        combined.contains("hello-world"),
        "output should contain 'hello-world', got: {combined}"
    );

    // Cleanup
    pool.kill(&id).await.unwrap();
    pool.close_all().await.unwrap();
}

#[tokio::test]
async fn should_not_error_when_terminal_resized() {
    // Given a pool with a /bin/cat terminal
    let pool = TerminalPool::with_defaults();
    let id = pool
        .create_terminal(PathBuf::from("/tmp"), Some("/bin/cat".to_string()), 80, 24)
        .await
        .expect("create should succeed");

    tokio::time::sleep(Duration::from_millis(200)).await;

    // When resizing the terminal
    let result = pool.resize(&id, 120, 40).await;

    // Then it succeeds without error
    assert!(result.is_ok(), "resize should succeed: {:?}", result.err());

    // Cleanup
    pool.kill(&id).await.unwrap();
    pool.close_all().await.unwrap();
}

#[tokio::test]
async fn should_have_exited_status_when_echo_terminal_exits() {
    // Given a pool with a /bin/echo terminal (echo writes output then exits)
    let pool = TerminalPool::with_defaults();
    let id = pool
        .create_terminal(PathBuf::from("/tmp"), Some("/bin/echo".to_string()), 80, 24)
        .await
        .expect("create should succeed");

    // When the echo process runs and exits
    // (echo exits immediately after printing)
    wait_for_status(
        &pool,
        &id,
        TerminalStatus::Exited(Some(0)),
        Duration::from_secs(5),
    )
    .await;

    // Then the terminal status is Exited with code 0
    let terminals = pool.list_terminals().await;
    let info = terminals
        .iter()
        .find(|t| t.id == id)
        .expect("terminal should be in the list");
    assert_eq!(
        info.status,
        TerminalStatus::Exited(Some(0)),
        "echo should exit with code 0"
    );
    assert!(
        info.closed_at.is_some(),
        "closed_at should be set after exit"
    );

    // Cleanup
    pool.close_all().await.unwrap();
}

#[tokio::test]
async fn should_have_exited_status_when_terminal_killed() {
    // Given a pool with a /bin/cat terminal (cat runs until killed)
    let pool = TerminalPool::with_defaults();
    let id = pool
        .create_terminal(PathBuf::from("/tmp"), Some("/bin/cat".to_string()), 80, 24)
        .await
        .expect("create should succeed");

    tokio::time::sleep(Duration::from_millis(200)).await;

    // When killing the terminal
    pool.kill(&id).await.expect("kill should succeed");

    // Then the status becomes Exited
    // (cat may exit with a signal, so code might not be 0)
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let terminals = pool.list_terminals().await;
        let info = terminals
            .iter()
            .find(|t| t.id == id)
            .expect("terminal should be in the list");
        if info.status != TerminalStatus::Running {
            assert!(
                matches!(info.status, TerminalStatus::Exited(_)),
                "status should be Exited after kill, got: {:?}",
                info.status
            );
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out waiting for terminal to exit after kill");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Cleanup
    pool.close_all().await.unwrap();
}

/// Given a /bin/cat terminal with prior output, when attaching with replay
/// while the terminal keeps producing output, then the replay plus the live
/// sink delivery cover every frame exactly once — no duplicate seq, and
/// every written marker reaches the caller (F-P3-1, real-PTY variant).
#[tokio::test]
async fn should_lose_no_frames_when_attaching_with_replay_while_output_streams() {
    // Given a cat terminal and 30 lines of output already produced
    let pool = Arc::new(TerminalPool::with_defaults());
    let id = pool
        .create_terminal(PathBuf::from("/tmp"), Some("/bin/cat".to_string()), 80, 24)
        .await
        .expect("create should succeed");

    tokio::time::sleep(Duration::from_millis(200)).await;
    for i in 0..30 {
        pool.write(&id, &format!("warm-{i}\n"))
            .await
            .expect("write should succeed");
    }
    // Wait for the last warm marker (cat may coalesce writes into fewer
    // frames, so frame counting is not a reliable readiness signal).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let warmup = loop {
        let frames = pool
            .get_output_since(&id, 0)
            .await
            .expect("get_output_since should succeed");
        let combined: String = frames
            .iter()
            .filter_map(|f| match f {
                TerminalFrame::Output { data, .. } => Some(data.as_str()),
                _ => None,
            })
            .collect();
        if combined.contains("warm-29") || tokio::time::Instant::now() >= deadline {
            break frames;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let last_seq = warmup
        .iter()
        .filter_map(|f| match f {
            TerminalFrame::Output { seq, .. } => Some(*seq),
            _ => None,
        })
        .max()
        .unwrap_or(0);

    // When attaching with replay while another 100 lines stream in
    let sink_seqs = Arc::new(StdMutex::new(Vec::<u64>::new()));
    let sink_data = Arc::new(StdMutex::new(String::new()));
    let sink_seqs_for_sink = Arc::clone(&sink_seqs);
    let sink_data_for_sink = Arc::clone(&sink_data);
    let writer_pool = Arc::clone(&pool);
    let writer_id = id.clone();
    let writer = tokio::spawn(async move {
        for i in 0..100 {
            writer_pool
                .write(&writer_id, &format!("live-{i}\n"))
                .await
                .expect("write should succeed");
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    });

    let (_handle, replay) = pool
        .attach_sink_with_replay(
            &id,
            last_seq,
            Box::new(move |frame: TerminalFrame| {
                if let TerminalFrame::Output { seq, data, .. } = frame {
                    sink_seqs_for_sink.lock().unwrap().push(seq);
                    sink_data_for_sink.lock().unwrap().push_str(&data);
                }
            }),
        )
        .await
        .expect("attach with replay should succeed");

    writer.await.expect("writer should finish");

    // Then no seq is both replayed and sink-delivered (zero duplication)…
    let sink_seqs = sink_seqs.lock().unwrap().clone();
    for seq in &sink_seqs {
        assert!(
            replay
                .iter()
                .all(|f| !matches!(f, TerminalFrame::Output { seq: s, .. } if s == seq)),
            "seq {seq} was both replayed and sink-delivered"
        );
    }
    // …and every live marker reaches the caller through either path
    // (zero loss — cat may coalesce writes, so the assertion is on the
    // combined data, not per-seq contiguity).
    let replay_data: String = replay
        .iter()
        .filter_map(|f| match f {
            TerminalFrame::Output { data, .. } => Some(data.as_str()),
            _ => None,
        })
        .collect();
    let combined = format!("{replay_data}{}", sink_data.lock().unwrap());
    for i in 0..100 {
        assert!(
            combined.contains(&format!("live-{i}")),
            "marker 'live-{i}' never reached the caller, combined={combined}"
        );
    }

    // Cleanup
    pool.kill(&id).await.unwrap();
    pool.close_all().await.unwrap();
}

#[tokio::test]
async fn should_return_only_newer_frames_when_get_output_since_called() {
    // Given a pool with a /bin/cat terminal that has produced output
    let pool = TerminalPool::with_defaults();
    let id = pool
        .create_terminal(PathBuf::from("/tmp"), Some("/bin/cat".to_string()), 80, 24)
        .await
        .expect("create should succeed");

    tokio::time::sleep(Duration::from_millis(200)).await;

    // Write some data to produce output frames
    pool.write(&id, "first-line\n")
        .await
        .expect("write should succeed");
    let first_frames = wait_for_output(&pool, &id, 1, Duration::from_secs(5)).await;

    // Get the seq of the last frame we've seen
    let last_seq = first_frames
        .iter()
        .filter_map(|f| match f {
            TerminalFrame::Output { seq, .. } => Some(*seq),
            _ => None,
        })
        .max()
        .unwrap_or(0);

    // When writing more data and querying since last_seq
    pool.write(&id, "second-line\n")
        .await
        .expect("write should succeed");

    // Then only frames with seq > last_seq are returned. Poll for the
    // newer output instead of a fixed sleep: under a fully parallel test
    // run, cat's echo can land well after any fixed window (F-P3-5).
    let newer_frames = loop {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let frames = pool
            .get_output_since(&id, last_seq)
            .await
            .expect("get_output_since should succeed");
        let combined: String = frames
            .iter()
            .filter_map(|f| match f {
                TerminalFrame::Output { data, .. } => Some(data.as_str()),
                _ => None,
            })
            .collect();
        if combined.contains("second-line") || tokio::time::Instant::now() >= deadline {
            break frames;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    for frame in &newer_frames {
        if let TerminalFrame::Output { seq, .. } = frame {
            assert!(
                *seq > last_seq,
                "frame with seq {seq} should be > last_seq {last_seq}"
            );
        }
    }

    // And the newer output should contain "second-line"
    let combined: String = newer_frames
        .iter()
        .filter_map(|f| match f {
            TerminalFrame::Output { data, .. } => Some(data.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        combined.contains("second-line"),
        "newer frames should contain 'second-line', got: {combined}"
    );

    // Cleanup
    pool.kill(&id).await.unwrap();
    pool.close_all().await.unwrap();
}

#[tokio::test]
async fn should_receive_frames_when_sink_attached_and_output_produced() {
    // Given a pool with a /bin/cat terminal
    let pool = TerminalPool::with_defaults();
    let id = pool
        .create_terminal(PathBuf::from("/tmp"), Some("/bin/cat".to_string()), 80, 24)
        .await
        .expect("create should succeed");

    tokio::time::sleep(Duration::from_millis(200)).await;

    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = Arc::clone(&counter);

    // When attaching a sink and writing data
    let _handle: SinkHandle = pool
        .attach_sink(
            &id,
            Box::new(move |_frame| {
                counter_clone.fetch_add(1, Ordering::SeqCst);
            }),
        )
        .await
        .expect("attach_sink should succeed");

    pool.write(&id, "sink-test\n")
        .await
        .expect("write should succeed");

    // Then the sink should receive at least one frame
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if counter.load(Ordering::SeqCst) > 0 {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("sink did not receive any frames within timeout");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Cleanup
    pool.kill(&id).await.unwrap();
    pool.close_all().await.unwrap();
}

#[tokio::test]
async fn should_stop_receiving_frames_when_sink_detached() {
    // Given a pool with a /bin/cat terminal and an attached sink
    let pool = TerminalPool::with_defaults();
    let id = pool
        .create_terminal(PathBuf::from("/tmp"), Some("/bin/cat".to_string()), 80, 24)
        .await
        .expect("create should succeed");

    tokio::time::sleep(Duration::from_millis(200)).await;

    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = Arc::clone(&counter);

    let handle = pool
        .attach_sink(
            &id,
            Box::new(move |_frame| {
                counter_clone.fetch_add(1, Ordering::SeqCst);
            }),
        )
        .await
        .expect("attach_sink should succeed");

    // Write to verify the sink works
    pool.write(&id, "before-detach\n")
        .await
        .expect("write should succeed");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if counter.load(Ordering::SeqCst) > 0 {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("sink should have received frames before detach");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let count_before = counter.load(Ordering::SeqCst);

    // When detaching the sink
    pool.detach_sink(&id, handle)
        .await
        .expect("detach_sink should succeed");

    tokio::time::sleep(Duration::from_millis(100)).await;

    // Write more data
    pool.write(&id, "after-detach\n")
        .await
        .expect("write should succeed");
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Then the counter should not have increased significantly
    // (there might be a tiny race, but the count should be stable)
    let count_after = counter.load(Ordering::SeqCst);
    assert!(
        count_after <= count_before + 1,
        "sink should not receive frames after detach: before={count_before}, after={count_after}"
    );

    // Cleanup
    pool.kill(&id).await.unwrap();
    pool.close_all().await.unwrap();
}

#[tokio::test]
async fn should_list_all_terminals_when_multiple_terminals_exist() {
    // Given a pool with multiple terminals
    let pool = TerminalPool::new(5, DEFAULT_BUFFER_SIZE);

    let id1 = pool
        .create_terminal(PathBuf::from("/tmp"), Some("/bin/cat".to_string()), 80, 24)
        .await
        .expect("create should succeed");
    let id2 = pool
        .create_terminal(PathBuf::from("/tmp"), Some("/bin/cat".to_string()), 80, 24)
        .await
        .expect("create should succeed");
    let id3 = pool
        .create_terminal(PathBuf::from("/tmp"), Some("/bin/cat".to_string()), 80, 24)
        .await
        .expect("create should succeed");

    // When listing terminals
    let terminals = pool.list_terminals().await;

    // Then all three are present
    assert_eq!(terminals.len(), 3, "should have three terminals");
    let ids: Vec<&str> = terminals.iter().map(|t| t.id.as_str()).collect();
    assert!(ids.contains(&id1.as_str()), "id1 should be in the list");
    assert!(ids.contains(&id2.as_str()), "id2 should be in the list");
    assert!(ids.contains(&id3.as_str()), "id3 should be in the list");

    // Cleanup
    pool.close_all().await.unwrap();
}

#[tokio::test]
async fn should_be_empty_when_close_all_called() {
    // Given a pool with multiple terminals
    let pool = TerminalPool::new(5, DEFAULT_BUFFER_SIZE);
    pool.create_terminal(PathBuf::from("/tmp"), Some("/bin/cat".to_string()), 80, 24)
        .await
        .expect("create should succeed");
    pool.create_terminal(PathBuf::from("/tmp"), Some("/bin/cat".to_string()), 80, 24)
        .await
        .expect("create should succeed");

    assert_eq!(
        pool.len().await,
        2,
        "should have two terminals before close_all"
    );

    // When calling close_all
    pool.close_all().await.expect("close_all should succeed");

    // Then the pool is empty
    assert!(
        pool.is_empty().await,
        "pool should be empty after close_all"
    );
    assert_eq!(
        pool.len().await,
        0,
        "pool should have zero terminals after close_all"
    );
}

#[tokio::test]
async fn should_evict_old_frames_when_many_frames_produced_with_small_buffer() {
    // Given a pool with a very small buffer (3 frames)
    let pool = TerminalPool::new(5, 3);
    let id = pool
        .create_terminal(PathBuf::from("/tmp"), Some("/bin/cat".to_string()), 80, 24)
        .await
        .expect("create should succeed");

    tokio::time::sleep(Duration::from_millis(200)).await;

    // When writing multiple lines of data
    for i in 0..10 {
        pool.write(&id, &format!("line-{i}\n"))
            .await
            .expect("write should succeed");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Wait for output to settle
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Then the buffer should not exceed its max size
    let all_frames = pool
        .get_output_since(&id, 0)
        .await
        .expect("get_output_since should succeed");

    // The buffer is bounded to 3 frames, so we should have at most 3
    assert!(
        all_frames.len() <= 3,
        "buffer should have at most 3 frames, got {}",
        all_frames.len()
    );

    // Cleanup
    pool.kill(&id).await.unwrap();
    pool.close_all().await.unwrap();
}

#[tokio::test]
async fn should_return_error_when_creating_terminal_at_pool_capacity() {
    // Given a pool with max 1 terminal
    let pool = TerminalPool::new(1, DEFAULT_BUFFER_SIZE);
    pool.create_terminal(PathBuf::from("/tmp"), Some("/bin/cat".to_string()), 80, 24)
        .await
        .expect("first create should succeed");

    // When creating a second terminal
    let result = pool
        .create_terminal(PathBuf::from("/tmp"), Some("/bin/cat".to_string()), 80, 24)
        .await;

    // Then it returns an error
    assert!(
        result.is_err(),
        "creating a second terminal should fail when pool is at capacity"
    );

    // Cleanup
    pool.close_all().await.unwrap();
}

#[tokio::test]
async fn should_return_error_when_writing_to_nonexistent_terminal() {
    // Given an empty pool
    let pool = TerminalPool::with_defaults();

    // When writing to a nonexistent terminal
    let result = pool.write("term_nonexistent", "data").await;

    // Then it returns an error
    assert!(
        result.is_err(),
        "writing to a nonexistent terminal should fail"
    );
}

#[tokio::test]
async fn should_have_correct_type_tag_when_output_frame_serialized() {
    // Given a TerminalFrame::Output
    let frame = TerminalFrame::Output {
        terminal_id: "term_abc".to_string(),
        seq: 42,
        data: "hello".to_string(),
    };

    // When serializing to JSON
    let json = serde_json::to_value(&frame).expect("serialize should succeed");

    // Then the type tag is correct
    assert_eq!(json["type"], "terminal_output");
    assert_eq!(json["terminal_id"], "term_abc");
    assert_eq!(json["seq"], 42);
    assert_eq!(json["data"], "hello");
}

#[tokio::test]
async fn should_have_correct_type_tag_when_exit_frame_serialized() {
    // Given a TerminalFrame::Exit
    let frame = TerminalFrame::Exit {
        terminal_id: "term_abc".to_string(),
        code: Some(0),
    };

    // When serializing to JSON
    let json = serde_json::to_value(&frame).expect("serialize should succeed");

    // Then the type tag is correct
    assert_eq!(json["type"], "terminal_exit");
    assert_eq!(json["terminal_id"], "term_abc");
    assert_eq!(json["code"], 0);
}

#[tokio::test]
async fn should_have_monotonically_increasing_seq_numbers_when_terminal_produces_output() {
    // Given a pool with a /bin/cat terminal
    let pool = TerminalPool::with_defaults();
    let id = pool
        .create_terminal(PathBuf::from("/tmp"), Some("/bin/cat".to_string()), 80, 24)
        .await
        .expect("create should succeed");

    tokio::time::sleep(Duration::from_millis(200)).await;

    // When writing multiple times
    pool.write(&id, "aaa\n").await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    pool.write(&id, "bbb\n").await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    pool.write(&id, "ccc\n").await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Then the seq numbers in the output frames are monotonically increasing
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

    assert!(!seqs.is_empty(), "should have at least some output frames");

    for i in 1..seqs.len() {
        assert!(
            seqs[i] > seqs[i - 1],
            "seq numbers should be monotonically increasing: {:?}",
            seqs
        );
    }

    // Cleanup
    pool.kill(&id).await.unwrap();
    pool.close_all().await.unwrap();
}

}

mod terminal_pool_provider_ {
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

// Env-mutating tests serialize on the workspace-wide env lock
// (mirri-test-support).

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
/// Mutates the process environment, so it holds the workspace env lock from the set
/// through the assert and cleanup (a test that panics while holding the
/// lock poisons it for the sibling test — acceptable: a poisoned lock would
/// surface the race loudly instead of silently flaking).
#[tokio::test]
#[allow(clippy::await_holding_lock)] // ENV_LOCK must span the env-window awaits: the std Mutex serializes process-global env mutation with the sibling test
async fn should_pass_mirri_shell_path_override_into_spawned_pty_config() {
    let _env_guard = mirri_test_support::env::lock();
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
/// Mutates the process environment, so it holds the workspace env lock from the set
/// through the assert and cleanup (same guard as the canonical-env test —
/// the two must never run their env windows concurrently).
#[tokio::test]
#[allow(clippy::await_holding_lock)] // ENV_LOCK must span the env-window awaits: the std Mutex serializes process-global env mutation with the sibling test
async fn should_pass_mirricode_shell_path_alias_into_spawned_pty_config() {
    let _env_guard = mirri_test_support::env::lock();
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

}

mod loop_engine_ {
//! Comprehensive tests for the loop engine module.
//!
//! These tests use a [`MockLlm`] that returns configurable responses from a
//! queue, and [`MockHooks`] that allows controlling hook behavior.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use futures::stream;
use serde_json::Value;

use mirri_core::PromptOrigin;
use mirri_core::cancel::CancelToken;
use mirri_core::context_memory::ContextMemory;
use mirri_core::error::{EngineError, EngineResult};
use mirri_core::hooks::*;
use mirri_core::llm::*;
use mirri_core::loop_engine::*;
use mirri_core::message::{ContentPart, ImageUrlData, Message};
use mirri_core::tool::*;
use mirri_core::tool_registry::{ToolManager, ValidatorCache};
use mirri_core::tools::ReadMediaFileTool;

// ---------------------------------------------------------------------------
// Mock LLM
// ---------------------------------------------------------------------------

/// A pre-configured response for the MockLlm to return.
#[derive(Clone)]
struct MockResponse {
    text: String,
    think_text: String,
    tool_calls: Vec<LlmToolCall>,
    /// Streamed tool-call argument fragments emitted before the block-level
    /// `ToolCall`s (index, fragment, call id, tool name).
    tool_parts: Vec<(u64, String, Option<String>, Option<String>)>,
    stop_reason: String,
    usage: LlmUsage,
    error: Option<String>,
    /// When set, the stream ends after the delivered text with no `Usage` and
    /// no `Done` (mid-stream cancellation shape).
    interrupted: bool,
}

impl MockResponse {
    fn end_turn(text: &str) -> Self {
        Self {
            text: text.to_string(),
            think_text: String::new(),
            tool_calls: Vec::new(),
            tool_parts: Vec::new(),
            stop_reason: "completed".to_string(),
            usage: LlmUsage {
                input_tokens: 100,
                output_tokens: 50,
                ..Default::default()
            },
            error: None,
            interrupted: false,
        }
    }

    fn tool_use(calls: Vec<LlmToolCall>) -> Self {
        Self {
            text: String::new(),
            think_text: String::new(),
            tool_calls: calls,
            tool_parts: Vec::new(),
            stop_reason: "tool_calls".to_string(),
            usage: LlmUsage {
                input_tokens: 100,
                output_tokens: 50,
                ..Default::default()
            },
            error: None,
            interrupted: false,
        }
    }

    fn with_tool_parts(
        mut self,
        parts: Vec<(u64, String, Option<String>, Option<String>)>,
    ) -> Self {
        self.tool_parts = parts;
        self
    }

    fn with_usage(mut self, input: u64, output: u64) -> Self {
        self.usage = LlmUsage {
            input_tokens: input,
            output_tokens: output,
            ..Default::default()
        };
        self
    }

    /// A response whose stream is cancelled mid-flight: the partial text is
    /// delivered (delta + block part) but the stream ends with no `Usage` and
    /// no `Done` event — the shape the provider pump produces when the cancel
    /// token fires mid-stream.
    fn interrupted_after(text: &str) -> Self {
        Self {
            text: text.to_string(),
            think_text: String::new(),
            tool_calls: Vec::new(),
            tool_parts: Vec::new(),
            stop_reason: String::new(),
            usage: LlmUsage::default(),
            error: None,
            interrupted: true,
        }
    }

    fn error(message: &str) -> Self {
        Self {
            text: String::new(),
            think_text: String::new(),
            tool_calls: Vec::new(),
            tool_parts: Vec::new(),
            stop_reason: String::new(),
            usage: LlmUsage::default(),
            error: Some(message.to_string()),
            interrupted: false,
        }
    }

    /// A degenerate response: usage was billed and the stream ended with a
    /// clean natural stop, but no text, no thinking, and no tool calls were
    /// delivered — the shape observed when a model burns output tokens yet
    /// produces nothing visible.
    fn empty_end_turn() -> Self {
        Self::empty_with_stop_reason("completed")
    }

    /// The same degenerate empty response under an arbitrary stop reason —
    /// every non-tool-use, non-filtered stop settles as "completed" on the
    /// wire, so an empty response must fail regardless of which one the
    /// provider used.
    fn empty_with_stop_reason(stop_reason: &str) -> Self {
        Self {
            text: String::new(),
            think_text: String::new(),
            tool_calls: Vec::new(),
            tool_parts: Vec::new(),
            stop_reason: stop_reason.to_string(),
            usage: LlmUsage {
                input_tokens: 8129,
                output_tokens: 109,
                ..Default::default()
            },
            error: None,
            interrupted: false,
        }
    }

    /// An empty stream that ends mid-flight because the cancel token fired —
    /// no text, no `Usage`, no `Done`. The cancellation closure owns this
    /// shape, never the empty-response guard.
    fn interrupted_empty() -> Self {
        Self {
            text: String::new(),
            think_text: String::new(),
            tool_calls: Vec::new(),
            tool_parts: Vec::new(),
            stop_reason: String::new(),
            usage: LlmUsage::default(),
            error: None,
            interrupted: true,
        }
    }
}

/// A mock LLM that pops responses from a queue.
struct MockLlm {
    system_prompt: String,
    model_name: String,
    responses: Arc<Mutex<Vec<MockResponse>>>,
    retryable: bool,
    /// When set, the token is cancelled at the moment `chat()` is invoked —
    // simulating a mid-stream cancellation (the engine sees a stream that
    /// ends without `Done` while the token is set).
    cancel_on_chat: Option<CancelToken>,
    /// The fixed capability reported to the engine. Defaults to a
    /// text-only model (every modality false — undefined is unsupported,
    /// ADR-003 revision); tests opt into media support via
    /// [`with_capability`](Self::with_capability).
    capability: ModelCapability,
    /// Captured per-call request message arrays (for snapshot assertions).
    requests: Arc<Mutex<Vec<Vec<Message>>>>,
    /// Captured per-call tool sets (for tool-visibility assertions).
    tool_sets: Arc<Mutex<Vec<Vec<ToolDefinition>>>>,
}

impl MockLlm {
    fn new(responses: Vec<MockResponse>) -> Self {
        Self {
            system_prompt: "You are a test assistant.".to_string(),
            model_name: "test-model".to_string(),
            responses: Arc::new(Mutex::new(responses)),
            retryable: false,
            cancel_on_chat: None,
            capability: ModelCapability::default(),
            requests: Arc::new(Mutex::new(Vec::new())),
            tool_sets: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn with_retryable(mut self, retryable: bool) -> Self {
        self.retryable = retryable;
        self
    }

    fn with_capability(mut self, capability: ModelCapability) -> Self {
        self.capability = capability;
        self
    }

    fn with_cancel_on_chat(mut self, cancel: CancelToken) -> Self {
        self.cancel_on_chat = Some(cancel);
        self
    }

    /// Number of `chat` calls this instance has served — per-instance
    /// assertions for mid-turn model-switch tests.
    fn served_calls(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

impl Llm for MockLlm {
    fn system_prompt(&self) -> &str {
        &self.system_prompt
    }

    fn model_name(&self) -> &str {
        &self.model_name
    }

    fn capability(&self) -> &ModelCapability {
        &self.capability
    }

    fn is_retryable_error(&self, _error: &ProviderErrorInfo) -> bool {
        self.retryable
    }

    fn chat(&self, params: LlmChatParams) -> EngineResult<LlmEventStream> {
        self.requests.lock().unwrap().push(params.messages.clone());
        self.tool_sets.lock().unwrap().push(params.tools.clone());
        if let Some(cancel) = &self.cancel_on_chat {
            cancel.cancel();
        }
        let mut queue = self.responses.lock().unwrap();
        if queue.is_empty() {
            return Err(EngineError::ProviderError(
                "No more mock responses".to_string(),
            ));
        }
        let resp = queue.remove(0);
        drop(queue);

        let events = build_events(&resp);
        let stream = stream::iter(events);
        Ok(Box::pin(stream))
    }
}

/// Build the list of `LlmEvent`s for a `MockResponse`.
fn build_events(resp: &MockResponse) -> Vec<LlmEvent> {
    let mut events = Vec::new();

    if let Some(ref err) = resp.error {
        events.push(LlmEvent::Error {
            message: err.clone(),
        });
        return events;
    }

    if !resp.text.is_empty() {
        events.push(LlmEvent::TextDelta {
            text: resp.text.clone(),
        });
        events.push(LlmEvent::TextPart {
            text: resp.text.clone(),
        });
    }

    // A mid-stream cancellation ends the stream right after the delivered
    // text — no usage, no stop reason.
    if resp.interrupted {
        return events;
    }

    if !resp.think_text.is_empty() {
        events.push(LlmEvent::ThinkPart {
            text: resp.think_text.clone(),
        });
    }

    for (index, fragment, call_id, tool_name) in &resp.tool_parts {
        events.push(LlmEvent::ToolCallPart {
            index: *index,
            arguments_part: fragment.clone(),
            call_id: call_id.clone(),
            tool_name: tool_name.clone(),
        });
    }

    for tc in &resp.tool_calls {
        events.push(LlmEvent::ToolCall {
            tool: tc.name.clone(),
            args: tc.args.clone(),
            call_id: tc.id.clone(),
        });
    }

    events.push(LlmEvent::Usage {
        input_tokens: resp.usage.input_tokens,
        output_tokens: resp.usage.output_tokens,
        cache_read_tokens: resp.usage.cache_read_tokens,
        cache_creation_tokens: resp.usage.cache_creation_tokens,
    });

    events.push(LlmEvent::Done {
        stop_reason: resp.stop_reason.clone(),
    });

    events
}

// ---------------------------------------------------------------------------
// Mock Tool
// ---------------------------------------------------------------------------

/// A simple echo tool for testing.
struct EchoTool;

impl Tool for EchoTool {
    fn name(&self) -> &str {
        "Echo"
    }

    fn description(&self) -> &str {
        "Echoes back the input"
    }

    fn parameters(&self) -> &Value {
        static SCHEMA: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "message": { "type": "string" }
                },
                "required": ["message"]
            })
        })
    }

    fn resolve_execution(&self, input: Value) -> EngineResult<ToolExecution> {
        let msg = input
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("no message")
            .to_string();
        Ok(ToolExecution::Runnable {
            plan: ExecutionPlan {
                accesses: Vec::new(),
                display: ToolInputDisplay::Generic {
                    summary: format!("Echo: {}", msg),
                },
                approval_rule: "Echo".to_string(),
                stop_batch_after_this: false,
            },
            execute: Box::new(move |_ctx| Box::pin(async move { ToolResult::success(msg) })),
        })
    }
}

/// A tool that reports incremental updates through `ctx.progress` before
/// finishing — exercises the live tool-progress channel end to end.
struct ProgressingTool;

impl Tool for ProgressingTool {
    fn name(&self) -> &str {
        "Progressing"
    }

    fn description(&self) -> &str {
        "Emits progress updates then finishes"
    }

    fn parameters(&self) -> &Value {
        static SCHEMA: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| serde_json::json!({"type": "object", "properties": {}}))
    }

    fn resolve_execution(&self, _input: Value) -> EngineResult<ToolExecution> {
        Ok(ToolExecution::Runnable {
            plan: ExecutionPlan {
                accesses: Vec::new(),
                display: ToolInputDisplay::Generic {
                    summary: "Progressing".to_string(),
                },
                approval_rule: "Progressing".to_string(),
                stop_batch_after_this: false,
            },
            execute: Box::new(move |ctx| {
                Box::pin(async move {
                    ctx.progress.send(ToolUpdate {
                        kind: ToolUpdateKind::Progress,
                        text: Some("step one".to_string()),
                        percent: Some(25.0),
                        custom_kind: None,
                        custom_data: None,
                    });
                    ctx.progress.send(ToolUpdate {
                        kind: ToolUpdateKind::Status,
                        text: Some("step two".to_string()),
                        percent: Some(75.0),
                        custom_kind: None,
                        custom_data: None,
                    });
                    ToolResult::success("done")
                })
            }),
        })
    }
}

/// A tool that sets stop_turn on its result.
struct StopTurnTool;

impl Tool for StopTurnTool {
    fn name(&self) -> &str {
        "StopTurn"
    }

    fn description(&self) -> &str {
        "A tool that stops the turn"
    }

    fn parameters(&self) -> &Value {
        static SCHEMA: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
        SCHEMA.get_or_init(|| serde_json::json!({"type": "object", "properties": {}}))
    }

    fn resolve_execution(&self, _input: Value) -> EngineResult<ToolExecution> {
        Ok(ToolExecution::Runnable {
            plan: ExecutionPlan {
                accesses: Vec::new(),
                display: ToolInputDisplay::Generic {
                    summary: "Stop turn".to_string(),
                },
                approval_rule: "StopTurn".to_string(),
                stop_batch_after_this: false,
            },
            execute: Box::new(|_ctx| {
                Box::pin(async {
                    ToolResult {
                        output: ToolOutput::text("stopping turn"),
                        is_error: false,
                        note: None,
                        stop_turn: true,
                        truncated: false,
                        injection: None,
                    }
                })
            }),
        })
    }
}

// ---------------------------------------------------------------------------
// Mock Hooks
// ---------------------------------------------------------------------------

/// Hooks that can be configured to control behavior.
#[derive(Clone, Default)]
struct TestHooks {
    before_step_block: bool,
    before_step_block_reason: Option<String>,
    after_step_stop_turn: bool,
    should_continue: bool,
}

impl LoopHooks for TestHooks {
    fn before_step(&self, _ctx: BeforeStepContext) -> HookFuture<EngineResult<BeforeStepResult>> {
        let block = self.before_step_block;
        let reason = self.before_step_block_reason.clone();
        Box::pin(async move {
            Ok(BeforeStepResult {
                block,
                block_reason: reason,
            })
        })
    }

    fn after_step(&self, _ctx: AfterStepContext) -> HookFuture<EngineResult<AfterStepResult>> {
        let stop = self.after_step_stop_turn;
        Box::pin(async move { Ok(AfterStepResult { stop_turn: stop }) })
    }

    fn should_continue_after_stop(
        &self,
        _ctx: ContinueAfterStopContext,
    ) -> HookFuture<EngineResult<ContinueAfterStopResult>> {
        let cont = self.should_continue;
        Box::pin(async move {
            Ok(ContinueAfterStopResult {
                continue_turn: cont,
            })
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_message_builders() -> MessageBuilders {
    MessageBuilders {
        normal: Box::new(|| Ok(vec![Message::user("Hello")])),
        media_degraded: None,
        media_stripped: None,
        strict: None,
        format_rejected_flag: Arc::new(AtomicBool::new(false)),
    }
}

fn make_tool_manager_with_echo() -> ToolManager {
    let mut tm = ToolManager::new();
    tm.set_active_tools(vec!["Echo".to_string()]);
    tm.register_builtin(Arc::new(EchoTool));
    tm
}

fn make_tool_manager_with_stop_turn() -> ToolManager {
    let mut tm = ToolManager::new();
    tm.set_active_tools(vec!["StopTurn".to_string()]);
    tm.register_builtin(Arc::new(StopTurnTool));
    tm
}

fn make_tool_call(id: &str, name: &str, args: Value) -> LlmToolCall {
    LlmToolCall {
        id: id.to_string(),
        name: name.to_string(),
        args,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn should_end_turn_in_single_step_when_llm_returns_completed_response() {
    let llm = MockLlm::new(vec![MockResponse::end_turn("Hello!")]);
    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    let result = run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        None,
        None,
        None,
    )
    .await
    .expect("turn should succeed");

    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    assert_eq!(result.steps, 1);
    assert_eq!(result.usage.input_tokens, 100);
    assert_eq!(result.usage.output_tokens, 50);
}

#[tokio::test]
async fn should_continue_to_next_step_when_tool_call_is_made() {
    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![make_tool_call(
            "call-1",
            "Echo",
            serde_json::json!({"message": "hi"}),
        )]),
        MockResponse::end_turn("Done!"),
    ]);
    let hooks = TestHooks::default();
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    let result = run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        None,
        None,
        None,
    )
    .await
    .expect("turn should succeed");

    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    assert_eq!(result.steps, 2);
}

#[tokio::test]
async fn should_return_max_steps_exceeded_error_when_step_limit_reached() {
    // Tool use forever — never ends.
    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![make_tool_call(
            "call-1",
            "Echo",
            serde_json::json!({"message": "a"}),
        )]),
        MockResponse::tool_use(vec![make_tool_call(
            "call-2",
            "Echo",
            serde_json::json!({"message": "b"}),
        )]),
        MockResponse::tool_use(vec![make_tool_call(
            "call-3",
            "Echo",
            serde_json::json!({"message": "c"}),
        )]),
    ]);
    let hooks = TestHooks::default();
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    let mut config = RunTurnConfig::new("turn-1");
    config.max_steps = Some(2);

    let result = run_turn(
        config,
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        None,
        None,
        None,
    )
    .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(matches!(err, EngineError::MaxStepsExceeded(_)));
}

#[tokio::test]
async fn should_abort_when_cancel_token_cancelled_before_turn() {
    let llm = MockLlm::new(vec![MockResponse::end_turn("Hello")]);
    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    // Cancel before running.
    cancel.cancel();

    let result = run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        None,
        None,
        None,
    )
    .await
    .expect("turn should return (aborted)");

    assert_eq!(result.stop_reason, TurnStopReason::Aborted);
    assert_eq!(result.steps, 0);
}

#[tokio::test]
async fn should_hit_max_steps_error_when_should_continue_returns_true() {
    let llm = MockLlm::new(vec![
        MockResponse::end_turn("First"),
        MockResponse::end_turn("Second"),
    ]);
    let hooks = TestHooks {
        should_continue: true,
        ..Default::default()
    };
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    let mut config = RunTurnConfig::new("turn-1");
    config.max_steps = Some(2);

    let result = run_turn(
        config,
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        None,
        None,
        None,
    )
    .await;

    // After 2 steps with should_continue=true, max_steps triggers an error.
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(matches!(err, EngineError::MaxStepsExceeded(_)));
}

#[tokio::test]
async fn should_end_turn_when_should_continue_returns_false() {
    let llm = MockLlm::new(vec![MockResponse::end_turn("Done")]);
    let hooks = TestHooks {
        should_continue: false,
        ..Default::default()
    };
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    let result = run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        None,
        None,
        None,
    )
    .await
    .expect("turn should succeed");

    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    assert_eq!(result.steps, 1);
}

#[tokio::test]
async fn should_aggregate_usage_across_steps_when_multiple_steps_executed() {
    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![make_tool_call(
            "call-1",
            "Echo",
            serde_json::json!({"message": "hi"}),
        )])
        .with_usage(100, 50),
        MockResponse::end_turn("Done").with_usage(200, 75),
    ]);
    let hooks = TestHooks::default();
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    let result = run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        None,
        None,
        None,
    )
    .await
    .expect("turn should succeed");

    assert_eq!(result.usage.input_tokens, 300);
    assert_eq!(result.usage.output_tokens, 125);
    assert_eq!(result.steps, 2);
}

#[tokio::test]
async fn should_record_tool_call_results_in_history_when_tool_call_executed() {
    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![make_tool_call(
            "call-1",
            "Echo",
            serde_json::json!({"message": "hello world"}),
        )]),
        MockResponse::end_turn("Done"),
    ]);
    let hooks = TestHooks::default();
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    let result = run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        None,
        None,
        None,
    )
    .await
    .expect("turn should succeed");

    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);

    // The history should contain: assistant (with tool call), tool result, assistant (end turn).
    let history = {
        let guard = memory.lock().unwrap();
        guard.history().to_vec()
    };
    // At least 3 messages: assistant step 1, tool result, assistant step 2.
    assert!(
        history.len() >= 2,
        "expected at least 2 messages in history, got {}",
        history.len()
    );

    // Check the assistant message has the tool call.
    let assistant_msg = &history[0].message;
    assert_eq!(assistant_msg.tool_calls.len(), 1);
    assert_eq!(assistant_msg.tool_calls[0].id, "call-1");

    // Check there's a tool result message.
    let tool_result_msg = &history[1].message;
    assert_eq!(tool_result_msg.role, mirri_core::message::Role::Tool);
    assert_eq!(tool_result_msg.tool_call_id.as_deref(), Some("call-1"));
}

#[tokio::test]
async fn should_pause_turn_with_unexecuted_tool_calls_when_stop_reason_is_paused() {
    let llm = MockLlm::new(vec![MockResponse {
        text: String::new(),
        think_text: String::new(),
        tool_calls: vec![make_tool_call(
            "call-1",
            "Echo",
            serde_json::json!({"message": "hi"}),
        )],
        tool_parts: Vec::new(),
        stop_reason: "paused".to_string(),
        usage: LlmUsage {
            input_tokens: 10,
            output_tokens: 5,
            ..Default::default()
        },
        error: None,
        interrupted: false,
    }]);
    let hooks = TestHooks::default();
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    let result = run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        None,
        None,
        None,
    )
    .await
    .expect("turn should succeed");

    // Paused → should stop the turn with Paused stop reason.
    assert_eq!(result.stop_reason, TurnStopReason::Paused);
    assert_eq!(result.steps, 1);

    // The tool call should have been recorded but not executed (synthetic error).
    let history = {
        let guard = memory.lock().unwrap();
        guard.history().to_vec()
    };
    assert!(history.len() >= 2, "expected at least 2 messages");

    // Find the tool result message — it should be an error.
    let tool_msgs: Vec<_> = history
        .iter()
        .filter(|m| m.message.role == mirri_core::message::Role::Tool)
        .collect();
    assert!(!tool_msgs.is_empty(), "expected at least one tool message");
    assert!(tool_msgs[0].is_error, "tool result should be an error");
}

#[tokio::test]
async fn should_succeed_on_retry_when_first_attempt_errors() {
    // First call errors, second call succeeds. MockLlm returns from queue.
    let llm = MockLlm::new(vec![
        MockResponse::error("429 Too Many Requests"),
        MockResponse::end_turn("Success after retry"),
    ])
    .with_retryable(true);

    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    let mut config = RunTurnConfig::new("turn-1");
    config.max_retry_attempts = 3;

    let result = run_turn(
        config,
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        None,
        None,
        None,
    )
    .await
    .expect("turn should succeed after retry");

    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    assert_eq!(result.steps, 1);
}

#[tokio::test]
async fn should_return_provider_error_when_retry_attempts_exhausted() {
    let llm = MockLlm::new(vec![
        MockResponse::error("429 Too Many Requests"),
        MockResponse::error("429 Too Many Requests"),
        MockResponse::error("429 Too Many Requests"),
    ])
    .with_retryable(true);

    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    let mut config = RunTurnConfig::new("turn-1");
    config.max_retry_attempts = 2;

    let result = run_turn(
        config,
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        None,
        None,
        None,
    )
    .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    // T7 (F-P8-8): an exhausted transient rate limit surfaces with its
    // precise domain code (`provider.rate_limit`) instead of the generic
    // provider error — the retries themselves still ran (see the retry
    // budget assertions in the retry-layer tests).
    assert!(matches!(err, EngineError::ProviderRateLimit(_)));
    assert_eq!(err.code().as_str(), "provider.rate_limit");
}

#[tokio::test]
async fn should_fail_without_retry_when_error_is_non_retryable() {
    let llm =
        MockLlm::new(vec![MockResponse::error("400 Bad Request: invalid")]).with_retryable(false);

    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    let mut config = RunTurnConfig::new("turn-1");
    config.max_retry_attempts = 5;

    let result = run_turn(
        config,
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        None,
        None,
        None,
    )
    .await;

    assert!(result.is_err());
}

#[tokio::test]
async fn should_stop_turn_when_tool_returns_stop_turn_flag() {
    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![make_tool_call(
            "call-1",
            "StopTurn",
            serde_json::json!({}),
        )]),
        // This should never be reached because the tool stops the turn.
        MockResponse::end_turn("Should not reach"),
    ]);
    let hooks = TestHooks::default();
    let tools = make_tool_manager_with_stop_turn();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    let result = run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        None,
        None,
        None,
    )
    .await
    .expect("turn should succeed");

    // The tool set stop_turn, so effective stop reason becomes EndTurn.
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    assert_eq!(result.steps, 1);
}

#[tokio::test]
async fn should_return_hook_blocked_error_when_before_step_blocks() {
    let llm = MockLlm::new(vec![MockResponse::end_turn("Hello")]);
    let hooks = TestHooks {
        before_step_block: true,
        before_step_block_reason: Some("Blocked by test".to_string()),
        ..Default::default()
    };
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    let result = run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        None,
        None,
        None,
    )
    .await;

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(matches!(err, EngineError::HookBlocked(_)));
}

#[tokio::test]
async fn should_stop_turn_when_after_step_hook_returns_stop_turn() {
    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![make_tool_call(
            "call-1",
            "Echo",
            serde_json::json!({"message": "hi"}),
        )]),
        // Should not be reached because after_step forces stop.
        MockResponse::end_turn("Should not reach"),
    ]);
    let hooks = TestHooks {
        after_step_stop_turn: true,
        ..Default::default()
    };
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    let result = run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        None,
        None,
        None,
    )
    .await
    .expect("turn should succeed");

    // after_step returns stop_turn=true, so the ToolUse becomes EndTurn.
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    assert_eq!(result.steps, 1);
}

#[tokio::test]
async fn should_stop_turn_when_record_step_usage_returns_stop() {
    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![make_tool_call(
            "call-1",
            "Echo",
            serde_json::json!({"message": "hi"}),
        )]),
        // Should not be reached because record_step_usage stops the turn.
        MockResponse::end_turn("Should not reach"),
    ]);
    let hooks = TestHooks::default();
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    let record_fn: RecordStepUsageFn =
        Box::new(|_usage, _natural_stop, _step| Box::pin(async { Ok(Some(true)) }));

    let result = run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        None,
        Some(&record_fn),
        None,
    )
    .await
    .expect("turn should succeed");

    // record_step_usage returned Some(true), so ToolUse becomes EndTurn.
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    assert_eq!(result.steps, 1);
}

#[tokio::test]
async fn should_record_think_part_in_assistant_message_when_llm_returns_thinking() {
    let llm = MockLlm::new(vec![MockResponse {
        text: "Response".to_string(),
        think_text: "Thinking...".to_string(),
        tool_calls: Vec::new(),
        tool_parts: Vec::new(),
        stop_reason: "completed".to_string(),
        usage: LlmUsage {
            input_tokens: 10,
            output_tokens: 20,
            ..Default::default()
        },
        error: None,
        interrupted: false,
    }]);
    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    let result = run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        None,
        None,
        None,
    )
    .await
    .expect("turn should succeed");

    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);

    // Check that the assistant message has both text and think parts.
    let history = {
        let guard = memory.lock().unwrap();
        guard.history().to_vec()
    };
    assert!(!history.is_empty());
    let assistant = &history[0].message;
    assert_eq!(assistant.role, mirri_core::message::Role::Assistant);

    let has_text = assistant.content.iter().any(
        |p| matches!(p, mirri_core::message::ContentPart::Text { text } if text == "Response"),
    );
    let has_think = assistant.content.iter().any(
        |p| matches!(p, mirri_core::message::ContentPart::Think { text } if text == "Thinking..."),
    );
    assert!(has_text, "assistant message should have text part");
    assert!(has_think, "assistant message should have think part");
}

#[tokio::test]
async fn should_record_the_think_part_before_the_text_part_when_llm_returns_both() {
    // Given an LLM response carrying both reasoning and text (streamed
    // reasoning-first, the OpenAI/DeepSeek convention the live projector
    // renders in arrival order)
    let llm = MockLlm::new(vec![MockResponse {
        text: "Response".to_string(),
        think_text: "Thinking...".to_string(),
        tool_calls: Vec::new(),
        tool_parts: Vec::new(),
        stop_reason: "completed".to_string(),
        usage: LlmUsage {
            input_tokens: 10,
            output_tokens: 20,
            ..Default::default()
        },
        error: None,
        interrupted: false,
    }]);
    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        None,
        None,
        None,
    )
    .await
    .expect("turn should succeed");

    // When the assistant message is materialized, the think part must come
    // BEFORE the text part — the wire/replay path must preserve the streamed
    // arrival order the live projector already renders (reasoning first),
    // or a re-rendered session shows the thinking block after the answer.
    let history = {
        let guard = memory.lock().unwrap();
        guard.history().to_vec()
    };
    let assistant = &history[0].message;
    let first_is_think = matches!(
        assistant.content.first(),
        Some(mirri_core::message::ContentPart::Think { .. })
    );
    assert!(
        first_is_think,
        "assistant content should be [think, text], got {:?}",
        assistant
            .content
            .iter()
            .map(|p| match p {
                mirri_core::message::ContentPart::Text { .. } => "text",
                mirri_core::message::ContentPart::Think { .. } => "think",
                _ => "other",
            })
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn should_return_max_tokens_stop_reason_when_stop_reason_is_truncated() {
    let llm = MockLlm::new(vec![MockResponse {
        text: "Truncated response".to_string(),
        think_text: String::new(),
        tool_calls: Vec::new(),
        tool_parts: Vec::new(),
        stop_reason: "truncated".to_string(),
        usage: LlmUsage {
            input_tokens: 10,
            output_tokens: 4096,
            ..Default::default()
        },
        error: None,
        interrupted: false,
    }]);
    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    let result = run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        None,
        None,
        None,
    )
    .await
    .expect("turn should succeed");

    assert_eq!(result.stop_reason, TurnStopReason::MaxTokens);
}

#[tokio::test]
async fn should_return_filtered_stop_reason_when_stop_reason_is_filtered() {
    let llm = MockLlm::new(vec![MockResponse {
        text: "[filtered]".to_string(),
        think_text: String::new(),
        tool_calls: Vec::new(),
        tool_parts: Vec::new(),
        stop_reason: "filtered".to_string(),
        usage: LlmUsage {
            input_tokens: 10,
            output_tokens: 5,
            ..Default::default()
        },
        error: None,
        interrupted: false,
    }]);
    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    let result = run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        None,
        None,
        None,
    )
    .await
    .expect("turn should succeed");

    assert_eq!(result.stop_reason, TurnStopReason::Filtered);
}

#[tokio::test]
async fn should_forward_text_delta_events_to_callback_when_live_events_provided() {
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    let captured_deltas: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
    let deltas_clone = captured_deltas.clone();

    let callback: LiveEventCallback = Arc::new(move |event| {
        if let LiveEvent::TextDelta { text } = event {
            deltas_clone.lock().unwrap().push(text);
        }
    });

    let llm = MockLlm::new(vec![MockResponse::end_turn("Hello world!")]);
    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    let result = run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        Some(&callback),
        None,
        None,
    )
    .await
    .expect("turn should succeed");

    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);

    let deltas = captured_deltas.lock().unwrap();
    assert!(
        !deltas.is_empty(),
        "expected at least one TextDelta live event"
    );
    assert_eq!(deltas[0], "Hello world!");
}

#[tokio::test]
async fn should_forward_tool_call_part_identity_to_callback_when_parts_carry_it() {
    use std::sync::Mutex as StdMutex;

    // Given an LLM response whose tool-call arguments arrive as two streamed
    // fragments, each carrying the call id and tool name
    type ForwardedPart = (Option<String>, Option<String>, String);
    let captured: Arc<StdMutex<Vec<ForwardedPart>>> = Arc::new(StdMutex::new(Vec::new()));
    let captured_clone = captured.clone();

    let callback: LiveEventCallback = Arc::new(move |event| {
        if let LiveEvent::ToolCallPart {
            call_id,
            tool_name,
            arguments_part,
            ..
        } = event
        {
            captured_clone
                .lock()
                .unwrap()
                .push((call_id, tool_name, arguments_part));
        }
    });

    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![LlmToolCall {
            id: "call_1".to_string(),
            name: "search".to_string(),
            args: serde_json::json!({"q": "hello"}),
        }])
        .with_tool_parts(vec![
            (
                0,
                "{\"q\":".to_string(),
                Some("call_1".to_string()),
                Some("search".to_string()),
            ),
            (
                0,
                "hello}".to_string(),
                Some("call_1".to_string()),
                Some("search".to_string()),
            ),
        ]),
        MockResponse::end_turn("done"),
    ]);

    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        Some(&callback),
        None,
        None,
    )
    .await
    .expect("turn should succeed");

    // Then both forwarded fragments keep their identity field-for-field —
    // the consumer can route interleaved parallel calls live
    let parts = captured.lock().unwrap();
    assert_eq!(parts.len(), 2, "expected two ToolCallPart live events");
    for (call_id, tool_name, _arguments_part) in parts.iter() {
        assert_eq!(call_id.as_deref(), Some("call_1"));
        assert_eq!(tool_name.as_deref(), Some("search"));
    }
    assert_eq!(parts[0].2, "{\"q\":");
    assert_eq!(parts[1].2, "hello}");
}

#[tokio::test]
async fn should_forward_tool_progress_updates_to_callback_when_tool_emits_updates() {
    use std::sync::Mutex as StdMutex;

    // Given a tool that emits two updates through its progress handle
    let captured: Arc<StdMutex<Vec<String>>> = Arc::new(StdMutex::new(Vec::new()));
    let captured_clone = captured.clone();

    let callback: LiveEventCallback = Arc::new(move |event| match event {
        LiveEvent::ToolCallStarted { tool_call_id, .. } => {
            captured_clone
                .lock()
                .unwrap()
                .push(format!("started:{tool_call_id}"));
        }
        LiveEvent::ToolProgress {
            tool_call_id,
            update,
        } => {
            captured_clone
                .lock()
                .unwrap()
                .push(format!("progress:{tool_call_id}:{:?}", update.kind));
        }
        LiveEvent::ToolCallCompleted {
            tool_call_id,
            is_error,
            ..
        } => {
            captured_clone
                .lock()
                .unwrap()
                .push(format!("completed:{tool_call_id}:{is_error}"));
        }
        _ => {}
    });

    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![make_tool_call(
            "call-1",
            "Progressing",
            serde_json::json!({}),
        )]),
        MockResponse::end_turn("Done!"),
    ]);
    let hooks = TestHooks::default();
    let mut tools = ToolManager::new();
    tools.set_active_tools(vec!["Progressing".to_string()]);
    tools.register_builtin(Arc::new(ProgressingTool));
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    // When a turn runs the tool with the live callback installed
    let result = run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory,
        &cancel,
        &builders,
        Some(&callback),
        None,
        None,
    )
    .await
    .expect("turn should succeed");

    // Then the events arrive in order: started → both updates → completed
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    let events = captured.lock().unwrap();
    assert_eq!(
        *events,
        vec![
            "started:call-1".to_string(),
            "progress:call-1:Progress".to_string(),
            "progress:call-1:Status".to_string(),
            "completed:call-1:false".to_string(),
        ],
        "expected started → progress ×2 → completed in order"
    );
}

#[tokio::test]
async fn should_emit_step_retrying_event_when_request_level_retry_occurs() {
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    let retry_events: Arc<StdMutex<Vec<(u32, u32, u64)>>> = Arc::new(StdMutex::new(Vec::new()));
    let retry_clone = retry_events.clone();

    let callback: LiveEventCallback = Arc::new(move |event| {
        if let LiveEvent::StepRetrying {
            failed_attempt,
            next_attempt,
            delay_ms,
            ..
        } = event
        {
            retry_clone
                .lock()
                .unwrap()
                .push((failed_attempt, next_attempt, delay_ms));
        }
    });

    let llm = MockLlm::new(vec![
        MockResponse::error("429 Too Many Requests"),
        MockResponse::end_turn("Success"),
    ])
    .with_retryable(true);

    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    let mut config = RunTurnConfig::new("turn-1");
    config.max_retry_attempts = 3;

    let result = run_turn(
        config,
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        Some(&callback),
        None,
        None,
    )
    .await
    .expect("turn should succeed");

    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);

    let events = retry_events.lock().unwrap();
    assert_eq!(events.len(), 1, "expected one StepRetrying event");
    assert_eq!(events[0].0, 1, "failed_attempt should be 1");
    assert_eq!(events[0].1, 2, "next_attempt should be 2");
}

#[tokio::test]
async fn should_retry_failed_step_when_step_level_retry_is_granted() {
    // The mock is not request-level retryable, so `chat_with_retry` surfaces
    // the provider error immediately (max_retry_attempts = 1) and the
    // step-level service gets the chance to re-enqueue the failed step.
    let llm = MockLlm::new(vec![
        MockResponse::error("429 Too Many Requests"),
        MockResponse::end_turn("Recovered"),
    ]);

    let retry_events: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
    let retry_clone = retry_events.clone();
    let callback: LiveEventCallback = Arc::new(move |event| {
        if let LiveEvent::StepRetrying { next_attempt, .. } = event {
            retry_clone.lock().unwrap().push(next_attempt);
        }
    });

    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    let mut config = RunTurnConfig::new("turn-1");
    config.max_retry_attempts = 1; // surface the provider error immediately
    config.step_retry = Some(Arc::new(StepRetryService::with_defaults()));

    let result = run_turn(
        config,
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        Some(&callback),
        None,
        None,
    )
    .await
    .expect("turn should succeed after step-level retry");

    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    // The retried step keeps its step number: exactly one step executed.
    assert_eq!(result.steps, 1);
    let retries = retry_events.lock().unwrap();
    assert_eq!(
        retries.as_slice(),
        &[2],
        "expected one step-level retry with next attempt 2"
    );
}

#[tokio::test]
async fn should_give_up_step_retry_when_step_retry_budget_is_exhausted() {
    // Three identical failures: the first two consume the per-step budget
    // (default max_retries_per_step = 2), the third is a GiveUp and the
    // error surfaces to the caller.
    let llm = MockLlm::new(vec![
        MockResponse::error("connection reset by peer"),
        MockResponse::error("connection reset by peer"),
        MockResponse::error("connection reset by peer"),
    ]);

    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    let mut config = RunTurnConfig::new("turn-1");
    config.max_retry_attempts = 1; // surface the provider error immediately
    config.step_retry = Some(Arc::new(StepRetryService::with_defaults()));

    let result = run_turn(
        config,
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        None,
        None,
        None,
    )
    .await;

    let error = result.expect_err("step retry budget must be exhausted");
    assert!(error.to_string().contains("connection reset by peer"));
}

#[tokio::test]
async fn should_retry_step_when_llm_returns_empty_end_turn_response() {
    // Given an LLM whose first response is completely empty (usage billed,
    // clean natural stop, but no text, no thinking, no tool calls) and whose
    // second response recovers
    let llm = MockLlm::new(vec![
        MockResponse::empty_end_turn(),
        MockResponse::end_turn("Recovered after empty response"),
    ]);

    let retry_events: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
    let retry_clone = retry_events.clone();
    let callback: LiveEventCallback = Arc::new(move |event| {
        if let LiveEvent::StepRetrying { next_attempt, .. } = event {
            retry_clone.lock().unwrap().push(next_attempt);
        }
    });

    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    let mut config = RunTurnConfig::new("turn-1");
    config.max_retry_attempts = 1;
    config.step_retry = Some(Arc::new(StepRetryService::with_defaults()));

    // When the turn runs
    let result = run_turn(
        config,
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        Some(&callback),
        None,
        None,
    )
    .await
    .expect("the turn should succeed after the empty response was retried");

    // Then the empty step was retried once (same step number) and the
    // recovered answer is the recorded outcome — never a silent completion.
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    assert_eq!(result.steps, 1);
    let retries = retry_events.lock().unwrap();
    assert_eq!(
        retries.as_slice(),
        &[2],
        "expected one step-level retry with next attempt 2"
    );
    let recovered = memory
        .lock()
        .unwrap()
        .messages()
        .iter()
        .any(|m| m.text_content().contains("Recovered after empty response"));
    assert!(recovered, "the retried step's answer must be recorded");
}

#[tokio::test]
async fn should_fail_with_visible_wire_marker_when_empty_response_exhausts_step_retry() {
    // Given an LLM that only ever returns empty responses (no text, no
    // thinking, no tool calls) — the per-step retry budget (default 2) is
    // exhausted by the third attempt
    let llm = MockLlm::new(vec![
        MockResponse::empty_end_turn(),
        MockResponse::empty_end_turn(),
        MockResponse::empty_end_turn(),
    ]);

    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();
    let recorder = CollectingWireRecorder(Arc::new(Mutex::new(Vec::new())));

    let mut config = RunTurnConfig::new("turn-1");
    config.max_retry_attempts = 1;
    config.step_retry = Some(Arc::new(StepRetryService::with_defaults()));

    // When the turn runs
    let result = run_turn(
        config,
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        None,
        None,
        Some(&recorder as &dyn WireOpRecorder),
    )
    .await;

    // Then the turn fails explicitly with the dedicated error — it is never
    // silently completed.
    let error = result.expect_err("an all-empty turn must fail, not complete");
    assert!(
        matches!(error, EngineError::EmptyLlmResponse),
        "expected EmptyLlmResponse, got {error:?}"
    );

    // And the wire carries a visible marker telling the user the model
    // returned nothing (a system-reminder append, same mechanism as the
    // user-cancel closure).
    let ops = recorder.0.lock().unwrap().clone();
    let marker_on_wire = ops.iter().any(|op| match op {
        mirri_proto::WireOp::ContextAppendMessage { message } => message
            .get("content")
            .and_then(|c| c.as_array())
            .map(|parts| {
                parts.iter().any(|p| {
                    p.get("text")
                        .and_then(|t| t.as_str())
                        .is_some_and(|t| t.contains("empty response"))
                })
            })
            .unwrap_or(false),
        _ => false,
    });
    assert!(
        marker_on_wire,
        "the wire log must carry the empty-response marker"
    );

    // And the live memory carries the same reminder, so a resumed session
    // explains why the turn died.
    let marker_in_memory = memory
        .lock()
        .unwrap()
        .messages()
        .iter()
        .any(|m| m.text_content().contains("empty response"));
    assert!(
        marker_in_memory,
        "the memory must carry the empty-response reminder"
    );
}

#[tokio::test]
async fn should_fallback_to_media_degraded_when_payload_too_large() {
    // First step: 413 error → media_degraded fallback → success.
    // Second step: should use media_degraded projection.
    // We can't easily test the sticky flag directly, but we can verify the
    // turn completes successfully with the fallback.
    let llm = MockLlm::new(vec![
        MockResponse::error("413 Payload Too Large"),
        MockResponse::end_turn("Success with degraded"),
    ]);

    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = MessageBuilders {
        normal: Box::new(|| Ok(vec![Message::user("Hello")])),
        media_degraded: Some(Box::new(|| Ok(vec![Message::user("Hello degraded")]))),
        media_stripped: None,
        strict: None,
        format_rejected_flag: Arc::new(AtomicBool::new(false)),
    };

    // The mock LLM is not retryable, so the 413 error will not retry via
    // chat_with_retry. But the media fallback in turn_step should kick in.
    let result = run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        None,
        None,
        None,
    )
    .await;

    // The media fallback should succeed.
    assert!(result.is_ok());
    let result = result.unwrap();
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
}

#[tokio::test]
async fn should_strip_image_into_placeholder_for_text_only_model() {
    // Given a conversation whose user message carries an image part (with an
    // archive path) and a text-only model (image_in=false — the MockLlm's
    // fixed capability)
    let llm = Arc::new(MockLlm::new(vec![MockResponse::end_turn("Got it")]));
    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = memory_with_image_message();
    let cancel = CancelToken::new();
    let builders = memory_backed_builders(memory.clone());

    // When the turn executes and the LLM receives the request
    run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(llm.clone() as Arc<dyn Llm>),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        None,
        None,
        None,
    )
    .await
    .expect("turn should succeed");

    // Then the request carries the omission placeholder instead of the
    // image part (ROADMAP §3 P0 main acceptance, normal request shape)
    let requests = llm.requests.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(requests.len(), 1, "exactly one request was served");
    assert_request_has_placeholder_not_image(&requests[0]);

    // And context memory keeps the original image (ADR-002: the projection
    // is pure — only the request copy is adapted)
    let history = {
        let mem = memory.lock().unwrap_or_else(|e| e.into_inner());
        mem.history().to_vec()
    };
    assert!(
        history.iter().any(|m| m
            .message
            .content
            .iter()
            .any(|p| matches!(p, ContentPart::ImageUrl { .. }))),
        "context memory still holds the original image part"
    );
}

#[tokio::test]
async fn should_strip_image_into_placeholder_for_strict_retry_after_structure_error() {
    // Given a KNOWN text-only model whose first response is rejected as a
    // 400 structure error, with a strict fallback builder that rebuilds the
    // request straight from the image-bearing context memory (the exact
    // shape that would otherwise resurrect the ROADMAP §3 error path in
    // fallback form)
    let llm = Arc::new(MockLlm::new(vec![
        MockResponse::error("400 Bad Request: invalid message structure"),
        MockResponse::end_turn("recovered via strict"),
    ]));
    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = memory_with_image_message();
    let cancel = CancelToken::new();
    let builders = fully_memory_backed_builders(memory.clone());

    // When the turn runs and the structure-error fallback chain retries
    // with the strict projection
    run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(llm.clone() as Arc<dyn Llm>),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        None,
        None,
        None,
    )
    .await
    .expect("turn should succeed via the strict fallback");

    // Then BOTH requests (the initial one and the strict retry) carry the
    // omission placeholder instead of the image part — no request shape may
    // hand an image to a known text-only model
    let requests = llm.requests.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(
        requests.len(),
        2,
        "the initial request plus one strict retry"
    );
    assert_request_has_placeholder_not_image(&requests[0]);
    assert_request_has_placeholder_not_image(&requests[1]);
}

#[tokio::test]
async fn should_strip_image_into_placeholder_for_media_degraded_retry_after_413() {
    // Given a KNOWN text-only model whose first response is rejected as too
    // large, with a media-degraded fallback builder that also rebuilds from
    // the image-bearing context memory
    let llm = Arc::new(MockLlm::new(vec![
        MockResponse::error("413 Payload Too Large"),
        MockResponse::end_turn("recovered via degraded"),
    ]));
    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = memory_with_image_message();
    let cancel = CancelToken::new();
    let builders = fully_memory_backed_builders(memory.clone());

    // When the turn runs and the 413 fallback chain retries with the
    // media-degraded projection
    run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(llm.clone() as Arc<dyn Llm>),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        None,
        None,
        None,
    )
    .await
    .expect("turn should succeed via the degraded fallback");

    // Then the degraded retry request carries the omission placeholder
    // instead of the image part (and so does the initial request)
    let requests = llm.requests.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(
        requests.len(),
        2,
        "the initial request plus one degraded retry"
    );
    assert_request_has_placeholder_not_image(&requests[0]);
    assert_request_has_placeholder_not_image(&requests[1]);
}

/// Assert that a request carries no image part and that its user-message
/// text contains the given omission-cause marker (US6.1 size-limit vs US6.2
/// format-rejected copy).
fn assert_request_media_placeholder(messages: &[Message], marker: &str) {
    for message in messages {
        for part in &message.content {
            assert!(
                !matches!(part, ContentPart::ImageUrl { .. }),
                "no image part may survive the stripped projection: {part:?}"
            );
        }
    }
    let user = messages
        .iter()
        .find(|m| m.role == mirri_core::message::Role::User)
        .expect("the user message is in the request");
    let placeholder = user
        .content
        .iter()
        .find_map(|part| match part {
            ContentPart::Text { text } if text.contains("omitted") => Some(text.as_str()),
            _ => None,
        })
        .expect("an omission placeholder is in the request");
    assert!(
        placeholder.contains(marker),
        "placeholder should carry the {marker:?} cause: {placeholder}"
    );
}

#[tokio::test]
async fn should_use_format_rejected_placeholders_for_stripped_retry_after_format_error() {
    // Given a KNOWN image-capable model whose first response rejects the
    // image format, with the production builders over image-bearing memory
    let llm = Arc::new(
        MockLlm::new(vec![
            MockResponse::error("400 Unsupported image format"),
            // The stripped retry succeeds with a tool call, so the turn
            // runs a second step — which replays the sticky stripped
            // projection.
            MockResponse::tool_use(vec![LlmToolCall {
                id: "call_1".to_string(),
                name: "search".to_string(),
                args: serde_json::json!({"q": "hello"}),
            }]),
            MockResponse::end_turn("recovered"),
        ])
        .with_capability(ModelCapability {
            image_in: true,
            video_in: true,
            audio_in: true,
            dynamically_loaded_tools: false,
        }),
    );
    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = memory_with_image_message();
    let cancel = CancelToken::new();
    let builders = MessageBuilders::from_shared_memory(memory.clone());

    // When the turn runs through the format-error fallback chain
    run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(llm.clone() as Arc<dyn Llm>),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        None,
        None,
        None,
    )
    .await
    .expect("turn should succeed via the stripped fallback");

    // Then the initial request carried the image (the model is
    // image-capable), and BOTH the stripped retry and the subsequent sticky
    // stripped replay (step 2) carry the format-rejected placeholder with
    // the conversion guidance — not the size-limit copy (US6.2).
    let requests = llm.requests.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(
        requests.len(),
        3,
        "the initial request, the stripped retry, and the sticky step-2 replay"
    );
    assert!(
        requests[0].iter().any(|m| m
            .content
            .iter()
            .any(|p| matches!(p, ContentPart::ImageUrl { .. }))),
        "the image-capable model receives the image on the initial request"
    );
    for request in [&requests[1], &requests[2]] {
        assert_request_media_placeholder(request, "provider rejected its format");
        let user = request
            .iter()
            .find(|m| m.role == mirri_core::message::Role::User)
            .expect("the user message is in the request");
        let placeholder = user
            .content
            .iter()
            .find_map(|part| match part {
                ContentPart::Text { text } if text.contains("omitted") => Some(text.as_str()),
                _ => None,
            })
            .expect("an omission placeholder is in the request");
        assert!(placeholder.contains("Convert"));
        assert!(!placeholder.contains("size limit"));
    }
}

#[tokio::test]
async fn should_keep_size_limit_placeholders_for_stripped_retry_after_413() {
    // Given a KNOWN image-capable model that keeps rejecting the request as
    // too large (413), with the production builders over image-bearing
    // memory — the chain must walk degraded → stripped
    let llm = Arc::new(
        MockLlm::new(vec![
            MockResponse::error("413 Payload Too Large"),
            MockResponse::error("413 Payload Too Large"),
            MockResponse::end_turn("recovered via stripped"),
        ])
        .with_capability(ModelCapability {
            image_in: true,
            video_in: true,
            audio_in: true,
            dynamically_loaded_tools: false,
        }),
    );
    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = memory_with_image_message();
    let cancel = CancelToken::new();
    let builders = MessageBuilders::from_shared_memory(memory.clone());

    // When the turn runs through the 413 fallback chain
    run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(llm.clone() as Arc<dyn Llm>),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        None,
        None,
        None,
    )
    .await
    .expect("turn should succeed via the stripped fallback");

    // Then the stripped retry carries the SIZE-LIMIT placeholder copy — the
    // 413 path never flips the format-rejected cause (US6.1 regression
    // guard).
    let requests = llm.requests.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(
        requests.len(),
        3,
        "the initial request, the degraded retry, and the stripped retry"
    );
    // The degraded retry keeps the most recent media part (keep_recent=2).
    assert!(
        requests[1].iter().any(|m| m
            .content
            .iter()
            .any(|p| matches!(p, ContentPart::ImageUrl { .. }))),
        "the degraded projection keeps the recent image"
    );
    assert_request_media_placeholder(&requests[2], "size limit");
    let user = requests[2]
        .iter()
        .find(|m| m.role == mirri_core::message::Role::User)
        .expect("the user message is in the request");
    let placeholder = user
        .content
        .iter()
        .find_map(|part| match part {
            ContentPart::Text { text } if text.contains("omitted") => Some(text.as_str()),
            _ => None,
        })
        .expect("an omission placeholder is in the request");
    assert!(placeholder.contains("Re-read the file"));
    assert!(!placeholder.contains("rejected its format"));
}

#[tokio::test]
async fn should_hide_read_media_file_from_models_without_media_input() {
    // Given a model without image or video input (image_in=false,
    // video_in=false — the MockLlm default; undefined is unsupported,
    // ADR-003 revision) and a tool manager whose active set includes
    // ReadMediaFile
    let llm = Arc::new(MockLlm::new(vec![MockResponse::end_turn("Got it")]));
    let hooks = TestHooks::default();
    let mut tools = ToolManager::new();
    tools.set_active_tools(vec!["ReadMediaFile".to_string()]);
    tools.register_builtin(Arc::new(ReadMediaFileTool::new(
        None,
        Arc::new(mirri_os::LocalFs::new()),
    )));
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    // When the turn executes and the LLM receives the request
    run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(llm.clone() as Arc<dyn Llm>),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        None,
        None,
        None,
    )
    .await
    .expect("turn should succeed");

    // Then the request's tool set contains no ReadMediaFile entry (US4.3: a
    // model known to lack image and video input is not offered the
    // image-reading tool — it would only set up a doomed call)
    let tool_sets = llm.tool_sets.lock().unwrap_or_else(|e| e.into_inner());
    assert_eq!(tool_sets.len(), 1, "exactly one request was served");
    assert!(
        tool_sets[0].iter().all(|tool| tool.name != "ReadMediaFile"),
        "ReadMediaFile must be hidden from a model known to lack image/video input: {:?}",
        tool_sets[0]
    );
}

#[tokio::test]
async fn should_return_unknown_stop_reason_when_stop_reason_is_unrecognized() {
    let llm = MockLlm::new(vec![MockResponse {
        text: "Something happened".to_string(),
        think_text: String::new(),
        tool_calls: Vec::new(),
        tool_parts: Vec::new(),
        stop_reason: "other".to_string(),
        usage: LlmUsage::default(),
        error: None,
        interrupted: false,
    }]);
    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    let result = run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        None,
        None,
        None,
    )
    .await
    .expect("turn should succeed");

    assert_eq!(result.stop_reason, TurnStopReason::Unknown);
}

#[tokio::test]
async fn should_execute_all_tool_calls_when_multiple_tool_calls_in_one_step() {
    let llm = MockLlm::new(vec![
        MockResponse::tool_use(vec![
            make_tool_call("call-1", "Echo", serde_json::json!({"message": "first"})),
            make_tool_call("call-2", "Echo", serde_json::json!({"message": "second"})),
        ]),
        MockResponse::end_turn("Done with both"),
    ]);
    let hooks = TestHooks::default();
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    let result = run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        None,
        None,
        None,
    )
    .await
    .expect("turn should succeed");

    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    assert_eq!(result.steps, 2);

    // Check both tool results are in the history.
    let history_vec = {
        let guard = memory.lock().unwrap();
        guard.history().to_vec()
    };
    let tool_msgs: Vec<_> = history_vec
        .iter()
        .filter(|m| m.message.role == mirri_core::message::Role::Tool)
        .collect();
    assert_eq!(tool_msgs.len(), 2, "expected 2 tool result messages");
}

#[tokio::test]
async fn should_fail_turn_when_text_is_empty_and_no_retry_service_is_wired() {
    // Given a single completely empty response (no text, no thinking, no
    // tool calls) and no step-level retry service configured
    let llm = MockLlm::new(vec![MockResponse::empty_end_turn()]);
    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    // When the turn runs
    let result = run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        None,
        None,
        None,
    )
    .await;

    // Then the turn fails explicitly with the dedicated error — an empty
    // response is never a legitimate turn completion (turn-scheduling
    // US1.4 / SDR-002).
    let error = result.expect_err("an empty response must fail the turn");
    assert!(matches!(error, EngineError::EmptyLlmResponse));
}

#[tokio::test]
async fn should_retry_step_when_llm_returns_empty_response_with_unknown_stop_reason() {
    // Given an LLM whose first response is empty under an unrecognized stop
    // reason ("other" → Unknown) and whose second response recovers
    let llm = MockLlm::new(vec![
        MockResponse::empty_with_stop_reason("other"),
        MockResponse::end_turn("Recovered after unknown empty response"),
    ]);

    let retry_events: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
    let retry_clone = retry_events.clone();
    let callback: LiveEventCallback = Arc::new(move |event| {
        if let LiveEvent::StepRetrying { next_attempt, .. } = event {
            retry_clone.lock().unwrap().push(next_attempt);
        }
    });

    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    let mut config = RunTurnConfig::new("turn-1");
    config.max_retry_attempts = 1;
    config.step_retry = Some(Arc::new(StepRetryService::with_defaults()));

    // When the turn runs
    let result = run_turn(
        config,
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        Some(&callback),
        None,
        None,
    )
    .await
    .expect("the turn should succeed after the empty response was retried");

    // Then the empty step was retried — an Unknown stop also settles as
    // "completed" on the wire, so the guard must catch it (US1.4).
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    assert_eq!(result.steps, 1);
    let retries = retry_events.lock().unwrap();
    assert_eq!(
        retries.as_slice(),
        &[2],
        "expected one step-level retry with next attempt 2"
    );
}

#[tokio::test]
async fn should_fail_turn_when_empty_response_stops_truncated_without_retry_service() {
    // Given a single empty response truncated at the output limit (no
    // retry service wired)
    let llm = MockLlm::new(vec![MockResponse::empty_with_stop_reason("truncated")]);
    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    // When the turn runs
    let result = run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        None,
        None,
        None,
    )
    .await;

    // Then the turn fails explicitly — a MaxTokens stop also renders as
    // "completed" on the wire, so an empty truncated response is never a
    // legitimate completion (US1.4).
    let error = result.expect_err("an empty truncated response must fail the turn");
    assert!(matches!(error, EngineError::EmptyLlmResponse));
}

#[tokio::test]
async fn should_settle_aborted_without_step_retry_when_empty_stream_is_interrupted() {
    // Given an empty stream cancelled mid-flight (the cancel token fires at
    // chat time, so the stream ends with no Usage and no Done) and a
    // step-level retry service wired
    let cancel = CancelToken::new();
    let llm =
        MockLlm::new(vec![MockResponse::interrupted_empty()]).with_cancel_on_chat(cancel.clone());

    let retry_events: Arc<Mutex<Vec<u32>>> = Arc::new(Mutex::new(Vec::new()));
    let retry_clone = retry_events.clone();
    let callback: LiveEventCallback = Arc::new(move |event| {
        if let LiveEvent::StepRetrying { next_attempt, .. } = event {
            retry_clone.lock().unwrap().push(next_attempt);
        }
    });

    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let builders = make_message_builders();

    let mut config = RunTurnConfig::new("turn-1");
    config.max_retry_attempts = 1;
    config.step_retry = Some(Arc::new(StepRetryService::with_defaults()));

    // When the turn runs
    let result = run_turn(
        config,
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        Some(&callback),
        None,
        None,
    )
    .await
    .expect("the cancelled turn should settle through the abort path");

    // Then the turn settles as aborted (the cancellation closure owns the
    // interrupted shape) and the empty-response guard never fires — no
    // step-level retry was attempted (US1.4 acceptance: cancellation does
    // not trigger the empty-response retry).
    assert_eq!(result.stop_reason, TurnStopReason::Aborted);
    assert!(retry_events.lock().unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// Interrupted-turn closure (方式 B 取消闭合) — P8a
// ---------------------------------------------------------------------------

/// A [`WireOpRecorder`] test double that collects every op into a shared
/// vector, in dispatch order.
struct CollectingWireRecorder(Arc<Mutex<Vec<mirri_proto::WireOp>>>);

impl WireOpRecorder for CollectingWireRecorder {
    fn record_op<'a>(
        &'a self,
        op: mirri_proto::WireOp,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
        let collected = Arc::clone(&self.0);
        Box::pin(async move {
            collected.lock().unwrap_or_else(|e| e.into_inner()).push(op);
        })
    }
}

/// Message builders that project the given shared memory (normal projection
/// only), so request snapshots reflect the canonical history.
fn memory_backed_builders(memory: Arc<Mutex<ContextMemory>>) -> MessageBuilders {
    let mem_for_normal = memory;
    MessageBuilders {
        normal: Box::new(move || {
            let mem = mem_for_normal.lock().unwrap_or_else(|e| e.into_inner());
            Ok(mem.messages())
        }),
        media_degraded: None,
        media_stripped: None,
        strict: None,
        format_rejected_flag: Arc::new(AtomicBool::new(false)),
    }
}

/// Message builders that project the given shared memory through EVERY
/// projection slot (normal / degraded / stripped / strict), so fallback
/// retries rebuild straight from the canonical history — the shape that
/// must still pass the media adaptation pipeline (plan constraint: every
/// request assembly, including retries and fallbacks).
fn fully_memory_backed_builders(memory: Arc<Mutex<ContextMemory>>) -> MessageBuilders {
    let make = |memory: Arc<Mutex<ContextMemory>>| {
        Box::new(move || {
            let mem = memory.lock().unwrap_or_else(|e| e.into_inner());
            Ok(mem.messages())
        }) as Box<dyn Fn() -> EngineResult<Vec<Message>> + Send + Sync>
    };
    MessageBuilders {
        normal: make(memory.clone()),
        media_degraded: Some(make(memory.clone())),
        media_stripped: Some(make(memory.clone())),
        strict: Some(make(memory)),
        format_rejected_flag: Arc::new(AtomicBool::new(false)),
    }
}

/// Assert that a captured request to the KNOWN text-only mock LLM carries no
/// `ImageUrl` part, and that the user-message position holds the omission
/// placeholder with the archive path alongside the preserved user text
/// (ROADMAP §3 P0 acceptance, applied to a fallback/retry request shape).
fn assert_request_has_placeholder_not_image(messages: &[Message]) {
    for message in messages {
        for part in &message.content {
            assert!(
                !matches!(part, ContentPart::ImageUrl { .. }),
                "no image part may reach a text-only model, even on a fallback \
                 retry: {part:?}"
            );
        }
    }
    let user = messages
        .iter()
        .find(|m| m.role == mirri_core::message::Role::User)
        .expect("the user message is in the request");
    let placeholder = user
        .content
        .iter()
        .find_map(|part| match part {
            ContentPart::Text { text } if text.contains("does not support image input") => {
                Some(text.as_str())
            }
            _ => None,
        })
        .expect("the image is replaced with an omission placeholder");
    assert!(
        placeholder.contains("/tmp/sessions/s1/blobs/ab12"),
        "the placeholder carries the archive path: {placeholder}"
    );
    assert!(
        user.content.iter().any(
            |part| matches!(part, ContentPart::Text { text } if text == "what is in this picture?")
        ),
        "the user's text is preserved"
    );
}

/// Seed a memory with one user message carrying an archived image part.
fn memory_with_image_message() -> Arc<Mutex<ContextMemory>> {
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    memory
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .append_user_message(
            vec![
                ContentPart::Text {
                    text: "what is in this picture?".to_string(),
                },
                ContentPart::ImageUrl {
                    image_url: ImageUrlData {
                        url: "data:image/png;base64,AAAA".to_string(),
                        detail: None,
                        id: Some("/tmp/sessions/s1/blobs/ab12".to_string()),
                    },
                    detail: None,
                },
            ],
            PromptOrigin::User,
        );
    memory
}

/// Extract the loop-event sub-type of a collected wire op, if it is one.
fn loop_event_type(op: &mirri_proto::WireOp) -> Option<String> {
    match op {
        mirri_proto::WireOp::ContextAppendLoopEvent { event } => event
            .get("type")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        _ => None,
    }
}

#[tokio::test]
async fn should_abort_with_interrupted_anchor_when_stream_cancelled_mid_flight() {
    // Given an LLM whose first stream is cancelled mid-flight after
    // delivering partial text
    let cancel = CancelToken::new();
    let llm = Arc::new(
        MockLlm::new(vec![
            MockResponse::interrupted_after("Partial answer: the first half"),
            MockResponse::end_turn("Second turn answer"),
        ])
        .with_cancel_on_chat(cancel.clone()),
    );
    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let builders = memory_backed_builders(memory.clone());

    // When the turn runs, it aborts (cancelled mid-stream)
    let result = run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(Arc::clone(&llm) as Arc<dyn Llm>),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        None,
        None,
        None,
    )
    .await
    .expect("turn should return, not error");

    assert_eq!(result.stop_reason, TurnStopReason::Aborted);
    assert_eq!(result.steps, 1);

    // Then the memory holds the delivered prefix as a SETTLED assistant
    // message with the interrupted anchor — not a dropped partial.
    {
        let mem = memory.lock().unwrap_or_else(|e| e.into_inner());
        let assistant = mem
            .history()
            .iter()
            .rev()
            .find(|m| m.message.role == mirri_core::message::Role::Assistant)
            .expect("an assistant message must survive the abort")
            .clone();
        assert!(
            !assistant.message.partial,
            "the interrupted step must be settled, not left partial"
        );
        let text = assistant.message.text_content();
        assert!(
            text.contains("Partial answer: the first half"),
            "the delivered prefix must be kept, got: {text:?}"
        );
        assert!(
            text.contains("[Request interrupted]"),
            "the interrupted anchor must be appended, got: {text:?}"
        );
        // No deferred stranding and no open tool exchange left behind.
        assert!(!mem.has_open_tool_exchange());
    }

    // And the NEXT request (fresh turn, same memory) contains the delivered
    // prefix — the model sees what the user saw.
    let cancel2 = CancelToken::new();
    run_turn(
        RunTurnConfig::new("turn-2"),
        &FixedLlmResolver::new(Arc::clone(&llm) as Arc<dyn Llm>),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel2,
        &builders,
        None,
        None,
        None,
    )
    .await
    .expect("second turn should succeed");

    let requests = llm.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2, "two LLM requests must have been made");
    let second_request = &requests[1];
    let assistant_texts: Vec<String> = second_request
        .iter()
        .filter(|m| m.role == mirri_core::message::Role::Assistant)
        .map(|m| m.text_content())
        .collect();
    assert!(
        assistant_texts
            .iter()
            .any(|t| t.contains("Partial answer: the first half")),
        "the second request must include the delivered prefix, got: {assistant_texts:?}"
    );
    assert!(
        assistant_texts
            .iter()
            .any(|t| t.contains("[Request interrupted]")),
        "the second request must include the interrupted anchor, got: {assistant_texts:?}"
    );
}

#[tokio::test]
async fn should_record_interrupted_closure_events_to_wire_when_recorder_installed() {
    // Given an LLM whose stream is cancelled mid-flight, and a wire recorder
    let cancel = CancelToken::new();
    let llm = MockLlm::new(vec![MockResponse::interrupted_after("Halfway")])
        .with_cancel_on_chat(cancel.clone());
    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let builders = memory_backed_builders(memory.clone());
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let recorder = CollectingWireRecorder(Arc::clone(&recorded));

    // When the turn aborts
    let result = run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        None,
        None,
        Some(&recorder as &dyn WireOpRecorder),
    )
    .await
    .expect("turn should return, not error");
    assert_eq!(result.stop_reason, TurnStopReason::Aborted);

    // Then the wire log carries the interrupted closure: the delivered
    // content part, the anchor content part, and a step.end — replay of the
    // log rebuilds the identical closed state.
    let ops = recorded.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let types: Vec<String> = ops.iter().filter_map(loop_event_type).collect();
    assert!(
        types.iter().any(|t| t == "step.begin"),
        "step.begin must be recorded, got {types:?}"
    );
    assert!(
        types.iter().filter(|t| t == &"content.part").count() >= 2,
        "delivered part + anchor part must be recorded, got {types:?}"
    );
    assert!(
        types.iter().any(|t| t == "step.end"),
        "the closure step.end must be recorded, got {types:?}"
    );
    // The anchor is on the wire as a content part.
    let has_anchor = ops.iter().any(|op| {
        if let mirri_proto::WireOp::ContextAppendLoopEvent { event } = op {
            event
                .get("part")
                .and_then(|p| p.get("text"))
                .and_then(serde_json::Value::as_str)
                .is_some_and(|t| t.contains("[Request interrupted]"))
        } else {
            false
        }
    });
    assert!(has_anchor, "the interrupted anchor must be on the wire");
}

/// A resolver that serves one instance for the first N resolves, then
/// switches — the deterministic mid-turn model switch.
struct SwitchingResolver {
    first: Arc<MockLlm>,
    second: Arc<MockLlm>,
    calls: std::sync::atomic::AtomicUsize,
    switch_at: usize,
}

impl LlmResolver for SwitchingResolver {
    fn resolve(&self) -> Result<Arc<dyn Llm>, String> {
        let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(if n < self.switch_at {
            self.first.clone()
        } else {
            self.second.clone()
        })
    }
}

#[tokio::test]
async fn should_use_the_switched_llm_for_the_next_step_when_the_resolver_changes_mid_turn() {
    // Given a turn whose first LLM answers with a tool call and whose
    // resolver switches to a second LLM after that step
    let first = Arc::new(MockLlm::new(vec![MockResponse::tool_use(vec![
        make_tool_call("call-1", "Echo", serde_json::json!({"message": "hi"})),
    ])]));
    let second = Arc::new(MockLlm::new(vec![MockResponse::end_turn("done by second")]));
    let resolver = SwitchingResolver {
        first: Arc::clone(&first),
        second: Arc::clone(&second),
        calls: std::sync::atomic::AtomicUsize::new(0),
        switch_at: 1,
    };
    let hooks = TestHooks::default();
    let tools = make_tool_manager_with_echo();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = make_message_builders();

    // When the turn runs with the echo tool registered
    let result = run_turn(
        RunTurnConfig::new("turn-1"),
        &resolver,
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        None,
        None,
        None,
    )
    .await
    .expect("turn should succeed");

    // Then the turn ran two steps and the second LLM served the second
    // step (the switch took effect on the next LLM call, not the next turn)
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);
    assert_eq!(result.steps, 2);
    assert!(
        second.served_calls() >= 1,
        "the switched-to LLM must serve the post-switch step"
    );
    assert_eq!(
        first.served_calls(),
        1,
        "the pre-switch LLM serves exactly the first step"
    );
}

#[tokio::test]
async fn should_record_request_header_changed_when_media_fallback_switches_projection() {
    // Given an LLM whose first response is rejected as too large, and a
    // media-degraded fallback builder, with a wire recorder installed
    let llm = MockLlm::new(vec![
        MockResponse::error("413 Payload Too Large"),
        MockResponse::end_turn("Success with degraded"),
    ]);
    let hooks = TestHooks::default();
    let tools = ToolManager::new();
    let mut validators = ValidatorCache::new();
    let memory = Arc::new(Mutex::new(ContextMemory::new()));
    let cancel = CancelToken::new();
    let builders = MessageBuilders {
        normal: Box::new(|| Ok(vec![Message::user("Hello")])),
        media_degraded: Some(Box::new(|| Ok(vec![Message::user("Hello degraded")]))),
        media_stripped: None,
        strict: None,
        format_rejected_flag: Arc::new(AtomicBool::new(false)),
    };
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let recorder = CollectingWireRecorder(Arc::clone(&recorded));

    // When the turn runs and the fallback projection kicks in
    let result = run_turn(
        RunTurnConfig::new("turn-1"),
        &FixedLlmResolver::new(Arc::new(llm)),
        &hooks,
        &tools,
        &mut validators,
        memory.clone(),
        &cancel,
        &builders,
        None,
        None,
        Some(&recorder as &dyn WireOpRecorder),
    )
    .await
    .expect("turn should succeed with the fallback");
    assert_eq!(result.stop_reason, TurnStopReason::EndTurn);

    // Then the wire log carries the explicit request-envelope drift marker,
    // and a reducer replay of the log can audit it after resume.
    let ops = recorded.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let mut state = mirri_proto::AgentState::new();
    let mut header_events = 0;
    for op in &ops {
        let (_, events) = mirri_proto::reducer::reduce(&state, op);
        if matches!(op, mirri_proto::WireOp::RequestHeaderChanged { .. }) {
            header_events += 1;
        }
        for event in events {
            if let mirri_proto::DomainEvent::RequestHeaderChanged { reason } = event {
                assert_eq!(reason, "media_projection_degraded");
            }
        }
        let (next, _) = mirri_proto::reducer::reduce(&state, op);
        state = next;
    }
    assert_eq!(
        header_events, 1,
        "exactly one request.header.changed op must be recorded"
    );
    let header = state
        .request_header
        .expect("the reducer must fold the request header state");
    assert_eq!(header.change_count, 1);
    assert_eq!(header.last_reason, "media_projection_degraded");
}

}

mod background_agent_ {
//! 后台任务管理器集成测试 — 覆盖心跳过期、agent 超时终止、输出隔离与截断、
//! 持久化恢复以及重启后 running→stopped 调谐等行为。
//!
//! 这些测试通过 [`TaskManager`] 的公共 API 验证端到端行为, 使用真实文件系统
//! 持久化 (tempfile) 和 wall-clock 时间 (短间隔 + sleep) 来模拟时间流逝。

use std::sync::Arc;
use std::time::Duration;

use mirri_core::task::{
    MAX_TASK_OUTPUT_BYTES, TASK_TIMEOUT_MESSAGE, TaskKind, TaskManager, TaskStatus,
};
use mirri_core::task_persist::TaskPersistence;

// ============================================================
// 心跳与过期检测
// ============================================================

/// 当心跳间隔已过且未刷新时, is_stale 应返回 true。
#[test]
fn should_mark_task_as_stale_when_heartbeat_expires() {
    // 给定: 一个配置了 50ms 心跳间隔的 TaskManager
    let manager = TaskManager::new().with_heartbeat_interval(Duration::from_millis(50));
    let task_id = manager.register(
        TaskKind::Agent {
            agent_id: None,
            subagent_type: None,
            parent_tool_call_id: None,
            swarm_index: None,
            model: None,
        },
        "stale-agent".into(),
        "session-1".into(),
    );

    // 刚注册时不应过期
    assert!(!manager.is_stale(&task_id), "新注册的任务不应立即过期");

    // 当: 等待超过心跳间隔
    std::thread::sleep(Duration::from_millis(100));

    // 则: is_stale 返回 true
    assert!(manager.is_stale(&task_id), "心跳过期后任务应被标记为 stale");
}

/// 当心跳间隔内刷新了心跳, is_stale 应返回 false。
#[test]
fn should_keep_task_alive_when_heartbeat_is_refreshed_within_interval() {
    // 给定: 一个配置了 200ms 心跳间隔的 TaskManager
    let manager = TaskManager::new().with_heartbeat_interval(Duration::from_millis(200));
    let task_id = manager.register(
        TaskKind::Agent {
            agent_id: None,
            subagent_type: None,
            parent_tool_call_id: None,
            swarm_index: None,
            model: None,
        },
        "alive-agent".into(),
        "session-1".into(),
    );

    // 当: 等待一半间隔后刷新心跳, 再等待一半间隔
    std::thread::sleep(Duration::from_millis(100));
    manager.heartbeat(&task_id);
    std::thread::sleep(Duration::from_millis(100));

    // 则: is_stale 仍返回 false (心跳在过期前被刷新)
    assert!(
        !manager.is_stale(&task_id),
        "心跳在过期前刷新, 任务不应被标记为 stale"
    );
}

/// 当未配置心跳间隔时, is_stale 始终返回 false。
#[test]
fn should_never_report_stale_when_no_heartbeat_interval_is_configured() {
    // 给定: 一个未配置心跳间隔的 TaskManager
    let manager = TaskManager::new();
    let task_id = manager.register(
        TaskKind::Agent {
            agent_id: None,
            subagent_type: None,
            parent_tool_call_id: None,
            swarm_index: None,
            model: None,
        },
        "no-heartbeat".into(),
        "session-1".into(),
    );

    // 当: 等待一段时间
    std::thread::sleep(Duration::from_millis(50));

    // 则: is_stale 返回 false
    assert!(
        !manager.is_stale(&task_id),
        "未配置心跳间隔时, 任务永远不应被标记为 stale"
    );
}

// ============================================================
// Agent 超时终止
// ============================================================

/// 当 agent_timeout 到期时, terminate_expired 应终止任务并返回其 task_id。
#[test]
fn should_terminate_task_when_agent_timeout_is_reached() {
    // 给定: 一个配置了 50ms agent_timeout 的 TaskManager 和一个 running 任务
    let manager = TaskManager::new().with_agent_timeout(Duration::from_millis(50));
    let task_id = manager.register(
        TaskKind::Agent {
            agent_id: None,
            subagent_type: None,
            parent_tool_call_id: None,
            swarm_index: None,
            model: None,
        },
        "slow-agent".into(),
        "session-1".into(),
    );

    // 当: 等待超过 agent_timeout 后调用 terminate_expired
    std::thread::sleep(Duration::from_millis(100));
    let terminated = manager.terminate_expired();

    // 则: 返回的列表包含该 task_id, 且任务状态变为 Failed
    assert_eq!(terminated, vec![task_id.clone()]);
    let task = manager.get(&task_id).expect("任务应存在");
    assert_eq!(
        task.status,
        TaskStatus::Failed(TASK_TIMEOUT_MESSAGE.to_string())
    );
}

/// 当未配置 agent_timeout 时, terminate_expired 应返回空列表。
#[test]
fn should_not_terminate_when_no_agent_timeout_is_configured() {
    // 给定: 一个未配置 agent_timeout 的 TaskManager 和一个 running 任务
    let manager = TaskManager::new();
    let task_id = manager.register(
        TaskKind::Agent {
            agent_id: None,
            subagent_type: None,
            parent_tool_call_id: None,
            swarm_index: None,
            model: None,
        },
        "no-timeout".into(),
        "session-1".into(),
    );

    // 当: 等待一段时间后调用 terminate_expired
    std::thread::sleep(Duration::from_millis(50));
    let terminated = manager.terminate_expired();

    // 则: 返回空列表, 任务仍为 Running
    assert!(
        terminated.is_empty(),
        "未配置 agent_timeout 时不应终止任何任务"
    );
    let task = manager.get(&task_id).expect("任务应存在");
    assert_eq!(task.status, TaskStatus::Running);
}

// ============================================================
// 输出访问隔离
// ============================================================

/// 当使用非创建会话访问任务输出时, 应被拒绝 (返回 None)。
#[test]
fn should_restrict_output_access_to_creating_session() {
    // 给定: 会话 A 创建的任务, 并写入了输出
    let manager = TaskManager::new();
    let task_id = manager.register(
        TaskKind::Agent {
            agent_id: None,
            subagent_type: None,
            parent_tool_call_id: None,
            swarm_index: None,
            model: None,
        },
        "session-scoped".into(),
        "session-A".into(),
    );
    manager.append_output(&task_id, "secret output\n");

    // 当: 用会话 B 尝试访问输出
    let result_b = manager.output_for_session(&task_id, "session-B");

    // 则: 返回 None (拒绝访问)
    assert!(result_b.is_none(), "非创建会话不应能访问任务输出");

    // 同时验证创建会话 A 仍可正常访问
    let result_a = manager.output_for_session(&task_id, "session-A");
    assert!(result_a.is_some(), "创建会话应能访问任务输出");
    let output = result_a.unwrap();
    assert!(output.full_output.contains("secret output"));
}

// ============================================================
// 输出截断
// ============================================================

/// 当输出超过上限时, 应被截断并标记为 truncated。
#[test]
fn should_truncate_output_when_exceeding_limit() {
    // 给定: 一个任务
    let manager = TaskManager::new();
    let task_id = manager.register(
        TaskKind::Agent {
            agent_id: None,
            subagent_type: None,
            parent_tool_call_id: None,
            swarm_index: None,
            model: None,
        },
        "chatty-task".into(),
        "session-1".into(),
    );

    // 当: 写入超过 MAX_TASK_OUTPUT_BYTES 的输出
    let head = "H".repeat(3 * MAX_TASK_OUTPUT_BYTES / 4);
    let tail = "T".repeat(3 * MAX_TASK_OUTPUT_BYTES / 4);
    manager.append_output(&task_id, &head);
    manager.append_output(&task_id, &tail);

    // 则: 输出被截断, 长度不超过上限, 且保留了最近的尾部
    let output = manager.output(&task_id).expect("任务应存在");
    assert!(output.truncated, "超过上限的输出应被标记为 truncated");
    assert!(
        output.full_output.len() <= MAX_TASK_OUTPUT_BYTES,
        "截断后输出不应超过上限"
    );
    assert!(output.full_output.ends_with('T'), "截断应保留最近的输出");
    assert!(
        output.preview.contains("[output truncated]"),
        "预览应包含截断标记"
    );
}

// ============================================================
// 持久化与恢复
// ============================================================

/// 当 TaskManager 配置了持久化时, 注册的任务应被写入磁盘, 并可通过新的
/// TaskManager 恢复。
#[test]
fn should_persist_and_restore_background_task_state() {
    // 给定: 一个带持久化的 TaskManager, 注册并完成任务
    let tmp = tempfile::tempdir().unwrap();
    let tasks_dir = tmp.path().join("tasks");
    let persistence = Arc::new(TaskPersistence::new(
        tasks_dir.clone(),
        std::sync::Arc::new(mirri_os::LocalSyncFs::new()),
    ));
    let manager = TaskManager::with_persistence(persistence);

    let task_id = manager.register(
        TaskKind::Agent {
            agent_id: None,
            subagent_type: None,
            parent_tool_call_id: None,
            swarm_index: None,
            model: None,
        },
        "persisted-task".into(),
        "session-1".into(),
    );
    manager.append_output(&task_id, "some output\n");
    manager.complete(&task_id, 0);

    // 当: 用同一个持久化目录创建新的 TaskManager (模拟重启)
    let persistence2 = Arc::new(TaskPersistence::new(
        tasks_dir,
        std::sync::Arc::new(mirri_os::LocalSyncFs::new()),
    ));
    let restored_manager = TaskManager::with_persistence(persistence2);

    // 则: 任务被恢复, 状态和输出保持一致
    let restored_task = restored_manager
        .get(&task_id)
        .expect("持久化的任务应被恢复");
    assert_eq!(restored_task.id, task_id);
    assert_eq!(restored_task.description, "persisted-task");
    assert_eq!(restored_task.status, TaskStatus::Completed { exit_code: 0 });
    assert!(restored_task.output.contains("some output"));
}

/// 当从磁盘加载 running 状态的任务时, 应被调谐为 stopped (重启后无活跃进程)。
#[test]
fn should_reconcile_running_tasks_as_stopped_on_load() {
    // 给定: 磁盘上有一个 running 状态的任务
    let tmp = tempfile::tempdir().unwrap();
    let tasks_dir = tmp.path().join("tasks");
    let persistence = Arc::new(TaskPersistence::new(
        tasks_dir.clone(),
        std::sync::Arc::new(mirri_os::LocalSyncFs::new()),
    ));

    let manager = TaskManager::with_persistence(Arc::clone(&persistence));
    let task_id = manager.register(
        TaskKind::Agent {
            agent_id: None,
            subagent_type: None,
            parent_tool_call_id: None,
            swarm_index: None,
            model: None,
        },
        "running-task".into(),
        "session-1".into(),
    );
    // 任务此时是 Running 状态, 已被持久化到磁盘

    // 当: 用同一持久化目录创建新的 TaskManager (模拟进程重启)
    let persistence2 = Arc::new(TaskPersistence::new(
        tasks_dir.clone(),
        std::sync::Arc::new(mirri_os::LocalSyncFs::new()),
    ));
    let restarted_manager = TaskManager::with_persistence(persistence2);

    // 则: 任务被恢复, 但状态调谐为 Stopped
    let restored_task = restarted_manager.get(&task_id).expect("任务应被恢复");
    assert_eq!(
        restored_task.status,
        TaskStatus::Stopped,
        "重启后 running 任务应被调谐为 stopped"
    );

    // 磁盘上的状态也应被更新为 Stopped
    let on_disk: mirri_core::task::AgentTask = serde_json::from_str(
        &std::fs::read_to_string(tasks_dir.join(format!("{task_id}.json"))).unwrap(),
    )
    .unwrap();
    assert_eq!(on_disk.status, TaskStatus::Stopped);
}

// ============================================================
// B1 T3: 优雅停止 — 宽限期 + 自然定局优先 + 后台进程真实可杀
// ============================================================

/// 当挂接的信号让任务在宽限窗口内自然定局时, stop_with_grace 应保留
/// 自然终态 — 而不是过早翻转成 killed。
#[tokio::test]
async fn should_keep_the_natural_terminal_status_when_graceful_stop_settles_in_time() {
    use mirri_core::cancel::CancelToken;
    use mirri_core::task::TaskManager;

    let tasks = Arc::new(TaskManager::new());
    let id = tasks.register(
        TaskKind::Agent {
            agent_id: Some("agent-1".into()),
            subagent_type: Some("coder".into()),
            parent_tool_call_id: None,
            swarm_index: None,
            model: None,
        },
        "quick".into(),
        "session-1".into(),
    );
    let signal = CancelToken::new();
    tasks.attach_signal(&id, signal.clone());
    // 模拟子代理观察到取消后 50ms 自然定局。
    let mgr = Arc::clone(&tasks);
    let id_clone = id.clone();
    tokio::spawn(async move {
        signal.cancelled().await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        mgr.complete(&id_clone, 0);
    });

    tasks.stop_with_grace(&id, Duration::from_secs(2)).await;

    assert_eq!(
        tasks.get(&id).unwrap().status,
        TaskStatus::Completed { exit_code: 0 },
        "graceful stop must not flip a task that settles naturally in-window"
    );
}

/// 当任务在宽限期内从不定局时, stop_with_grace 应升级为 Stopped
/// (TS forceStop 语义)。挂接一个永不 settle 的信号, 真正走过等待路径。
#[tokio::test]
async fn should_escalate_to_stopped_when_grace_expires_without_a_settlement() {
    use mirri_core::cancel::CancelToken;

    let tasks = TaskManager::new();
    let id = tasks.register(
        TaskKind::Process {
            command: None,
            dispatcher: None,
        },
        "stuck".into(),
        "session-1".into(),
    );
    // 挂接信号但 runner 从不定局 —— 等待路径必须走满 grace 后升级。
    tasks.attach_signal(&id, CancelToken::new());
    let started = std::time::Instant::now();
    tasks.stop_with_grace(&id, Duration::from_millis(150)).await;
    assert_eq!(tasks.get(&id).unwrap().status, TaskStatus::Stopped);
    assert!(
        started.elapsed() >= Duration::from_millis(140),
        "the escalation must wait out the grace window before flipping (took {:?})",
        started.elapsed()
    );
}

/// 当 stop_all_with_grace 面对混合任务集时, 已定局者保留原终态、
/// 挂死者升级 Stopped。
#[tokio::test]
async fn should_stop_only_unsettled_tasks_when_stopping_all_with_grace() {
    let tasks = TaskManager::new();
    let settled = tasks.register(
        TaskKind::Process {
            command: None,
            dispatcher: None,
        },
        "settled".into(),
        "session-1".into(),
    );
    let stuck = tasks.register(
        TaskKind::Process {
            command: None,
            dispatcher: None,
        },
        "stuck".into(),
        "session-1".into(),
    );
    tasks.complete(&settled, 0);

    tasks.stop_all_with_grace(Duration::from_millis(100)).await;

    assert_eq!(
        tasks.get(&settled).unwrap().status,
        TaskStatus::Completed { exit_code: 0 }
    );
    assert_eq!(tasks.get(&stuck).unwrap().status, TaskStatus::Stopped);
}

/// 当真实的后台 `sleep 60` 进程任务被宽限停止时, OS 进程应被真实终止
/// (SIGTERM→SIGKILL 经 mirri-os) — 修复前的代码只翻转状态并留下孤儿进程。
#[cfg(unix)]
#[tokio::test]
#[ignore] // R-layer: real subprocess sampling — SIGTERM→SIGKILL escalation observed on a real process (design §3)
async fn should_kill_the_real_process_when_a_background_bash_task_is_stopped() {
    use mirri_core::tools::bash::start_background_command;

    let process = Arc::new(mirri_os::LocalProcess::new());
    let tasks = Arc::new(TaskManager::new());
    let started = start_background_command(
        process.clone(),
        "sleep 60".into(),
        None,
        None,
        false,
        "orphan check".into(),
        Arc::clone(&tasks),
        "session-1".into(),
        "agent-main".into(),
    )
    .await;
    let started_text = match &started.output {
        mirri_core::tool::ToolOutput::Text(text) => text.clone(),
        _ => panic!("expected text output"),
    };
    let task_id = started_text
        .lines()
        .find_map(|line| line.strip_prefix("task_id: "))
        .expect("background result carries the task id")
        .to_string();

    let alive = |pid: i32| {
        mirri_os::probe_process(pid)
            .map(|probe| probe.alive)
            .unwrap_or(false)
    };
    // Identify the spawned process through the task's own fingerprint
    // instead of matching its command line over the whole machine: the
    // workspace suite runs every test binary in parallel, so a
    // `pgrep -f "sleep 60"` here also sees the `sleep 60` processes other
    // tests leave running (the orphan-governance keepAlive fixtures), and
    // the post-stop liveness assertion could never hold.
    let pid = {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(fingerprint) = tasks.get(&task_id).and_then(|task| task.fingerprint) {
                break fingerprint.pid;
            }
            if std::time::Instant::now() >= deadline {
                panic!("the background task must record its process fingerprint");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    // The command spawns on a detached task — wait for the process to
    // appear before asserting liveness (start latency, not a semantic
    // race).
    let appeared = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < appeared && !alive(pid) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        alive(pid),
        "the background process should be alive before the stop"
    );

    tasks
        .stop_with_grace(&task_id, Duration::from_secs(2))
        .await;

    // 进程组内 SIGTERM 已送达；宽限升级后必然退出。
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    while std::time::Instant::now() < deadline && alive(pid) {
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        !alive(pid),
        "the stopped background task's process must be dead"
    );
}

// ============================================================
// B1 T7: 会话关闭 — persisted-killed 语义核验 + keepAliveOnExit
// ============================================================

/// 构建一个带 tasks 持久化根的 SessionManager（tasks 落在
/// `<tmp>/.mirri/v3_sessions/<session>/tasks/`）。
async fn b1_manager() -> (
    mirri_core::SessionManager,
    tempfile::TempDir,
    std::path::PathBuf,
) {
    use mirri_core::scope::{WorkspaceId, WorkspaceScopeConfig, WorkspaceTrustLevel};
    use mirri_core::{SessionManager, WorkspaceScope};

    let tmp = tempfile::tempdir().unwrap();
    let workspace = std::sync::Arc::new(WorkspaceScope::new(
        WorkspaceId::new("test-ws"),
        tmp.path().to_path_buf(),
        WorkspaceScopeConfig {
            trust_level: WorkspaceTrustLevel::Trusted,
            ..Default::default()
        },
    ));
    let sessions_dir = tmp.path().join(".mirri").join("v3_sessions");
    let manager = SessionManager::with_persistence_dir(
        workspace,
        sessions_dir.clone(),
        std::sync::Arc::new(mirri_os::LocalFs::new()),
    );
    (manager, tmp, sessions_dir)
}

/// 在持久化根下查找任务的 on-disk JSON 路径。
fn b1_task_json(sessions_dir: &std::path::Path, task_id: &str) -> std::path::PathBuf {
    let pattern = sessions_dir
        .join("**")
        .join("tasks")
        .join(format!("{task_id}.json"));
    let mut results = Vec::new();
    fn walk(dir: &std::path::Path, name: &str, out: &mut Vec<std::path::PathBuf>) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, name, out);
                } else if path.file_name().map(|n| n == name).unwrap_or(false) {
                    out.push(path);
                }
            }
        }
    }
    walk(
        sessions_dir,
        pattern.file_name().unwrap().to_str().unwrap(),
        &mut results,
    );
    assert_eq!(results.len(), 1, "expected exactly one persisted task file");
    results.remove(0)
}

/// 当会话关闭 (keep_alive_on_exit = false) 时, 运行中的任务应落 Stopped
/// 并已持久化; 从同一持久化根重新加载不再二次调谐 (无复活), 且 close 后
/// 迟到的自然定局不改变 Stopped —— Rust 以「同步落盘 Stopped」实现 TS 的
/// lost 语义 (D-62 裁决: 不新增 lost wire 状态)。
#[tokio::test]
async fn should_persist_a_stopped_task_and_not_re_reconcile_it_when_a_session_closes() {
    use mirri_core::CloseReason;
    use mirri_core::scope::SessionScopeConfig;
    use mirri_core::scope::TaskLifecycleSettings;

    let (manager, tmp, sessions_dir) = b1_manager().await;
    let metadata = mirri_core::scope::SessionMetadata::default();
    let session = manager
        .create_session(
            metadata,
            SessionScopeConfig {
                task_lifecycle: TaskLifecycleSettings {
                    keep_alive_on_exit: false,
                    kill_grace: Duration::from_millis(200),
                    ..TaskLifecycleSettings::default()
                },
                ..SessionScopeConfig::default()
            },
        )
        .await
        .unwrap();
    let session_id = session.id().clone();
    let tasks = session.tasks();
    let task_id = tasks.register(
        TaskKind::Agent {
            agent_id: None,
            subagent_type: None,
            parent_tool_call_id: None,
            swarm_index: None,
            model: None,
        },
        "converged".into(),
        session_id.to_string(),
    );

    manager
        .close_session(&session_id, CloseReason::UserInitiated)
        .await
        .unwrap();

    // 断言 1: 内存态 Stopped, 磁盘 JSON status == stopped。
    assert_eq!(tasks.get(&task_id).unwrap().status, TaskStatus::Stopped);
    let on_disk: mirri_core::task::AgentTask = serde_json::from_str(
        &std::fs::read_to_string(b1_task_json(&sessions_dir, &task_id)).unwrap(),
    )
    .unwrap();
    assert_eq!(on_disk.status, TaskStatus::Stopped);
    let ended_at = on_disk.ended_at;

    // 断言 2: 同根重载 → 仍 Stopped, ended_at 不变 (重启不再触碰)。
    let tasks_dir = b1_task_json(&sessions_dir, &task_id)
        .parent()
        .unwrap()
        .to_path_buf();
    let reloaded = TaskManager::with_persistence(std::sync::Arc::new(TaskPersistence::new(
        tasks_dir,
        std::sync::Arc::new(mirri_os::LocalSyncFs::new()),
    )));
    let reloaded_task = reloaded.get(&task_id).unwrap();
    assert_eq!(reloaded_task.status, TaskStatus::Stopped);
    assert_eq!(reloaded_task.ended_at, ended_at);

    // 断言 3: close 后迟到的自然定局不改变 Stopped (first-terminal-wins 跨
    // close 存活)。
    tasks.complete(&task_id, 0);
    assert_eq!(tasks.get(&task_id).unwrap().status, TaskStatus::Stopped);
    drop(tmp);
}

/// 当 keep_alive_on_exit = true 时, 会话关闭应跳过任务收敛: 任务保持
/// Running, 随后仍可自然定局; 仍 running 的任务在下次重载时被调谐为
/// Stopped (TS lost 的重启等价物)。
#[tokio::test]
async fn should_skip_task_convergence_when_keep_alive_on_exit_is_enabled() {
    use mirri_core::CloseReason;
    use mirri_core::scope::SessionScopeConfig;
    use mirri_core::scope::TaskLifecycleSettings;

    let (manager, tmp, sessions_dir) = b1_manager().await;
    let session = manager
        .create_session(
            mirri_core::scope::SessionMetadata::default(),
            SessionScopeConfig {
                task_lifecycle: TaskLifecycleSettings {
                    keep_alive_on_exit: true,
                    ..TaskLifecycleSettings::default()
                },
                ..SessionScopeConfig::default()
            },
        )
        .await
        .unwrap();
    let session_id = session.id().clone();
    let tasks = session.tasks();
    let settleable = tasks.register(
        TaskKind::Agent {
            agent_id: None,
            subagent_type: None,
            parent_tool_call_id: None,
            swarm_index: None,
            model: None,
        },
        "keeps running".into(),
        session_id.to_string(),
    );
    let stuck = tasks.register(
        TaskKind::Agent {
            agent_id: None,
            subagent_type: None,
            parent_tool_call_id: None,
            swarm_index: None,
            model: None,
        },
        "stays running".into(),
        session_id.to_string(),
    );

    manager
        .close_session(&session_id, CloseReason::UserInitiated)
        .await
        .unwrap();

    // 收敛被跳过: 两个任务仍 Running。
    assert_eq!(tasks.get(&settleable).unwrap().status, TaskStatus::Running);
    assert_eq!(tasks.get(&stuck).unwrap().status, TaskStatus::Running);

    // keepAlive 关闭的豁免落盘 (SDR-013 d): 两个幸存的 Running 行都在
    // 内存中带 lifetime_exempt — 常驻寿命清扫器不得在会话 (及其 driver)
    // 消失后按年龄上限清杀它们。
    assert!(
        tasks.get(&settleable).unwrap().lifetime_exempt,
        "the surviving row must carry the lifetime exemption in memory"
    );
    assert!(
        tasks.get(&stuck).unwrap().lifetime_exempt,
        "the surviving row must carry the lifetime exemption in memory"
    );
    // ...且豁免已持久化到磁盘档案 — 清扫器跨重启判定的是档案, 不是内存。
    let settled_json = std::fs::read_to_string(b1_task_json(&sessions_dir, &stuck)).unwrap();
    let archived: serde_json::Value = serde_json::from_str(&settled_json).unwrap();
    assert_eq!(
        archived["lifetime_exempt"],
        serde_json::json!(true),
        "the exemption must be persisted in the archive the sweeper walks"
    );

    // 随后的自然定局仍然生效 (TaskManager Arc 被脱离会话的 runner 持有)。
    tasks.complete(&settleable, 0);
    assert_eq!(
        tasks.get(&settleable).unwrap().status,
        TaskStatus::Completed { exit_code: 0 }
    );

    // 重启语义: 仍 running 的任务重载时被调谐为 Stopped (TS lost 等价)。
    let tasks_dir = b1_task_json(&sessions_dir, &stuck)
        .parent()
        .unwrap()
        .to_path_buf();
    let reloaded = TaskManager::with_persistence(std::sync::Arc::new(TaskPersistence::new(
        tasks_dir,
        std::sync::Arc::new(mirri_os::LocalSyncFs::new()),
    )));
    assert_eq!(reloaded.get(&stuck).unwrap().status, TaskStatus::Stopped);
    drop(tmp);
}

/// 当任务在关闭宽限窗口内自然定局时 (D-63 × close 路径集成), 其自然终态
/// 应被保留而非翻转成 killed。
#[tokio::test]
async fn should_keep_a_natural_settlement_when_the_session_close_grace_window_settles() {
    use mirri_core::CloseReason;
    use mirri_core::cancel::CancelToken;
    use mirri_core::scope::SessionScopeConfig;
    use mirri_core::scope::TaskLifecycleSettings;

    let (manager, tmp, _sessions_dir) = b1_manager().await;
    let session = manager
        .create_session(
            mirri_core::scope::SessionMetadata::default(),
            SessionScopeConfig {
                task_lifecycle: TaskLifecycleSettings {
                    keep_alive_on_exit: false,
                    kill_grace: Duration::from_secs(2),
                    ..TaskLifecycleSettings::default()
                },
                ..SessionScopeConfig::default()
            },
        )
        .await
        .unwrap();
    let session_id = session.id().clone();
    let tasks = session.tasks();
    let task_id = tasks.register(
        TaskKind::Agent {
            agent_id: Some("agent-quick".into()),
            subagent_type: Some("coder".into()),
            parent_tool_call_id: None,
            swarm_index: None,
            model: None,
        },
        "settles in window".into(),
        session_id.to_string(),
    );
    let signal = CancelToken::new();
    tasks.attach_signal(&task_id, signal.clone());
    // 模拟 runner 观察取消后 50ms 自然定局。
    let runner_tasks = tasks.clone();
    let runner_id = task_id.clone();
    tokio::spawn(async move {
        signal.cancelled().await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        runner_tasks.complete(&runner_id, 0);
    });

    manager
        .close_session(&session_id, CloseReason::UserInitiated)
        .await
        .unwrap();

    assert_eq!(
        tasks.get(&task_id).unwrap().status,
        TaskStatus::Completed { exit_code: 0 },
        "a task that settles inside the close grace window keeps its natural status"
    );
    drop(tmp);
}

}

