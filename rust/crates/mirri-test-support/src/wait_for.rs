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
/// carrying the elapsed time. Synchronous by design — this helper targets
/// non-paused tests, so a `Future`-returning variant would drag in a tokio
/// dependency for no current consumer.
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
}
