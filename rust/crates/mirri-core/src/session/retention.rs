//! Session-retention compression engine (architecture §2.1 / §6 / §7,
//! ADR-002 / ADR-004 / ADR-007) — the core orchestration the embedded
//! scanner drives.
//!
//! The engine is deliberately split in halves:
//!
//! - [`should_compress`] — the pure deadline verdict (no IO, no deletion
//!   branch: this domain only compresses; deletion is a separate manual
//!   path driven by RPC, §2.2).
//! - The action orchestration — [`compress_session`],
//!   [`backfill_archived_at`], [`restore_decompress`], and the ADR-007
//!   marker/shape self-heal [`heal_compression_state`]. Each action takes
//!   the **directory-level advisory lock** (ADR-002:
//!   `mirri_persistence::DirLockGuard::try_acquire`) and re-validates
//!   its precondition inside the lock (§2.1: the candidate check and the
//!   lock acquisition are separate steps; another process may restore or
//!   compress in between — the re-validation closes that window).
//!
//! Conventions:
//!
//! - The `state.json` `compressed` marker is a **cache** of the disk shape
//!   (ADR-007). The [`SessionPersistence::set_compressed`] /
//!   [`clear_compressed`] explicit lifecycle writes are the only writes
//!   that keep it fresh. A crash between the atomic wire-log replacement
//!   and the stamp leaves the marker stale — never falsely ahead — and
//!   [`heal_compression_state`] converges it on the next pass (§7).
//! - Persistence-primitive IO (gzip, lock) lives in `mirri-persistence`;
//!   the marker writes ride `SessionPersistence`. Neither route trips
//!   the core zero-IO gate.
//!
//! The embedded wiring (the periodic scanner, the restore dispatch's
//! call into [`restore_decompress`], the compress-now RPC) lands in
//! Tasks 8/9 — this module is the core-layer seam they call into.

use std::path::Path;

use chrono::{DateTime, Utc};

use mirri_persistence::compress::{compress_wire_log, decompress_wire_log};
use mirri_persistence::{CompressedShape, DirLockGuard, probe_compressed};

use super::error::{SessionError, SessionResult};
use super::persistence::SessionPersistence;
use crate::scope::SessionId;

/// What a [`compress_session`] invocation did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompressOutcome {
    /// The wire log was compressed: the `.gz` is on disk (the atomic
    /// replacement landed) and the compression metadata batch was
    /// stamped. `pre_size` is the pre-compression byte count (the
    /// "savings" basis, §3.1).
    Compressed {
        /// The wire log's byte size before compression.
        pre_size: u64,
    },
    /// Nothing was done — a benign, non-terminal skip (§5: "拿不到 = 本轮
    /// 跳过，跳过不是终局").
    Skipped(SkipReason),
}

/// Why a compression attempt skipped without acting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// The directory lock is held (another process is restoring /
    /// compressing / deleting this session) — this round's pass on it.
    Locked,
    /// The lock was acquired, but the in-lock re-validation found the
    /// precondition no longer holds (the session was restored or
    /// compressed between the candidate check and the lock).
    Revalidated,
}

impl CompressOutcome {
    /// Whether the session's wire log was actually compressed.
    pub fn was_compressed(&self) -> bool {
        matches!(self, Self::Compressed { .. })
    }
}

/// Whether an archived session's wire log has reached the compression
/// deadline (§2.1 判定 — the pure function; no deletion branch).
///
/// `None` (a live or legacy un-stamped session) never compresses; the
/// deadline is `archived_at + grace_days` and is **inclusive** — a session
/// archived exactly N days ago is due on the Nth day.
pub fn should_compress(archived_at: Option<&str>, now: DateTime<Utc>, grace_days: u32) -> bool {
    let Some(archived_at) = archived_at else {
        return false;
    };
    let Ok(archived_at) = DateTime::parse_from_rfc3339(archived_at) else {
        // An unparseable stamp cannot anchor a deadline — treat it as
        // absent (the scanner's backfill pass rewrites a fresh stamp on
        // the next round, §3.1).
        return false;
    };
    let deadline = archived_at + chrono::Duration::days(i64::from(grace_days));
    now >= deadline
}

