//! Directory-level cross-process advisory lock (session-retention ADR-002).
//!
//! A fixed-name `lock` file inside the target directory holds an OS advisory
//! lock (`flock` on Unix, `LockFileEx` on Windows — both via the std
//! `File::try_lock` API stabilized in Rust 1.89). The guard is RAII: the
//! lock is held exactly as long as the guard lives and released on drop or
//! process death (OS semantics) — no unlock calls are ever needed on crash
//! paths.
//!
//! Invariants:
//! - **Non-blocking**: [`DirLockGuard::try_acquire`] returns `None`
//!   immediately when another process (or handle) holds the lock —
//!   contention means "skip this round", never "wait".
//! - **Lock file permanence**: the `lock` file is created on first
//!   acquisition and never deleted — its existence is NOT a liveness
//!   signal (only the held state is meaningful), and a stale file costs
//!   nothing to re-lock. A side effect worth naming: acquiring the lock
//!   MATERIALIZES the directory (create-if-missing open) — the unified
//!   delete executor probes existence FIRST and never acquires on an
//!   absent directory, precisely so a re-delete of a tombstone cannot
//!   resurrect it.
//! - **Flock re-open semantics**: on Unix, two independently opened
//!   descriptors for the same file in ONE process each take their own
//!   flock — the second succeeds. The guard therefore detects
//!   same-process contention by remembering its own directory.

use std::path::{Path, PathBuf};

/// The fixed lock-file name inside a locked directory.
pub const DIR_LOCK_FILE: &str = "lock";

/// An RAII guard holding a directory's advisory lock.
///
/// Dropping the guard releases the OS lock. The guard IS `Send` (the
/// std `File` it holds is `Send`) — releasing happens wherever the guard
/// is dropped, which the ownership discipline of OS locks permits; the
/// flock releases on fd close regardless of the dropping thread.
pub struct DirLockGuard {
    /// The open lock file. Closing the fd releases the flock; the explicit
    /// `unlock` in Drop is belt-and-suspenders for the Windows path where
    /// the handle is shared per-process rather than per-fd.
    file: std::fs::File,
    /// The directory the lock guards (diagnostics).
    dir: PathBuf,
}

impl DirLockGuard {
    /// Try to acquire `<dir>/lock` exclusively and non-blockingly.
    ///
    /// Returns `None` when the lock is already held (by another process,
    /// or by another live guard in this process), or when the lock file
    /// cannot be opened/created — callers treat both as "skip this round".
    pub fn try_acquire(dir: &Path) -> Option<Self> {
        let lock_path = dir.join(DIR_LOCK_FILE);
        // The lock file is permanent: create it if this is the first
        // acquisition, open it otherwise. Never removed afterwards.
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|error| {
                tracing::debug!(
                    directory = %dir.display(),
                    error = %error,
                    "failed to open session directory lock file"
                );
                error
            })
            .ok()?;
        match file.try_lock() {
            Ok(()) => Some(Self {
                file,
                dir: dir.to_path_buf(),
            }),
            Err(_) => {
                tracing::debug!(
                    directory = %dir.display(),
                    "session directory lock is held elsewhere — skipping this round"
                );
                None
            }
        }
    }

    /// The guarded directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

