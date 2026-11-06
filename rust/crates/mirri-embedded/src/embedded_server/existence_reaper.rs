//! The existence reaper (ADR-006 §3.6 / Task 10).
//!
//! The tombstone-based recycler: rows whose sessions were deleted carry a
//! tombstone (the row deletion and the tombstone share one transaction —
//! `SessionIndexPort::delete`). The reaper scans the tombstone set, removes
//! the on-disk directories it still finds, and clears the tombstones it
//! harvested. A directory with NO tombstone is never touched (裁决 3:
//! tomb-less ownerless directories belong to the user).
//!
//! Failure posture: every failure logs and leaves the tombstone for the
//! next run — the reaper is best-effort and never escalates.

use std::collections::HashSet;
use std::path::Path;

use mirri_session_index::SessionIndex;

/// One reaping sweep over the tombstone set. Returns the number of
/// directories actually removed (tombstones whose directories were already
/// gone are simply cleared — nothing to reap).
pub(super) fn reap_tombstoned_directories(sessions_root: &Path, index: &SessionIndex) -> usize {
    let tombstones: HashSet<String> = match index.tombstone_ids() {
        Ok(ids) => ids,
        Err(error) => {
            tracing::warn!(%error, "listing the tombstone set for the reaper failed");
            return 0;
        }
    };
    let mut reaped = 0usize;
    for id in tombstones {
        // A tombstone records the session id; its directory lives under
        // whichever bucket held it — a single id maps to at most one
        // bucket, so scan the buckets for it (the tombstone set is small:
        // one entry per deleted session, cleared as it reaps).
        let Some(bucket_dir) = find_directory_for(sessions_root, &id) else {
            // No directory anywhere — the tombstone is stale; clear it.
            if let Err(error) = index.clear_tombstone(&id) {
                tracing::warn!(session_id = %id, %error, "clearing a stale tombstone failed");
            }
            continue;
        };
        match std::fs::remove_dir_all(&bucket_dir) {
            Ok(()) => {
                reaped += 1;
                if let Err(error) = index.clear_tombstone(&id) {
                    tracing::warn!(
                        session_id = %id,
                        %error,
                        "the directory was reaped but its tombstone could not be cleared; \
                         the next sweep retries the clear (the directory is already gone)"
                    );
                }
            }
            Err(error) => {
                tracing::warn!(
                    session_id = %id,
                    %error,
                    "reaping a tombstoned session directory failed; the tombstone stays \
                     and the next sweep retries"
                );
            }
        }
    }
    reaped
}

/// Locate the bucket-relative directory of `session_id`, if any bucket
/// under `sessions_root` holds it.
fn find_directory_for(sessions_root: &Path, session_id: &str) -> Option<std::path::PathBuf> {
    let entries = std::fs::read_dir(sessions_root).ok()?;
    for entry in entries.flatten() {
        let candidate = entry.path().join(session_id);
        if candidate.is_dir() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use mirri_session_index::SessionRow;

    fn row(id: &str) -> SessionRow {
        let now = chrono::Utc::now();
        SessionRow {
            id: id.to_string(),
            workspace_id: "wd_test_000000000000".to_string(),
            cwd: None,
            title: Some("reaper test".to_string()),
            created_at: now,
            updated_at: now,
            archived: false,
            tags: Vec::new(),
            custom: None,
            last_prompt: None,
            state_mtime: None,
            archived_at: None,
            compressed: false,
            compressed_at: None,
            last_used_at: None,
            pre_compressed_size: None,
            pinned: false,
            pinned_at: None,
            pin_color: None,
        }
    }

    /// Given a tombstoned directory and an unmarked one, When the reaper
    /// runs, Then only the tombstoned directory is removed — ownerless
    /// directories are the user's to handle (裁决 3).
    #[test]
    fn should_reap_tombstoned_directories_and_never_touch_unmarked_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions_root = tmp.path().join("v3_sessions");
        let bucket = sessions_root.join("wd_test_000000000000");
        std::fs::create_dir_all(bucket.join("session_marked").join("tasks")).unwrap();
        std::fs::create_dir_all(bucket.join("session_plain").join("tasks")).unwrap();
        let db = tmp.path().join("session-index.db");
        let index = SessionIndex::open(&db).unwrap();
        let now = chrono::Utc::now();
        index.record_tombstones(&[("session_marked".to_string(), now)]).unwrap();

        let reaped = reap_tombstoned_directories(&sessions_root, &index);

        assert_eq!(reaped, 1, "only the tombstoned directory is reaped");
        assert!(
            !bucket.join("session_marked").exists(),
            "the tombstoned directory is gone"
        );
        assert!(
            bucket.join("session_plain").exists(),
            "an unmarked directory is never touched (裁决 3)"
        );
        assert!(
            !index.tombstone_ids().unwrap().contains("session_marked"),
            "the harvested tombstone is cleared"
        );
    }

    /// Given a tombstone whose directory is already gone, When the reaper
    /// runs, Then the stale tombstone is cleared and nothing else happens.
    #[test]
    fn should_clear_a_stale_tombstone_when_its_directory_is_already_gone() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions_root = tmp.path().join("v3_sessions");
        std::fs::create_dir_all(&sessions_root).unwrap();
        let index = SessionIndex::open(&tmp.path().join("session-index.db")).unwrap();
        let now = chrono::Utc::now();
        index.record_tombstones(&[("session_vanished".to_string(), now)]).unwrap();

        let reaped = reap_tombstoned_directories(&sessions_root, &index);

        assert_eq!(reaped, 0);
        assert!(
            !index.tombstone_ids().unwrap().contains("session_vanished"),
            "the stale tombstone is cleared"
        );
    }

    /// Given a session row that exists on disk with NO tombstone, When the
    /// reaper runs, Then the row and its directory both stay — the reaper
    /// never re-seeds deletions (the orphan pass is retired).
    #[test]
    fn should_keep_row_backed_directories_without_tombstones() {
        let tmp = tempfile::tempdir().unwrap();
        let sessions_root = tmp.path().join("v3_sessions");
        let bucket = sessions_root.join("wd_test_000000000000");
        std::fs::create_dir_all(bucket.join("session_live")).unwrap();
        let db = tmp.path().join("session-index.db");
        let index = SessionIndex::open(&db).unwrap();
        index.bulk_upsert_sessions(&[row("session_live")]).unwrap();

        let reaped = reap_tombstoned_directories(&sessions_root, &index);

        assert_eq!(reaped, 0);
        assert!(bucket.join("session_live").exists());
        assert!(index.get_session("session_live").is_some());
    }
}