/// One archived session's compression action (§2.1 执行 — lock, in-lock
/// re-validation, compress, stamp).
///
/// The full sequence:
///
/// 1. `try_acquire(dir)` — the ADR-002 advisory lock. `None` means
///    another process holds the session: [`CompressOutcome::Skipped`] with
///    [`SkipReason::Locked`] (benign; the next round retries, US4.2).
/// 2. **In-lock re-validation** — re-read `state.json`
///    ([`SessionPersistence::read_persisted_info`]): only a session
///    still `archived` AND still un-compressed (per the ADR-007 shape
///    truth) proceeds; anything else is
///    [`CompressOutcome::Skipped`] with [`SkipReason::Revalidated`].
/// 3. `compress_wire_log(dir)` — the persistence primitive (streaming
///    gzip, temp file + atomic replacement). Returns the pre-compression
///    byte size.
/// 4. `set_compressed(pre_size)` — the metadata stamp, written **after**
///    the atomic replacement landed (a crash in between leaves the
///    marker at worst stale, never falsely ahead — §7's heal covers it,
///    so a stamp-write failure is logged as a warning, not an error).
///
/// `dir` is the session directory and `session_id` its id — the pair the
/// manager-style callers (the embedded scanner, the compress-now RPC)
/// always hold together. The marker write routes through `persistence`
/// (the owning workspace's, W3) so the state-path resolution matches the
/// rest of the lifecycle.
pub async fn compress_session(
    persistence: &SessionPersistence,
    session_id: &SessionId,
    dir: &Path,
) -> SessionResult<CompressOutcome> {
    // Step 1 — the directory lock (ADR-002). A held lock is a skip, not
    // an error: the compression round is an optimization pass.
    let Some(_guard) = DirLockGuard::try_acquire(dir) else {
        tracing::debug!(
            session_id = %session_id,
            directory = %dir.display(),
            "retention compression skipped: session directory lock held elsewhere"
        );
        return Ok(CompressOutcome::Skipped(SkipReason::Locked));
    };

    // Step 2 — in-lock re-validation (§2.1: the candidate check ran
    // BEFORE the lock; restore/compress may have intervened).
    let Some(info) = persistence.read_persisted_info(session_id).await else {
        tracing::debug!(
            session_id = %session_id,
            directory = %dir.display(),
            "retention compression skipped: state.json unreadable at revalidation"
        );
        return Ok(CompressOutcome::Skipped(SkipReason::Revalidated));
    };
    if !info.metadata.archived {
        tracing::debug!(
            session_id = %session_id,
            "retention compression skipped: session was restored before the lock"
        );
        return Ok(CompressOutcome::Skipped(SkipReason::Revalidated));
    }
    // The ADR-007 shape verdict (shape first, marker only as the Missing
    // fallback) — a session already compressed (by this or another
    // process) must not be re-compressed: compress_wire_log fails on a
    // missing wire.jsonl source.
    if shape_or_marker_compressed(dir, &info) {
        tracing::debug!(
            session_id = %session_id,
            "retention compression skipped: session already compressed"
        );
        return Ok(CompressOutcome::Skipped(SkipReason::Revalidated));
    }

    // Step 3 — the persistence primitive. The atomic replacement is the
    // commit point of the compressed form; its failure propagates (an IO
    // failure is a real failure the round reports, not a skip).
    let pre_size = compress_wire_log(dir).map_err(|e| {
        SessionError::WireLog(format!(
            "failed to compress wire log for session {session_id} in {}: {e}",
            dir.display()
        ))
    })?;

    // Step 4 — the metadata stamp. The shape already transitioned, so a
    // stamp failure leaves the marker stale — the ADR-007 heal covers it
    // (§7: "压缩原子替换后、标记写入前" row); warn, don't fail.
    if let Err(error) = persistence.set_compressed(session_id, pre_size).await {
        tracing::warn!(
            session_id = %session_id,
            %error,
            "retention compression landed but the compressed marker stamp failed; \
             the shape-first heal covers it on the next pass"
        );
    }

    tracing::info!(
        session_id = %session_id,
        pre_size,
        gz = %dir.join(mirri_persistence::WIRE_LOG_GZ_FILE).display(),
        "retention compression completed"
    );
    Ok(CompressOutcome::Compressed { pre_size })
}

/// Backfill a missing `archivedAt` on a legacy archived session (SDR-008,
/// §3.1): a session archived before the field existed gets stamped with
/// the check moment — the earliest verifiable anchor the deadline can
/// start from.
///
/// Called by the scan flow **before** the [`should_compress`] verdict: a
/// backfilled session then judges by the fresh stamp (never compressing
/// earlier than one full grace period after the backfill). Idempotent —
/// a session that already has a stamp (live, restored, or backfilled) is
/// untouched.
///
/// Returns whether a stamp was backfilled.
pub async fn backfill_archived_at(
    persistence: &SessionPersistence,
    session_id: &SessionId,
) -> SessionResult<bool> {
    let Some(info) = persistence.read_persisted_info(session_id).await else {
        // No readable state.json — nothing to backfill onto; the caller's
        // candidate flow already required a readable archived flag.
        return Ok(false);
    };
    if !info.metadata.archived || info.archived_at.is_some() {
        return Ok(false);
    }
    // set_archived(true) stamps `archived_at = now` (its archive branch)
    // while load-mutate-saving every other field untouched — exactly the
    // backfill write.
    persistence.set_archived(session_id, true).await?;
    tracing::info!(
        session_id = %session_id,
        "backfilled missing archivedAt on a legacy archived session (SDR-008)"
    );
    Ok(true)
}

