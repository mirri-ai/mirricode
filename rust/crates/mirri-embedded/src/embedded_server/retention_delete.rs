//! The unified delete executor (architecture §2.2, ADR-002, plan Task 9) —
//! the single body every session-deletion entry point converges on
//! (SDR-004): the management-page single delete, the tier batch, the
//! workspace purge, and the ACP `session/delete` path.
//!
//! The storage order is the persistence domain's existing one:
//!
//! 1. **Pre-lock existence probe**: an absent directory is the idempotent
//!    `AlreadyGone` (any stale index row is dropped) — the probe must run
//!    BEFORE the lock, because acquiring materializes the `lock` file and
//!    with it the directory, resurrecting a tombstone on a re-delete.
//! 2. `try_acquire` the session directory's advisory lock (ADR-002 —
//!    compression/restore/delete all hold it). A held lock is a business
//!    error ("session is busy"), never a silent skip — the user asked.
//! 3. In-lock re-validation: the session still exists on disk (its
//!    `state.json` or at least the directory — a torn half-delete
//!    re-enters here and converges).
//! 4. Attachment decrement FIRST (the blob references must be read while
//!    the streams still exist): `BlobLedger.decrement(session_dir)`
//!    takes the session's references off the ledger and returns the
//!    newly-zeroed hashes — the anchors the periodic recheck later
//!    judges.
//! 5. **Directory** (`SessionPersistence::delete_session_files`), then
//!    the **index row** (`SessionIndex::delete_session`): a crash
//!    between the two leaves an orphan row the existing startup orphan
//!    sweep already converges (§7's "删除：目录已删、索引行残留" row —
//!    zero new mechanism). A directory deletion that fails midway
//!    propagates without touching the index row — the next attempt
//!    clears the tail (幂等收敛).
//! 6. The report: `reclaimed_bytes` = the session directory's on-disk
//!    bytes ONLY (m4: the number's口径 is the directory tree, not an
//!    attachment estimate — the aged attachments reclaim later through
//!    the recheck and are never part of this figure). The newly-zeroed
//!    hashes carry their `zeroed_since` anchors for the periodic
//!    recheck (the retention task's tail segment); nothing is reclaimed
//!    within this same call.
//!
//! ## Cross-process double delete
//!
//! Two processes deleting the same session serialize on the directory
//! lock: the second, in-lock, finds no directory — the idempotent
//! success (`AlreadyGone`), never an error.
//!
//! ## The blob ledger instance
//!
//! The ledger is a process-wide cache shared between this executor and
//! the periodic recheck (the `EmbeddedServer::blob_ledger` field; both
//! hold the same `Arc`). It is built lazily on first use — a deployment
//! that never deletes and has retention disabled never pays the build.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use mirri_core::blob_ledger::BlobLedger;
use mirri_core::{SessionId, SessionPersistence};
use mirri_persistence::DirLockGuard;
use mirri_session_index::SessionIndex;

use super::{EmbeddedServer, EngineError};

/// What one unified delete did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DeleteOutcome {
    /// The session was deleted this call; `reclaimed_bytes` counts the
    /// session directory's on-disk bytes plus the immediately-reclaimed
    /// (aged-past) attachment bytes.
    Deleted { reclaimed_bytes: u64 },
    /// The session was already gone (directory and index row both) —
    /// the idempotent success.
    AlreadyGone,
}

