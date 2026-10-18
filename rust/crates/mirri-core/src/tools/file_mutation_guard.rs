//! Process-wide per-path serialization for file mutations (Edit / Write).
//!
//! Two guarantees for read-modify-write file tools running concurrently in
//! one engine process (parallel agents, swarms, multi-session servers):
//!
//! 1. **Mutual exclusion**: `acquire` returns a guard that serializes every
//!    mutation of the same path, so two edits can never interleave their
//!    read/write pairs (the classic silent lost-update).
//! 2. **External-change detection**: `verify_unchanged` re-reads the file
//!    right before the write and compares the raw bytes with what the caller
//!    originally read, turning out-of-process modifications (editor, git,
//!    second CLI instance) into an explicit conflict error instead of a
//!    silent clobber.
//!
//! Purely in-memory (zero-IO): the lock table never touches the filesystem;
//! the verification re-read goes through the injected `DynFileSystem`.
//!
//! Known residual gaps, accepted by design:
//! - TOCTOU window between `verify_unchanged` and the subsequent write
//!   (inherent to optimistic checking; nanoseconds wide).
//! - Path aliasing: the key is the post-validation path, so the same file
//!   reached via different spellings or symlinks gets different locks.
//! - Lock-table entries are never evicted (one small allocation per
//!   distinct edited path per process).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use mirri_os::DynFileSystem;

/// How long a mutation waits for a concurrent same-path mutation before
/// failing. Generous on purpose: edits are fast, and a spurious timeout
/// fails a tool call that would otherwise succeed.
pub const FILE_MUTATION_LOCK_TIMEOUT: Duration = Duration::from_secs(30);

static LOCK_TABLE: LazyLock<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Handle proving the caller owns the per-path mutation lock.
#[derive(Debug)]
pub struct FileMutationGuard {
    // Held for the guard's lifetime; dropped (releasing the lock) when the
    // mutation finishes, errors, or the future is cancelled.
    _lock: tokio::sync::OwnedMutexGuard<()>,
}

impl FileMutationGuard {
    /// Acquire the process-wide mutation lock for `path` (default timeout).
    pub async fn acquire(path: &Path) -> Result<Self, String> {
        Self::acquire_with_timeout(path, FILE_MUTATION_LOCK_TIMEOUT).await
    }

    /// Acquire the mutation lock, failing after `timeout` of waiting.
    pub async fn acquire_with_timeout(path: &Path, timeout: Duration) -> Result<Self, String> {
        let lock = {
            let mut table = LOCK_TABLE.lock().unwrap_or_else(|e| e.into_inner());
            Arc::clone(table.entry(path.to_path_buf()).or_default())
        };
        match tokio::time::timeout(timeout, lock.lock_owned()).await {
            Ok(guard) => Ok(Self { _lock: guard }),
            Err(_) => Err(format!(
                "timed out after {timeout:?} waiting for a concurrent edit of '{}' to \
                 finish; re-read the file and retry once the other editor is done",
                path.display()
            )),
        }
    }
}

/// Verify the file on disk still matches the raw content the caller read
/// before mutating it. Returns `Err` with a conflict message when an
/// external writer changed (or removed) the file in the meantime.
pub async fn verify_unchanged(
    fs: &dyn DynFileSystem,
    path: &Path,
    expected_raw: &str,
) -> Result<(), String> {
    let current = match fs.read_text(path).await {
        Ok(text) => text,
        Err(error) => {
            return Err(format!(
                "'{}' changed while it was being edited (re-read failed: {error}); \
                 please re-read the file and retry",
                path.display()
            ));
        }
    };
    if current != expected_raw {
        return Err(format!(
            "'{}' was modified by another writer while it was being edited; \
             the file contents may be out of date. Please re-read the file and retry",
            path.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use mirri_os::DynFileSystem;

    use super::{FileMutationGuard, LOCK_TABLE, verify_unchanged};

    fn local_fs() -> Arc<dyn DynFileSystem> {
        Arc::new(mirri_os::LocalFs::new())
    }

    /// Given a path with no other holder, When acquiring the mutation lock,
    /// Then it is granted immediately.
    #[tokio::test]
    async fn should_acquire_immediately_when_no_other_holder_exists_for_the_path() {
        let path = std::env::temp_dir().join("mirri-guard-test-free.txt");
        let guard = FileMutationGuard::acquire(&path).await;
        assert!(guard.is_ok(), "uncontended acquire must succeed");
    }

    /// Given another holder keeps the path's lock, When acquiring with a
    /// short timeout, Then the acquire fails with a timeout message.
    #[tokio::test]
    async fn should_error_when_acquire_times_out_while_another_holder_keeps_the_lock() {
        let path = std::env::temp_dir().join("mirri-guard-test-held.txt");
        let lock = {
            let mut table = LOCK_TABLE.lock().unwrap_or_else(|e| e.into_inner());
            Arc::clone(table.entry(path.clone()).or_default())
        };
        let _held = lock.lock_owned().await;
        let result =
            FileMutationGuard::acquire_with_timeout(&path, Duration::from_millis(10)).await;
        let message = result.expect_err("must time out while the lock is held");
        assert!(message.contains("timed out"), "message was: {message}");
    }

    /// Given the file content changed since the caller read it, When
    /// verifying before write, Then the verification reports a conflict.
    #[tokio::test]
    async fn should_report_conflict_when_file_content_changed_between_read_and_verify() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("changed.txt");
        std::fs::write(&path, "original\n").unwrap();
        let fs = local_fs();
        assert!(
            verify_unchanged(fs.as_ref(), &path, "tampered\n")
                .await
                .is_err(),
            "changed content must be a conflict"
        );
        assert!(
            verify_unchanged(fs.as_ref(), &path, "original\n")
                .await
                .is_ok(),
            "identical content must pass"
        );
    }

    /// Given the file was removed since the caller read it, When verifying
    /// before write, Then the verification reports a conflict (not a crash).
    #[tokio::test]
    async fn should_report_conflict_when_file_was_removed_between_read_and_verify() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("gone.txt");
        std::fs::write(&path, "original\n").unwrap();
        std::fs::remove_file(&path).unwrap();
        let fs = local_fs();
        let message = verify_unchanged(fs.as_ref(), &path, "original\n")
            .await
            .expect_err("a removed file must be a conflict");
        assert!(message.contains("changed"), "message was: {message}");
    }
}