/// The restore-decompression half of the `session.restore` path (§6) —
/// the core-layer seam the embedded restore dispatch calls **before**
/// the resume assembly (ADR-004: the write path requires the plain
/// shape, so the decompression must precede any replay/append).
///
/// Sequence (each step's crash result is the still-compressed archive
/// state — idempotently re-restorable, §7):
///
/// 1. `try_acquire(dir)` — restore competes with compression and deletion
///    for the same advisory lock (§5). A held lock is an **error** here
///    (unlike the scanner's skip): the user asked for this session, so
///    "restore in progress elsewhere" surfaces instead of silently
///    no-op'ing.
/// 2. `probe_compressed(dir)` — the shape verdict. Not compressed →
///    `Ok(())` (idempotent no-op: a plain session, an already-restored
///    retry, or a never-conversed Missing shape all pass through).
/// 3. `decompress_wire_log(dir)` — restore `wire.jsonl.gz` back to
///    `wire.jsonl` (temp + atomic rename + `.gz` removal). No marker is
///    touched before this succeeds (§6: "还原完成前不清任何标记").
/// 4. `clear_compressed` + `set_archived(false)` — clear the compression
///    batch and the archive batch (the latter also clears `archived_at`
///    and — belt-and-braces — the compression batch again; the §3.1
///    restore semantics).
pub async fn restore_decompress(
    persistence: &SessionPersistence,
    session_id: &SessionId,
    dir: &Path,
) -> SessionResult<()> {
    let Some(_guard) = DirLockGuard::try_acquire(dir) else {
        // §5 压缩 vs 恢复方竞态: the restore side FAILS loudly when the
        // lock is held (compression or another restore in flight) — the
        // caller surfaces a retryable error, never a half state.
        return Err(SessionError::StateFile(format!(
            "session {session_id} is locked by another action \
             (compression/restore/delete in progress); retry the restore"
        )));
    };

    match probe_compressed(dir) {
        CompressedShape::Compressed => {
            // Restore the plain shape first; only a successful
            // decompression may touch any marker (§6 ordering).
            decompress_wire_log(dir).map_err(|e| {
                SessionError::WireLog(format!(
                    "failed to decompress wire log for session {session_id} in {}: {e}",
                    dir.display()
                ))
            })?;
        }
        // Plain: an already-restored retry or a never-compressed session.
        // Missing: a never-conversed (or externally-stripped) session —
        // the decompression is vacuous either way; the marker/archive
        // clears below still run (the restore lifecycle's flag write).
        CompressedShape::Plain | CompressedShape::Missing => {
            tracing::debug!(
                session_id = %session_id,
                shape = ?probe_compressed(dir),
                "restore_decompress: session not in the compressed shape — decompression vacuous"
            );
        }
    }

    // The shape is plain now. Clear the compression cache batch (a stale
    // `compressed = true` from a crash between decompression and this
    // write is the §7 "标记残留" row), then the archive batch.
    persistence.clear_compressed(session_id).await?;
    // set_archived(false) clears `archived_at` AND the whole compression
    // batch (§3.1's restore semantics — running it after
    // clear_compressed makes the pair idempotent in either repetition
    // order).
    persistence.set_archived(session_id, false).await?;
    Ok(())
}

/// Converge the `state.json` compression marker against the on-disk shape
/// (ADR-007: 形状优先并自愈补写 — the §7 crash-matrix rows "原子替换后、
/// 标记写入前" and "`.gz` 已删、标记未清").
///
/// - Shape `Compressed`, marker `false` → `set_compressed` with a
///   best-available `pre_size`: the recorded one when present, else the
///   **current `.gz` size** (a conservative, disk-derivable stand-in —
///   the true pre-compression size was lost with the removed plain file;
///   understating the savings basis is the honest degradation, noted
///   rather than invented).
/// - Shape `Plain`, marker `true` → `clear_compressed`.
/// - Shape `Missing` → the marker speaks (effective_compressed's
///   fallback); nothing to heal.
/// - Marker already agreeing with the shape → no-op.
///
/// Called by the compression engine's scan pass (each candidate, before
/// the verdict) and the restore path — the two flows the architecture
/// names as the heal hosts (§7: "所有收敛都是既有检查流程的一部分").
pub async fn heal_compression_state(
    persistence: &SessionPersistence,
    session_id: &SessionId,
    dir: &Path,
) -> SessionResult<()> {
    // The RAW marker read (the heal's own input — NOT the reconciled
    // `read_persisted_info`, whose M3 shape-first verdict would mask the
    // exact marker/shape disagreement this function exists to fix).
    let Some(info) = persistence.read_persisted_info_raw(session_id).await else {
        return Ok(()); // no readable state.json — nothing to reconcile
    };
    match probe_compressed(dir) {
        CompressedShape::Compressed => {
            if info.compressed {
                return Ok(()); // cache fresh — nothing to heal
            }
            // Backfill pre_size: the recorded value wins; without one the
            // current .gz size is the only disk-available conservative
            // stand-in (the true pre-compression size died with the
            // removed wire.jsonl — an invented larger number would
            // fabricate savings).
            let pre_size = info.pre_compressed_size.unwrap_or_else(|| {
                let gz = dir.join(mirri_persistence::WIRE_LOG_GZ_FILE);
                let gz_len = std::fs::metadata(&gz).map(|meta| meta.len()).unwrap_or(0); // zero-io-allow: retention heal — a stat of the persistence-owned .gz (same class as probe_compressed's is_file); no injected fs trait carries this sync site
                gz_len
            });
            persistence.set_compressed(session_id, pre_size).await?;
            tracing::info!(
                session_id = %session_id,
                pre_size,
                "healed stale compressed marker (shape compressed, marker false)"
            );
        }
        CompressedShape::Plain => {
            if !info.compressed {
                return Ok(());
            }
            persistence.clear_compressed(session_id).await?;
            tracing::info!(
                session_id = %session_id,
                "healed stale compressed marker (shape plain, marker true)"
            );
        }
        // Neither file: the marker is the only voice (ADR-007's
        // effective_compressed fallback) — no heal to make.
        CompressedShape::Missing => {}
    }
    Ok(())
}