impl EmbeddedServer {
    /// The unified delete executor (§2.2): lock → in-lock re-validation →
    /// attachment decrement → directory → index row → aged tail
    /// reclamation.
    ///
    /// `persistence` is the OWNING workspace's (W3: the bucket resolution
    /// must match the rest of the lifecycle); `index` the shared session
    /// index (`None` on a no-home engine — the disk deletion still runs,
    /// there is no row to remove).
    ///
    /// The blob-ledger decrement happens under the directory lock, BEFORE
    /// the directory removal: [`BlobLedger::decrement`] re-extracts the
    /// session's references from its record streams, which must still be
    /// on disk. A missing ledger row set (an unbuilt ledger, a foreign
    /// process's session) degrades to the zero-anchored "unclaimed"
    /// path the ledger itself prescribes — never a blocker.
    pub(super) async fn delete_session_with_reclaim(
        &self,
        persistence: &SessionPersistence,
        index: Option<&Arc<SessionIndex>>,
        session_id: &SessionId,
    ) -> Result<DeleteOutcome, EngineError> {
        let dir = persistence.session_dir(session_id);

        // 0. The pre-lock existence probe: an already-deleted session must
        //    NOT acquire the lock (acquiring would re-create the `lock`
        //    file and with it the directory — the idempotent re-delete
        //    would resurrect a tombstone). The directory check here races
        //    nothing that matters: a delete that runs concurrently holds
        //    the lock, and this probe failing to see it only delays the
        //    re-delete's AlreadyGone by one call.
        if !dir.is_dir() {
            // Nothing on disk: the idempotent path — drop any stale index
            // row (the §7 orphan case) and report AlreadyGone.
            if let Some(index) = index {
                let _ = index.delete_session(session_id.as_str());
            }
            return Ok(DeleteOutcome::AlreadyGone);
        }

        // 1. The directory lock (ADR-002): compression / restore / delete
        //    serialize on it. A held lock is a business error — the user
        //    asked for this deletion, "someone else is acting" surfaces.
        let Some(_guard) = DirLockGuard::try_acquire(&dir) else {
            return Err(super::invalid_params(format!(
                "session {session_id} is busy: another action (compression / restore / \
                 delete) holds its directory lock; retry shortly"
            )));
        };

        // 2. In-lock re-validation: the session must still exist on disk
        //    (a readable state.json, or at least a non-empty directory —
        //    a torn half-delete also re-enters here and converges).
        let state_exists = dir.join("state.json").is_file();
        if !state_exists && !dir.is_dir() {
            if let Some(index) = index {
                let _ = index.delete_session(session_id.as_str());
            }
            return Ok(DeleteOutcome::AlreadyGone);
        }

        // 3. Attachment extraction while the streams still exist (SR5):
        //    the session's references come off the shared ledger; the
        //    newly-zeroed hashes carry their anchor from now.
        let newly_zeroed = match self.shared_blob_ledger().await {
            Some(ledger) => ledger.decrement(&dir).map_err(|error| {
                EngineError::Internal(format!(
                    "extracting the blob references of session {session_id} failed: {error}"
                ))
            })?,
            None => Default::default(),
        };

        let reclaimed_dir_bytes = dir_size(&dir).unwrap_or(0);
        persistence
            .delete_session_files(session_id)
            .await
            .map_err(super::session_error)?;
        // 5. Index row second (the orphan sweep covers the crash gap).
        if let Some(index) = index {
            index.delete_session(session_id.as_str()).map_err(|error| {
                EngineError::Internal(format!(
                    "deleting the session index row for {session_id} failed: {error}"
                ))
            })?;
        }

        // 6. The report (SR5, m4): a fresh zeroing stamps `zeroed_since =
        //    now` — the aged reclaim rides the periodic recheck (the
        //    retention task's tail segment, which re-verifies every due
        //    hash against the disk before deleting its file). The
        //    reported bytes are the session directory's ONLY — the
        //    attachments that will later reclaim are not part of this
        //    number.
        if !newly_zeroed.is_empty() {
            tracing::info!(
                session_id = %session_id,
                action = "delete",
                zeroed_attachments = newly_zeroed.len(),
                bytes = reclaimed_dir_bytes,
                "retention delete removed the session; its exclusive attachments enter \
                 the aging window (the periodic recheck reclaims them)"
            );
        } else {
            tracing::info!(
                session_id = %session_id,
                action = "delete",
                bytes = reclaimed_dir_bytes,
                "retention delete removed the session (no exclusive attachments zeroed)"
            );
        }

        Ok(DeleteOutcome::Deleted {
            reclaimed_bytes: reclaimed_dir_bytes,
        })
    }

    /// The shared blob-ledger handle (the process-wide cache the delete
    /// executor and the periodic recheck both address — SDR-004's "same
    /// instance" ruling).
    ///
    /// Built lazily over the sessions root and the home's `blobs` dir; a
    /// build failure (an unreadable stream — the loud-failure stance)
    /// degrades to `None` and the delete proceeds without the tail
    /// reclamation (the attachments stay; the allowed side of the
    /// one-way boundary).
    pub(super) async fn shared_blob_ledger(&self) -> Option<Arc<BlobLedger>> {
        let mut slot = self.blob_ledger.lock().await;
        if let Some(ledger) = slot.as_ref() {
            return Some(Arc::clone(ledger));
        }
        let sessions_root = self.manager.persistence().sessions_dir().to_path_buf();
        let blobs_dir = self
            .home_dir()
            .map(|home| home.join("blobs"))
            .unwrap_or_else(|| {
                sessions_root
                    .parent()
                    .map(PathBuf::from)
                    .unwrap_or_else(|| sessions_root.clone())
                    .join("blobs")
            });
        let built =
            tokio::task::spawn_blocking(move || BlobLedger::build(&blobs_dir, &[sessions_root]))
                .await
                .ok()?
                .ok()?;
        let ledger = Arc::new(built);
        *slot = Some(Arc::clone(&ledger));
        Some(ledger)
    }

    /// The workspace-purge entry into the same tail reclamation (SDR-004):
    /// a purge that removes the bucket directory wholesale cannot go
    /// through the per-session executor (no per-session lock/revalidation
    /// applies to a bucket-level `remove_dir_all`), so the purge path
    /// decrements each session directory of the bucket BEFORE the
    /// wholesale removal — the references must be extracted while the
    /// streams still exist.
    pub(super) async fn blob_ledger_decrement_dir(&self, session_dir: &Path) {
        if let Some(ledger) = self.shared_blob_ledger().await
            && let Err(error) = ledger.decrement(session_dir)
        {
            tracing::warn!(
                directory = %session_dir.display(),
                %error,
                "purge blob-ledger decrement failed; the attachments stay (the \
                 one-way boundary's allowed side)"
            );
        }
    }
}

/// The byte size of a directory's whole file tree (regular files only,
/// symlinks not followed) — the occupancy basis the delete report and
/// the management-page listing share (§4: "会话目录内全部文件").
pub(super) fn dir_size(dir: &Path) -> std::io::Result<u64> {
    let mut total: u64 = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_symlink() {
                continue; // a symlink's target is not the session's bytes
            }
            if file_type.is_dir() {
                stack.push(entry.path());
            } else {
                total += entry.metadata()?.len();
            }
        }
    }
    Ok(total)
}
