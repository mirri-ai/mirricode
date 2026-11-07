//! Workspace-wide mutex for process-level environment mutation, plus the
//! save/restore [`EnvGuard`] built on top of it.
//!
//! Per the isolation contract (16-test-execution-strategy.md §4.2), any test
//! that must call `std::env::set_var` / `remove_var` has to hold this lock and
//! cover the minimal set of variables. All crate-local static env mutexes in
//! the workspace must be converged onto this single provider.

use std::sync::{Mutex, MutexGuard, OnceLock};

static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// Acquire the single workspace-wide mutex guarding process-level env mutation.
///
/// # Usage rules
///
/// - Never hold the returned guard across an `.await` point: under a
///   multi-threaded runtime this risks thread starvation deadlock. Finish
///   set/restore before entering an async block, or inject via
///   `Command::env` for subprocesses instead. For tests that genuinely
///   need the guard to span `.await`s (e.g. holding `PATH` stable across a
///   subprocess spawn), use [`async_lock`] instead.
/// - A poisoned lock is recovered with `into_inner` (env state may be stale
///   after a panicking test, which the offending test's failure already
///   signals).
pub fn lock() -> MutexGuard<'static, ()> {
    ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Acquire the workspace env lock in an async-friendly way.
///
/// This is the ONLY sanctioned way to keep the env-mutation guard alive
/// across `.await` points: it takes the same underlying mutex as
/// [`lock()`], so sync and async users still exclude each other, but the
/// wait happens on the tokio cooperative mutex (the std mutex is held only
/// for the microsecond it takes the async mutex's guard to take over).
///
/// Because the tokio mutex serializes tasks rather than threads, holding
/// this across `.await` cannot starve the runtime.
pub async fn async_lock() -> AsyncEnvGuard {
    // Take the async mutex first, then the std mutex. Lock order is one-way
    // (async → std), so a sync `lock()` holder on another thread can never
    // circularly wait on the async mutex — it never touches it.
    static ASYNC_ENV_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    let guard = ASYNC_ENV_LOCK
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await;
    let std_guard = lock();
    AsyncEnvGuard {
        guard,
        std_guard,
        saved: Vec::new(),
    }
}

/// Guard returned by [`async_lock`]; releasing it releases both the async
/// and the underlying std env mutex. The guard is opaque on purpose —
/// extracting the inner tokio guard would silently drop the std half and
/// break the sync/async mutual exclusion invariant, so no such API exists.
///
/// Like [`EnvGuard`], it records every mutation it is asked to make and
/// restores the previous values on drop — the sanctioned way to hold a
/// process-env mutation across `.await` points (the sync [`EnvGuard`]
/// must not span them).
pub struct AsyncEnvGuard {
    #[allow(dead_code)]
    guard: tokio::sync::MutexGuard<'static, ()>,
    #[allow(dead_code)]
    std_guard: MutexGuard<'static, ()>,
    saved: Vec<(String, Option<std::ffi::OsString>)>,
}

impl AsyncEnvGuard {
    /// Set `key` to `value`, recording the previous value for restore on
    /// drop. Same contract as [`EnvGuard::set`], for guards that must
    /// span `.await` points.
    pub fn set(&mut self, key: &str, value: impl AsRef<std::ffi::OsStr>) {
        self.record(key);
        // SAFETY: both halves of the workspace env lock are held for this
        // guard's whole lifetime; no other thread reads or writes these
        // variables concurrently.
        unsafe { std::env::set_var(key, value.as_ref()) };
    }

    /// Remove `key`, recording its previous value for restore on drop.
    pub fn remove(&mut self, key: &str) {
        self.record(key);
        // SAFETY: same lock discipline as `set`.
        unsafe { std::env::remove_var(key) };
    }

    fn record(&mut self, key: &str) {
        self.saved.push((key.to_string(), std::env::var_os(key)));
    }
}

impl Drop for AsyncEnvGuard {
    fn drop(&mut self) {
        for (key, previous) in self.saved.drain(..).rev() {
            // SAFETY: the lock is still held while this guard's fields are
            // dropped (field drop happens after `Drop::drop`), so no other
            // thread can race the restore.
            match previous {
                Some(value) => unsafe { std::env::set_var(&key, value) },
                None => unsafe { std::env::remove_var(&key) },
            }
        }
    }
}

/// Try to acquire the workspace env lock without blocking.
///
/// Returns `None` when the lock is currently held (e.g. by a live
/// [`EnvGuard`]). Mostly useful for asserting that the lock is held.
pub fn try_lock() -> Option<MutexGuard<'static, ()>> {
    ENV_LOCK
        .get_or_init(|| Mutex::new(()))
        .try_lock()
        .ok()
}

/// RAII guard that records env mutations and restores previous values on drop.
///
/// The workspace env mutex is acquired in [`EnvGuard::new`] and held until the
/// guard is dropped, so all `set` / `set_many` / `remove` calls are already
/// under the lock. On drop, recorded values are restored first (still under
/// the lock), then the lock is released.
///
/// Because the guard owns the mutex for its whole lifetime, it must not be
/// held across `.await` points, sent to another thread, or leaked into
/// background tasks.
pub struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
    saved: Vec<(String, Option<String>)>,
}