/// The ADR-007 effective-compression verdict over a [`super::persistence`
/// `PersistedInfo`] — shape first, marker only for the Missing shape.
fn shape_or_marker_compressed(dir: &Path, info: &super::persistence::PersistedInfo) -> bool {
    match probe_compressed(dir) {
        CompressedShape::Compressed => true,
        CompressedShape::Plain => false,
        CompressedShape::Missing => info.compressed,
    }
}

/// Reject forking a compressed source (SDR-003 / §3.3 复制拒斥): the fork
/// pipeline's earliest entry gate, placed before the source wire log is
/// read. A compressed session must be restored (decompressed) first —
/// the error is a business error (the embedded layer maps it to
/// invalid_params, same as the fork-anchor guard), carrying the
/// user-actionable message the product surfaces (US2.3).
pub(crate) fn reject_compressed_fork_source(source_dir: &Path) -> SessionResult<()> {
    if probe_compressed(source_dir) == CompressedShape::Compressed {
        return Err(SessionError::ForkTargetNotFound(
            "compressed session must be restored before fork".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Days;
    use mirri_persistence::{AppendLogStore, JsonlAppendLog, WIRE_LOG_FILE, WIRE_LOG_GZ_FILE};
    use mirri_proto::WireOp;
    use serde_json::json;
    use std::path::PathBuf;

    use crate::scope::SessionMetadata;

    /// A wire-log line with enough repetitive padding that gzip shrinks it
    /// well below half (§3.3's "small fraction" gate).
    fn padded_record(index: usize) -> WireOp {
        WireOp::ContextAppendMessage {
            message: json!({
                "role": "user",
                "content": format!("repetitive record number {index} with padding"),
            }),
        }
    }

    /// A `SessionPersistence` over a temp dir (the persistence.rs test
    /// setup convention) plus the session id.
    struct Fixture {
        persistence: SessionPersistence,
        session_id: SessionId,
        dir: PathBuf,
        _tmp: tempfile::TempDir,
    }

    impl Fixture {
        async fn new(name: &str) -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let persistence = SessionPersistence::new(
                tmp.path().to_path_buf(),
                std::sync::Arc::new(mirri_os::LocalFs::new()),
            );
            let session_id = SessionId::new(name);
            let dir = persistence.session_dir(&session_id);
            tokio::fs::create_dir_all(&dir).await.unwrap();
            Self {
                persistence,
                session_id,
                dir,
                _tmp: tmp,
            }
        }

        /// Persist a plain wire log with `records` padded lines plus the
        /// metadata header, and a state.json (un-archived).
        async fn seed(self, records: usize) -> Self {
            let mut log = JsonlAppendLog::create(&self.dir.join(WIRE_LOG_FILE))
                .await
                .unwrap();
            log.append(&WireOp::Metadata {
                protocol_version: None,
                created_at: None,
            })
            .await
            .unwrap();
            for index in 0..records {
                log.append(&padded_record(index)).await.unwrap();
            }
            drop(log);
            self.persistence
                .save_state(&self.session_id, &SessionMetadata::default())
                .await
                .unwrap();
            self
        }

        async fn read_state(&self) -> serde_json::Value {
            let path = self.persistence.state_path(&self.session_id);
            let raw = tokio::fs::read_to_string(&path).await.unwrap();
            serde_json::from_str(&raw).expect("state.json parses")
        }

        /// The parsed `archivedAt` stamp, when present.
        async fn archived_at(&self) -> Option<DateTime<Utc>> {
            self.read_state()
                .await
                .get("archivedAt")
                .cloned()
                .and_then(|value| {
                    serde_json::from_value::<Option<DateTime<Utc>>>(value)
                        .ok()
                        .flatten()
                })
        }
    }

    fn rfc3339(days_ago: u32) -> String {
        (Utc::now() - Days::new(u64::from(days_ago)))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
    }

    // -------------------------------------------------------------------
    // should_compress — the pure deadline verdict
    // -------------------------------------------------------------------

    #[test]
    fn should_return_false_when_archived_at_is_missing() {
        assert!(!should_compress(None, Utc::now(), 30));
    }

    #[test]
    fn should_return_false_when_the_grace_period_has_one_day_left() {
        let archived = rfc3339(29);
        assert!(!should_compress(Some(&archived), Utc::now(), 30));
    }

    #[test]
    fn should_return_true_when_the_grace_period_is_exactly_reached() {
        let archived = rfc3339(30);
        assert!(should_compress(Some(&archived), Utc::now(), 30));
    }

    #[test]
    fn should_return_true_when_the_grace_period_is_exceeded() {
        let archived = rfc3339(90);
        assert!(should_compress(Some(&archived), Utc::now(), 30));
    }

    #[test]
    fn should_return_false_when_the_stamp_is_unparseable() {
        assert!(!should_compress(Some("not-a-timestamp"), Utc::now(), 30));
    }

    // -------------------------------------------------------------------
    // compress_session — orchestration
    // -------------------------------------------------------------------

    /// Given a due archived session, when compressed, then the .gz is in
    /// place, the marker batch is stamped, and pre_size is correct.
    #[tokio::test]
    async fn should_compress_the_wire_log_and_stamp_the_marker_when_the_session_is_due() {
        let fixture = Fixture::new("due-compress").await.seed(200).await;
        fixture
            .persistence
            .set_archived(&fixture.session_id, true)
            .await
            .unwrap();
        let original_len = tokio::fs::metadata(fixture.dir.join(WIRE_LOG_FILE))
            .await
            .unwrap()
            .len();

        let outcome = compress_session(&fixture.persistence, &fixture.session_id, &fixture.dir)
            .await
            .unwrap();

        assert_eq!(
            outcome,
            CompressOutcome::Compressed {
                pre_size: original_len
            }
        );
        assert!(!fixture.dir.join(WIRE_LOG_FILE).exists());
        assert!(fixture.dir.join(WIRE_LOG_GZ_FILE).exists());
        let state = fixture.read_state().await;
        assert!(state["compressed"].as_bool().unwrap());
        assert!(state["compressedAt"].is_string());
        assert_eq!(state["preCompressedSize"], json!(original_len));
        assert!(
            state["archived"].as_bool().unwrap(),
            "the archive flag itself must survive"
        );
    }

    /// Given a session whose directory lock is held elsewhere, when
    /// compression is attempted, then it skips with Locked and the wire
    /// log stays plain.
    #[tokio::test]
    async fn should_skip_with_locked_when_the_directory_lock_is_held() {
        let fixture = Fixture::new("locked-compress").await.seed(50).await;
        fixture
            .persistence
            .set_archived(&fixture.session_id, true)
            .await
            .unwrap();
        let _held = DirLockGuard::try_acquire(&fixture.dir).unwrap();

        let outcome = compress_session(&fixture.persistence, &fixture.session_id, &fixture.dir)
            .await
            .unwrap();

        assert_eq!(outcome, CompressOutcome::Skipped(SkipReason::Locked));
        assert!(fixture.dir.join(WIRE_LOG_FILE).exists());
        assert!(!fixture.dir.join(WIRE_LOG_GZ_FILE).exists());
    }

    /// Given the session was restored between the candidate check and the
    /// lock, when compression re-validates in-lock, then it skips with
    /// Revalidated.
    #[tokio::test]
    async fn should_skip_with_revalidated_when_the_session_was_restored_before_the_lock() {
        let fixture = Fixture::new("restored-compress").await.seed(50).await;
        // archived = false — the re-validation's failure case.
        fixture
            .persistence
            .save_state(&fixture.session_id, &SessionMetadata::default())
            .await
            .unwrap();

        let outcome = compress_session(&fixture.persistence, &fixture.session_id, &fixture.dir)
            .await
            .unwrap();

        assert_eq!(outcome, CompressOutcome::Skipped(SkipReason::Revalidated));
        assert!(fixture.dir.join(WIRE_LOG_FILE).exists());
    }

    /// Given the session was already compressed (by another process)
    /// before the lock, when compression re-validates in-lock, then it
    /// skips with Revalidated instead of re-compressing.
    #[tokio::test]
    async fn should_skip_with_revalidated_when_the_session_is_already_compressed() {
        let fixture = Fixture::new("already-compressed").await.seed(50).await;
        fixture
            .persistence
            .set_archived(&fixture.session_id, true)
            .await
            .unwrap();
        // Another process compressed the log but crashed before the stamp
        // (the §7 heal row): the SHAPE says compressed, the marker false.
        let original_len = tokio::fs::metadata(fixture.dir.join(WIRE_LOG_FILE))
            .await
            .unwrap()
            .len();
        compress_wire_log(&fixture.dir).unwrap();

        let outcome = compress_session(&fixture.persistence, &fixture.session_id, &fixture.dir)
            .await
            .unwrap();

        assert_eq!(outcome, CompressOutcome::Skipped(SkipReason::Revalidated));
        let gz_len = tokio::fs::metadata(fixture.dir.join(WIRE_LOG_GZ_FILE))
            .await
            .unwrap()
            .len();
        // The .gz was not re-compressed (its content stands) — and the
        // size sanity anchors the non-action.
        assert!(gz_len < original_len);
    }

    // -------------------------------------------------------------------
    // backfill_archived_at — SDR-008
    // -------------------------------------------------------------------

    /// Given a legacy archived session without an archivedAt, when the
    /// backfill runs, then the stamp is written and a subsequent verdict
    /// judges by the fresh stamp (not due immediately).
    #[tokio::test]
    async fn should_backfill_archived_at_when_a_legacy_archived_session_lacks_the_stamp() {
        let fixture = Fixture::new("backfill").await.seed(20).await;
        fixture
            .persistence
            .set_archived(&fixture.session_id, true)
            .await
            .unwrap();
        // Simulate the legacy state: archived = true, archivedAt absent
        // (strip the field by rewriting the raw document).
        let state_path = fixture.persistence.state_path(&fixture.session_id);
        let raw: serde_json::Value =
            serde_json::from_str(&tokio::fs::read_to_string(&state_path).await.unwrap()).unwrap();
        let mut legacy = raw.clone();
        legacy.as_object_mut().unwrap().remove("archivedAt");
        tokio::fs::write(&state_path, serde_json::to_string_pretty(&legacy).unwrap())
            .await
            .unwrap();

        // When the backfill runs
        let backfilled = backfill_archived_at(&fixture.persistence, &fixture.session_id)
            .await
            .unwrap();

        // Then the stamp is written
        assert!(backfilled);
        let stamped = fixture.archived_at().await.expect("archivedAt backfilled");
        // And the stamp is fresh (the check moment, not a fabricated past)
        assert!(Utc::now().signed_duration_since(stamped).num_minutes() < 5);
        // And the verdict by the fresh stamp: not due immediately
        let stamp = stamped.to_rfc3339_opts(chrono::SecondsFormat::Secs, false);
        assert!(!should_compress(Some(&stamp), Utc::now(), 30));

        // And a second backfill is a no-op (idempotent)
        assert!(
            !backfill_archived_at(&fixture.persistence, &fixture.session_id)
                .await
                .unwrap()
        );
    }

    /// Given a live (un-archived) session, when the backfill runs, then
    /// nothing is stamped.
    #[tokio::test]
    async fn should_not_backfill_when_the_session_is_not_archived() {
        let fixture = Fixture::new("backfill-live").await.seed(20).await;

        assert!(
            !backfill_archived_at(&fixture.persistence, &fixture.session_id)
                .await
                .unwrap()
        );
        assert!(fixture.archived_at().await.is_none());
    }

    // -------------------------------------------------------------------
    // restore_decompress
    // -------------------------------------------------------------------

    /// Given a compressed archived session, when restore-decompressed,
    /// then the plain log returns byte-identical, the .gz is gone, and
    /// all four retention fields clear.
    #[tokio::test]
    async fn should_restore_the_plain_log_and_clear_all_retention_fields_when_decompressing() {
        let fixture = Fixture::new("restore-decompress").await.seed(200).await;
        fixture
            .persistence
            .set_archived(&fixture.session_id, true)
            .await
            .unwrap();
        let original = tokio::fs::read(fixture.dir.join(WIRE_LOG_FILE))
            .await
            .unwrap();
        compress_session(&fixture.persistence, &fixture.session_id, &fixture.dir)
            .await
            .unwrap();
        assert!(fixture.dir.join(WIRE_LOG_GZ_FILE).exists());

        restore_decompress(&fixture.persistence, &fixture.session_id, &fixture.dir)
            .await
            .unwrap();

        // Then the plain log is back, byte-identical (§6: 不可区分)
        let restored = tokio::fs::read(fixture.dir.join(WIRE_LOG_FILE))
            .await
            .unwrap();
        assert_eq!(restored, original);
        assert!(!fixture.dir.join(WIRE_LOG_GZ_FILE).exists());
        // And all four retention fields are clear
        let state = fixture.read_state().await;
        assert!(!state["archived"].as_bool().unwrap());
        assert!(state["archivedAt"].is_null());
        assert!(!state["compressed"].as_bool().unwrap());
        assert!(state["compressedAt"].is_null());
        assert!(state["preCompressedSize"].is_null());
    }

    /// Given a session not in the compressed shape, when
    /// restore-decompressed, then it is an idempotent no-op that still
    /// clears the archive batch.
    #[tokio::test]
    async fn should_be_a_no_op_for_the_shape_when_the_session_is_not_compressed() {
        let fixture = Fixture::new("restore-plain").await.seed(20).await;
        fixture
            .persistence
            .set_archived(&fixture.session_id, true)
            .await
            .unwrap();
        let before = tokio::fs::read(fixture.dir.join(WIRE_LOG_FILE))
            .await
            .unwrap();

        restore_decompress(&fixture.persistence, &fixture.session_id, &fixture.dir)
            .await
            .unwrap();

        let after = tokio::fs::read(fixture.dir.join(WIRE_LOG_FILE))
            .await
            .unwrap();
        assert_eq!(after, before, "the plain log must be untouched");
        assert!(fixture.archived_at().await.is_none());
    }

    /// Given a completed restore, when restore-decompress runs again,
    /// then it succeeds without side effects (retry idempotence, §6).
    #[tokio::test]
    async fn should_stay_idempotent_when_restore_decompress_is_retried() {
        let fixture = Fixture::new("restore-retry").await.seed(100).await;
        fixture
            .persistence
            .set_archived(&fixture.session_id, true)
            .await
            .unwrap();
        compress_session(&fixture.persistence, &fixture.session_id, &fixture.dir)
            .await
            .unwrap();

        restore_decompress(&fixture.persistence, &fixture.session_id, &fixture.dir)
            .await
            .unwrap();
        let after_first = tokio::fs::read(fixture.dir.join(WIRE_LOG_FILE))
            .await
            .unwrap();

        // When retried
        restore_decompress(&fixture.persistence, &fixture.session_id, &fixture.dir)
            .await
            .unwrap();

        // Then nothing changed
        let after_second = tokio::fs::read(fixture.dir.join(WIRE_LOG_FILE))
            .await
            .unwrap();
        assert_eq!(after_first, after_second);
        assert!(!fixture.dir.join(WIRE_LOG_GZ_FILE).exists());
    }

    /// Given the directory lock is held, when restore-decompress is
    /// called, then it fails with the retryable lock error.
    #[tokio::test]
    async fn should_error_when_the_directory_lock_is_held_on_restore() {
        let fixture = Fixture::new("restore-locked").await.seed(20).await;
        let _held = DirLockGuard::try_acquire(&fixture.dir).unwrap();

        let outcome =
            restore_decompress(&fixture.persistence, &fixture.session_id, &fixture.dir).await;

        assert!(outcome.is_err(), "a held lock must fail the restore loudly");
    }

    // -------------------------------------------------------------------
    // fork rejection (SDR-003)
    // -------------------------------------------------------------------

    /// Given a compressed source session, when the fork gate runs, then
    /// it returns the business error with the restore-first message.
    #[test]
    fn should_reject_the_fork_when_the_source_session_is_compressed() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(WIRE_LOG_FILE), "{}\n").unwrap();
        compress_wire_log(tmp.path()).unwrap();

        let error = reject_compressed_fork_source(tmp.path()).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("compressed session must be restored before fork"),
            "message should name the user action: {error}"
        );
    }

    /// Given a plain (or log-less) source session, when the fork gate
    /// runs, then it passes.
    #[test]
    fn should_pass_the_fork_when_the_source_session_is_plain() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(WIRE_LOG_FILE), "{}\n").unwrap();
        assert!(reject_compressed_fork_source(tmp.path()).is_ok());
        // A never-conversed session (no log at all) forks with empty
        // history — the gate must not reject it.
        let empty = tempfile::tempdir().unwrap();
        assert!(reject_compressed_fork_source(empty.path()).is_ok());
    }

    // -------------------------------------------------------------------
    // heal_compression_state (ADR-007)
    // -------------------------------------------------------------------

    /// Given the compressed shape with a false marker (crash between the
    /// atomic replacement and the stamp), when healed, then the marker
    /// batch is backfilled with the conservative pre_size.
    #[tokio::test]
    async fn should_heal_the_marker_when_the_shape_is_compressed_but_the_marker_is_false() {
        let fixture = Fixture::new("heal-stamp").await.seed(100).await;
        fixture
            .persistence
            .set_archived(&fixture.session_id, true)
            .await
            .unwrap();
        // Crash stand-in: the shape transitioned, the stamp never landed.
        compress_wire_log(&fixture.dir).unwrap();
        let gz_len = tokio::fs::metadata(fixture.dir.join(WIRE_LOG_GZ_FILE))
            .await
            .unwrap()
            .len();

        heal_compression_state(&fixture.persistence, &fixture.session_id, &fixture.dir)
            .await
            .unwrap();

        let state = fixture.read_state().await;
        assert!(state["compressed"].as_bool().unwrap());
        assert!(state["compressedAt"].is_string());
        // No recorded pre_size existed → the current .gz size (the
        // conservative disk-available stand-in, see the impl comment).
        assert_eq!(state["preCompressedSize"], json!(gz_len));
    }

    /// Given the plain shape with a true marker (crash between the
    /// restore's decompression and the marker clear), when healed, then
    /// the marker batch clears.
    #[tokio::test]
    async fn should_heal_the_marker_when_the_shape_is_plain_but_the_marker_is_true() {
        let fixture = Fixture::new("heal-clear").await.seed(100).await;
        // Crash stand-in: the restore removed the .gz and rebuilt the
        // plain log, but the marker clear never landed.
        fixture
            .persistence
            .set_compressed(&fixture.session_id, 123_456)
            .await
            .unwrap();

        heal_compression_state(&fixture.persistence, &fixture.session_id, &fixture.dir)
            .await
            .unwrap();

        let state = fixture.read_state().await;
        assert!(!state["compressed"].as_bool().unwrap());
        assert!(state["compressedAt"].is_null());
        assert!(state["preCompressedSize"].is_null());
    }

    /// Given marker and shape agree, when healed, then nothing is written
    /// (the heal is a no-op on consistent states).
    #[tokio::test]
    async fn should_not_write_when_the_marker_already_agrees_with_the_shape() {
        let fixture = Fixture::new("heal-consistent").await.seed(100).await;
        fixture
            .persistence
            .set_archived(&fixture.session_id, true)
            .await
            .unwrap();
        // Consistent plain: un-compressed marker + plain shape.
        let state_path = fixture.persistence.state_path(&fixture.session_id);
        let before = tokio::fs::read_to_string(&state_path).await.unwrap();

        heal_compression_state(&fixture.persistence, &fixture.session_id, &fixture.dir)
            .await
            .unwrap();

        let after = tokio::fs::read_to_string(&state_path).await.unwrap();
        assert_eq!(before, after, "a consistent state must not be rewritten");
    }

    // -------------------------------------------------------------------
    // Integration — the full happy path over a real SessionPersistence
    // (archive → due → compress → restore → append continues)
    // -------------------------------------------------------------------

    /// Given a session that is archived, compressed at its deadline,
    /// restored, and then appended to, when the whole cycle runs, then
    /// every intermediate state is correct and the post-restore append
    /// lands in the plain log (ADR-004's write-path premise).
    #[tokio::test]
    async fn should_keep_appending_after_a_full_archive_compress_restore_cycle() {
        let fixture = Fixture::new("full-cycle").await.seed(300).await;
        let session_id = fixture.session_id.clone();

        // Archive
        fixture
            .persistence
            .set_archived(&session_id, true)
            .await
            .unwrap();
        // Due (the verdict by the just-written stamp says not-yet; the
        // engine is driven by the scanner's deadline, so simulate the
        // deadline passing via the verdict on an aged stamp)
        let aged_stamp = (fixture.archived_at().await.unwrap() - Days::new(31))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, false);
        assert!(
            should_compress(Some(&aged_stamp), Utc::now(), 30),
            "an aged archive stamp must be due"
        );

        // Compress
        let outcome = compress_session(&fixture.persistence, &session_id, &fixture.dir)
            .await
            .unwrap();
        let CompressOutcome::Compressed { pre_size } = outcome else {
            panic!("the due session must compress: {outcome:?}");
        };
        // The expected bytes: decode the .gz directly for comparison.
        let expected: Vec<u8> = {
            let gz = std::fs::read(fixture.dir.join(WIRE_LOG_GZ_FILE)).unwrap();
            let mut decoder = flate2::read::GzDecoder::new(gz.as_slice());
            let mut out = Vec::new();
            std::io::Read::read_to_end(&mut decoder, &mut out).unwrap();
            out
        };
        assert_eq!(pre_size, expected.len() as u64);

        // Restore
        restore_decompress(&fixture.persistence, &session_id, &fixture.dir)
            .await
            .unwrap();
        let restored = tokio::fs::read(fixture.dir.join(WIRE_LOG_FILE))
            .await
            .unwrap();
        assert_eq!(restored, expected, "byte-identical restore (§6)");
        assert!(!fixture.dir.join(WIRE_LOG_GZ_FILE).exists());
        let state = fixture.read_state().await;
        assert!(!state["archived"].as_bool().unwrap() && !state["compressed"].as_bool().unwrap());

        // Append continues (ADR-004: the plain shape admits appends).
        // open_for_append — create would truncate the restored history.
        let mut log = JsonlAppendLog::open_for_append(&fixture.dir.join(WIRE_LOG_FILE))
            .await
            .unwrap();
        log.append(&padded_record(999)).await.unwrap();
        drop(log);
        let after_append = tokio::fs::read(fixture.dir.join(WIRE_LOG_FILE))
            .await
            .unwrap();
        assert!(
            after_append.len() > restored.len(),
            "the post-restore append must land in the plain log"
        );
        assert!(after_append.starts_with(&restored));
    }
}