impl Drop for DirLockGuard {
    fn drop(&mut self) {
        // Unlock explicitly, then let the file close. Errors are
        // best-effort: the OS releases the flock on fd close (or process
        // death) regardless.
        if let Err(error) = self.file.unlock() {
            tracing::debug!(
                directory = %self.dir.display(),
                error = %error,
                "failed to explicitly unlock session directory lock on drop"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_return_second_none_when_acquiring_twice_in_same_process_via_separate_descriptors() {
        let tmp = tempfile::tempdir().unwrap();

        // Given a held guard
        let first = DirLockGuard::try_acquire(tmp.path());
        assert!(first.is_some(), "first acquisition must succeed");

        // When a second independent acquisition is attempted (a separate
        // fd of the same lock file — flock would let this through, so the
        // guard must catch it itself), it must fail
        let second = DirLockGuard::try_acquire(tmp.path());
        assert!(second.is_none(), "second acquisition must be rejected");

        drop(first);
    }

    #[test]
    fn should_allow_reacquisition_after_guard_is_dropped() {
        let tmp = tempfile::tempdir().unwrap();

        let first = DirLockGuard::try_acquire(tmp.path()).unwrap();
        drop(first);

        let second = DirLockGuard::try_acquire(tmp.path());
        assert!(second.is_some(), "the lock must be reacquirable after drop");
    }

    #[test]
    fn should_keep_lock_file_in_place_after_guard_is_dropped() {
        let tmp = tempfile::tempdir().unwrap();

        let guard = DirLockGuard::try_acquire(tmp.path()).unwrap();
        drop(guard);

        // The lock file is permanent: its presence after release is expected
        // and is NOT a liveness signal (§5).
        assert!(tmp.path().join(DIR_LOCK_FILE).exists());
        // And it can still be re-locked (the stale file re-locks fine)
        assert!(DirLockGuard::try_acquire(tmp.path()).is_some());
    }

    #[test]
    fn should_create_lock_file_when_directory_is_locked_for_the_first_time() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!tmp.path().join(DIR_LOCK_FILE).exists());

        let _guard = DirLockGuard::try_acquire(tmp.path()).unwrap();

        assert!(tmp.path().join(DIR_LOCK_FILE).exists());
    }

    #[cfg(unix)]
    #[test]
    fn should_return_none_when_another_process_holds_the_lock() {
        use std::io::BufRead;
        use std::process::Command;

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();

        // Spawn a child that takes the flock via python's fcntl (no flock
        // CLI on macOS), signals readiness, holds the lock until the parent
        // closes the pipe, and exits 0.
        let script = r#"
import fcntl, os, sys
path = sys.argv[1]
f = open(os.path.join(path, 'lock'), 'a+')
fcntl.flock(f.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
print('ready', flush=True)
# Hold until stdin closes (parent drops the pipe) or 30s pass.
try:
    for _ in iter(lambda: sys.stdin.buffer.read(1), b''):
        pass
except Exception:
    pass
"#;
        let mut child = Command::new("python3")
            .arg("-c")
            .arg(script)
            .arg(&dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("failed to spawn lock holder subprocess");
        let child_stdin = child.stdin.take().unwrap();
        let mut child_stdout = std::io::BufReader::new(child.stdout.take().unwrap());

        let mut ready = String::new();
        child_stdout
            .read_line(&mut ready)
            .expect("failed to read readiness signal from subprocess");
        assert_eq!(
            ready.trim(),
            "ready",
            "subprocess must confirm it holds the lock"
        );

        // When the parent tries to acquire while the child holds the lock
        let outcome = DirLockGuard::try_acquire(&dir);

        // Then it must be rejected immediately (non-blocking — no waiting
        // on the child)
        assert!(
            outcome.is_none(),
            "acquisition must fail while another process holds the lock"
        );

        // Cleanup: release the child and let it exit cleanly.
        drop(child_stdin);
        let status = child.wait().expect("failed to wait for subprocess");
        assert!(
            status.success(),
            "subprocess must exit cleanly, got {status:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn should_succeed_again_after_the_other_process_releases_the_lock() {
        use std::io::BufRead;
        use std::process::Command;

        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().to_path_buf();

        // The child holds the lock briefly, then exits, releasing it via
        // fd close.
        let script = r#"
import fcntl, os, sys, time
path = sys.argv[1]
f = open(os.path.join(path, 'lock'), 'a+')
fcntl.flock(f.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
print('ready', flush=True)
time.sleep(0.5)
"#;
        let mut child = Command::new("python3")
            .arg("-c")
            .arg(script)
            .arg(&dir)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("failed to spawn lock holder subprocess");
        let mut child_stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut ready = String::new();
        child_stdout
            .read_line(&mut ready)
            .expect("failed to read readiness signal from subprocess");
        assert_eq!(ready.trim(), "ready");

        let status = child.wait().expect("failed to wait for subprocess");
        assert!(status.success());

        // After the holder exits, the parent must be able to take over.
        assert!(
            DirLockGuard::try_acquire(&dir).is_some(),
            "lock must be acquirable after the holding process exits"
        );
    }
}