impl EnvGuard {
    /// Acquire the workspace env lock and start tracking mutations.
    pub fn new() -> Self {
        Self {
            _lock: lock(),
            saved: Vec::new(),
        }
    }

    /// Set `key` to `value`, recording the previous value for restore.
    pub fn set(&mut self, key: &str, value: &str) {
        self.record(key);
        // SAFETY: the workspace env lock is held for this guard's whole
        // lifetime; no other thread reads or writes these variables
        // concurrently.
        unsafe { std::env::set_var(key, value) };
    }

    /// Set multiple variables at once (convenience over [`EnvGuard::set`]).
    pub fn set_many(&mut self, entries: &[(&str, &str)]) {
        for (key, value) in entries {
            self.set(key, value);
        }
    }

    /// Remove `key`, recording its previous value for restore.
    pub fn remove(&mut self, key: &str) {
        self.record(key);
        // SAFETY: same lock discipline as `set`.
        unsafe { std::env::remove_var(key) };
    }

    fn record(&mut self, key: &str) {
        self.saved
            .push((key.to_string(), std::env::var(key).ok()));
    }
}

impl Default for EnvGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, previous) in self.saved.drain(..).rev() {
            // SAFETY: the lock is still held while the guard's fields are
            // being dropped (field drop happens after `Drop::drop`), so no
            // other thread can race the restore.
            match previous {
                Some(value) => unsafe { std::env::set_var(&key, value) },
                None => unsafe { std::env::remove_var(&key) },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_allow_sequential_acquire_and_release_when_lock_is_dropped() {
        {
            let _guard = lock();
        }
        let _guard2 = lock();
    }

    // One key per test: sharing a key would let the two tests race each
    // other's value through the (per-test) lock scopes.
    const RESTORE_TEST_KEY: &str = "MIRRI_TEST_SUPPORT_ENV_GUARD_RESTORE_TEST";
    const UNSET_TEST_KEY: &str = "MIRRI_TEST_SUPPORT_ENV_GUARD_UNSET_TEST";

    /// Drop everything `guard` would restore on release, so its mutation
    /// persists after the guard goes out of scope — the fixture's
    /// setup/teardown half, expressed through the guard API instead of a
    /// bare env write (which the test-hygiene gate bans).
    fn commit(guard: &mut EnvGuard) {
        guard.saved.clear();
    }

    #[test]
    fn should_restore_previous_value_when_guard_drops() {
        // Given the key holds its initial value: the setup guard installs
        // it and commits the write, so the value persists into the
        // guard-under-test scope below
        {
            let mut setup = EnvGuard::new();
            setup.set(RESTORE_TEST_KEY, "initial");
            commit(&mut setup);
        }
        {
            let mut guard = EnvGuard::new();
            guard.set(RESTORE_TEST_KEY, "overridden");
            assert_eq!(std::env::var(RESTORE_TEST_KEY).as_deref(), Ok("overridden"));
        }
        // Then the guard above restored the `initial` value it recorded —
        // observed under the lock so a concurrent writer cannot race the read
        {
            let _lock = lock();
            assert_eq!(std::env::var(RESTORE_TEST_KEY).as_deref(), Ok("initial"));
        }
        // Teardown removes the fixture key for good (the recorded previous
        // value is committed away, like the setup above)
        {
            let mut teardown = EnvGuard::new();
            teardown.remove(RESTORE_TEST_KEY);
            commit(&mut teardown);
        }
    }

    #[test]
    fn should_preserve_unset_state_when_guard_drops() {
        {
            let mut guard = EnvGuard::new();
            guard.set(UNSET_TEST_KEY, "temporarily-set");
            assert_eq!(std::env::var(UNSET_TEST_KEY).as_deref(), Ok("temporarily-set"));
        }
        // Re-take the lock before observing the drop's effect: the guard
        // released it on drop, and another test in this binary may set the
        // same key the instant it becomes free — reading the variable outside
        // the lock would then race that writer and report a spurious
        // "still set".
        let _lock = lock();
        assert!(std::env::var(UNSET_TEST_KEY).is_err());
    }

    #[test]
    fn should_hold_workspace_lock_while_alive() {
        let _guard = EnvGuard::new();
        // The workspace mutex is held by the guard, so acquiring it again
        // must fail while the guard is alive.
        assert!(
            try_lock().is_none(),
            "guard alive but workspace env lock is free"
        );
    }

    #[tokio::test]
    async fn should_exclude_sync_lock_while_async_guard_is_held_across_await() {
        let async_guard = async_lock().await;
        // The async guard holds the std env mutex, so a sync `try_lock`
        // from this task must observe exclusion.
        assert!(
            try_lock().is_none(),
            "async guard alive but std env lock is free"
        );
        // Holding across an await point is the sanctioned use of async_lock.
        tokio::task::yield_now().await;
        drop(async_guard);
        // After release the sync lock is available again.
        assert!(try_lock().is_some(), "async guard dropped but env lock still held");
    }
}
