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
