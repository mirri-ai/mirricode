//! Deadline-bounded condition polling for tests.
//!
//! This helper exists for tests that cannot use a paused clock (real IO,
//! subprocesses, network): instead of a bare fixed `sleep` used as a
//! synchronization primitive, poll a condition with a small step until a
//! deadline. The `thread::sleep` inside is the stepping mechanism, not the
//! banned object — the isolation contract (16-test-execution-strategy.md
//! §4.2 item 4) forbids bare fixed sleeps in test code, with helper-internal
//! stepping (≤50ms) explicitly exempt.

use std::fmt;
use std::thread;
use std::time::{Duration, Instant};

/// Error returned when `wait_for` exceeds its deadline.
#[derive(Debug)]
pub struct WaitTimeout {
    /// How much time elapsed before giving up.
    pub elapsed: Duration,
}

impl fmt::Display for WaitTimeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "condition not met within {:?}", self.elapsed)
    }
}

impl std::error::Error for WaitTimeout {}

/// Poll `f` every `step` until it returns `Some`, or until `timeout` elapses.
///
/// Returns the value produced by `f` on success, or a [`WaitTimeout`] error
/// carrying the elapsed time. Synchronous — for tests whose progress does
/// not depend on the current thread being released (pure state, other
/// threads, subprocesses). Async tests that observe runtime-driven work
/// should use [`wait_for_async`] instead.
pub fn wait_for<F, T>(timeout: Duration, step: Duration, mut f: F) -> Result<T, WaitTimeout>
where
    F: FnMut() -> Option<T>,
{
    let started = Instant::now();
    loop {
        if let Some(value) = f() {
            return Ok(value);
        }
        if started.elapsed() >= timeout {
            return Err(WaitTimeout {
                elapsed: started.elapsed(),
            });
        }
        thread::sleep(step);
    }
}

/// Async sibling of [`wait_for`]: same check-first / step-later contract,
/// but the step is `tokio::time::sleep`, so the runtime can drive the
/// background work this poll is waiting on. For async tests that observe
/// runtime-driven state — the sync [`wait_for`] would block the thread
/// that must make progress.
pub async fn wait_for_async<F, T>(
    timeout: Duration,
    step: Duration,
    mut f: F,
) -> Result<T, WaitTimeout>
where
    F: FnMut() -> Option<T>,
{
    let started = std::time::Instant::now();
    loop {
        if let Some(value) = f() {
            return Ok(value);
        }
        if started.elapsed() >= timeout {
            return Err(WaitTimeout {
                elapsed: started.elapsed(),
            });
        }
        tokio::time::sleep(step).await;
    }
}

/// Assert that nothing arrives on `rx` for `window` — the negative-window
/// form ("prove absence of further events") that otherwise needs a bare
/// sleep at every call site. The window is real elapsed time by design:
/// absence can only be proven by observing it.
pub async fn assert_no_event_for<T>(window: Duration, rx: &mut tokio::sync::mpsc::Receiver<T>)
where
    T: fmt::Debug,
{
    match tokio::time::timeout(window, rx.recv()).await {
        Err(_) => {}
        Ok(Some(event)) => panic!("unexpected event during the negative window: {event:?}"),
        Ok(None) => panic!("channel closed during the negative window"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_return_immediately_when_condition_holds() {
        let started = Instant::now();
        let result = wait_for(Duration::from_secs(10), Duration::from_millis(1), || Some(42));
        assert_eq!(result.expect("condition holds"), 42);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "must not wait when the condition already holds"
        );
    }

    #[test]
    fn should_timeout_when_condition_never_holds() {
        let timeout = Duration::from_millis(10);
        let result: Result<(), WaitTimeout> = wait_for(timeout, Duration::from_millis(1), || None);
        let err = result.expect_err("condition never holds");
        assert!(err.elapsed >= timeout, "elapsed should cover the deadline");
    }

    #[tokio::test]
    async fn should_return_immediately_when_the_async_condition_holds() {
        let started = std::time::Instant::now();
        let result = wait_for_async(Duration::from_secs(10), Duration::from_millis(1), || Some(7))
            .await
            .expect("condition holds");
        assert_eq!(result, 7);
        assert!(started.elapsed() < Duration::from_secs(5), "must not wait");
    }

    #[tokio::test]
    async fn should_yield_to_the_runtime_between_async_polls() {
        // Given a condition that becomes true only after a spawned task runs
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let setter = std::sync::Arc::clone(&flag);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(5)).await;
            setter.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        // When polled through wait_for_async, the sleep yields let the
        // setter task run — the condition resolves instead of deadlocking
        let result = wait_for_async(Duration::from_secs(2), Duration::from_millis(5), || {
            flag.load(std::sync::atomic::Ordering::SeqCst).then_some(())
        })
        .await;
        assert!(result.is_ok(), "the yield between polls must let tasks run");
    }

    #[tokio::test]
    async fn should_fail_the_async_poll_when_the_condition_never_holds() {
        let result: Result<(), WaitTimeout> =
            wait_for_async(Duration::from_millis(10), Duration::from_millis(1), || None).await;
        assert!(result.expect_err("condition never holds").elapsed >= Duration::from_millis(10));
    }

    #[tokio::test]
    async fn should_pass_when_no_event_arrives_within_the_window() {
        let (_tx, mut rx) = tokio::sync::mpsc::channel::<u32>(1);
        assert_no_event_for(Duration::from_millis(20), &mut rx).await;
    }

    #[tokio::test]
    #[should_panic(expected = "unexpected event")]
    async fn should_panic_when_an_event_arrives_within_the_window() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<u32>(1);
        tx.send(9).await.unwrap();
        assert_no_event_for(Duration::from_millis(500), &mut rx).await;
    }
}
