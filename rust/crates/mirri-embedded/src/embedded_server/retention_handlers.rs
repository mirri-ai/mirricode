//! Session-retention management RPC — the `retention.*` route arms
//! (Task 9: the handlers over the session index, the retention engine,
//! and the unified delete executor).
//!
//! The typed contracts live in `mirri_proto::retention`; this module owns
//! the dispatch, the index/disk scans, and the conversions.
//!
//! | method | handler |
//! |---|---|
//! | `retention.list` | the archive / compressed view rows over the index + disk shape verdicts |
//! | `retention.stats` | the header counters + the four per-tier delete estimates |
//! | `retention.compressNow` | the core `compress_session` (same lock, same in-lock revalidation, deadline bypassed) |
//! | `retention.delete` | the unified delete executor (single) |
//! | `retention.deleteTiered` | the same executor, batched per tier with per-row outcomes |
//! | `retention.getPolicy` | the resolved `[retention]` knobs (implemented since Task 7) |

use std::sync::Arc;

use mirri_core::session::retention::{CompressOutcome, SkipReason, compress_session};
use mirri_core::{SessionId, SessionPersistence};
use mirri_proto::{
    RETENTION_DELETE_TIER_DAYS, RetentionCompressNowParams, RetentionCompressNowResult,
    RetentionDeleteParams, RetentionDeleteResult, RetentionDeleteTieredFailure,
    RetentionDeleteTieredParams, RetentionDeleteTieredResult, RetentionListEntry,
    RetentionListParams, RetentionListResult, RetentionListView, RetentionStatsResult,
    RetentionTierEstimate,
};
use mirri_session_index::ListOpts;
use serde_json::Value;

use super::retention_delete::{DeleteOutcome, dir_size};
use super::{EmbeddedServer, invalid_params};
use mirri_transport::adapter::EngineError;

/// One scan row: the index row's identity plus the verified disk facts.
struct ScanRow {
    session_id: String,
    workspace: String,
    title: String,
    archived_at: Option<String>,
    compressed_at: Option<String>,
    pre_compressed_size: Option<u64>,
    current_size_bytes: u64,
    /// The ADR-007 shape verdict (disk truth, not the marker).
    compressed_shape: bool,
    /// The current `wire.jsonl.gz` byte size, read from disk during the
    /// scan — the savings computation's subtrahend (M2: `saved_bytes` is
    /// `preCompressedSize - gz size`, never the pre-size itself).
    gz_size: u64,
}

/// The pre-verification scan entry (the index row's identity and stamps,
/// before the disk facts are gathered).
struct RawScanEntry {
    session_id: String,
    workspace: String,
    archived_at: Option<String>,
    compressed_at: Option<String>,
    pre_compressed_size: Option<u64>,
}

impl EmbeddedServer {
    /// `retention.list` — the management page's archive / compressed view
    /// listing: `include = archived` serves every archived session in the
    /// NON-compressed shape; `include = compressed` every archived
    /// session in the compressed shape.
    ///
    /// The rows come from the index (the `archived = 1` set) with each
    /// row's disk facts re-verified (shape, occupancy, savings) — the
    /// index is the candidate source, the disk the truth (§2.1).
    pub(super) async fn dispatch_retention_list(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let params: RetentionListParams =
            serde_json::from_value(normalize(params)).map_err(|e| invalid_params(e.to_string()))?;
        let rows = self.scan_retention_rows().await?;

        let items: Vec<RetentionListEntry> = rows
            .into_iter()
            .filter(|row| match params.include {
                RetentionListView::Archived => !row.compressed_shape,
                RetentionListView::Compressed => row.compressed_shape,
            })
            .map(|row| {
                let saved_bytes = row.saved_bytes();
                let compressed_days = row.compressed_days();
                RetentionListEntry {
                    session_id: row.session_id,
                    title: row.title,
                    workspace: row.workspace,
                    archived_at: row.archived_at,
                    compressed_at: row.compressed_at,
                    current_size_bytes: row.current_size_bytes,
                    saved_bytes,
                    compressed_days,
                }
            })
            .collect();
        to_value(RetentionListResult { items })
    }

    /// `retention.stats` — the header counters plus the four per-tier
    /// delete estimates (30/90/180/365, the product constant set).
    pub(super) async fn dispatch_retention_stats(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        if !params.is_null() && !params.as_object().is_some_and(|object| object.is_empty()) {
            return Err(invalid_params("retention.stats takes no params"));
        }
        let rows = self.scan_retention_rows().await?;

        let mut archived_count: u64 = 0;
        let mut compressed_count: u64 = 0;
        let mut compressed_total_bytes: u64 = 0;
        let mut reclaimed_potential: u64 = 0;
        // The per-tier accumulators (ascending tier order — the constant
        // set's own order, carried straight into the result).
        let mut tier_counts = [0u64; RETENTION_DELETE_TIER_DAYS.len()];
        let mut tier_bytes = [0u64; RETENTION_DELETE_TIER_DAYS.len()];
        for row in rows {
            if !row.compressed_shape {
                archived_count += 1;
                reclaimed_potential += row.current_size_bytes;
            } else {
                compressed_count += 1;
                compressed_total_bytes += row.current_size_bytes;
                // The tier membership: "already compressed for N days"
                // counts at every tier whose threshold N meets (a
                // cumulative count, the confirmation dialog's "N
                // sessions / X MB" for that button).
                if let Some(days) = row.compressed_days() {
                    for (index, threshold) in RETENTION_DELETE_TIER_DAYS.iter().enumerate() {
                        if days >= *threshold {
                            tier_counts[index] += 1;
                            tier_bytes[index] += row.current_size_bytes;
                        }
                    }
                }
            }
        }
        let tier_days = RETENTION_DELETE_TIER_DAYS
            .iter()
            .zip(tier_counts.iter().zip(tier_bytes.iter()))
            .map(|(days, (count, bytes))| RetentionTierEstimate {
                days: *days,
                count: *count,
                bytes: *bytes,
            })
            .collect();
        to_value(RetentionStatsResult {
            archived_count,
            compressed_count,
            compressed_total_bytes,
            reclaimed_potential,
            tier_days,
        })
    }

    /// `retention.compressNow` — compress one session immediately (the
    /// management page's row action): the SAME core `compress_session`
    /// (directory lock + in-lock revalidation + atomic swap) with the
    /// deadline bypassed — the in-lock revalidation's own conditions
    /// (still archived, still plain-shaped) are the guard; the day count
    /// is not consulted (§2.1 立即压缩).
    pub(super) async fn dispatch_retention_compress_now(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let params: RetentionCompressNowParams =
            serde_json::from_value(normalize(params)).map_err(|e| invalid_params(e.to_string()))?;
        let (persistence, session_id) = self.retention_session(&params.session_id).await?;
        let dir = persistence.session_dir(&session_id);

        // The deadline bypass: `compress_session`'s own in-lock
        // revalidation (archived + plain shape) is the whole guard —
        // the caller's "now" replaces the grace-period verdict.
        let outcome = compress_session(&persistence, &session_id, &dir)
            .await
            .map_err(super::session_error)?;
        match outcome {
            CompressOutcome::Compressed { .. } => {}
            // Already compressed (either shape or marker, in-lock
            // revalidated) is the IDEMPOTENT success — the requested
            // end state already holds, so ok:true is the honest answer
            // (M1: a repeated click on a compressed row must not error).
            CompressOutcome::Skipped(SkipReason::Revalidated) => {
                // A NOT-archived (restored) or already-compressed
                // session: distinguish them — the compressed shape is
                // the idempotent ok; a restored session is a business
                // error (the user asked to compress a session that no
                // longer qualifies).
                if mirri_persistence::probe_compressed(&dir)
                    == mirri_persistence::CompressedShape::Compressed
                {
                    tracing::info!(
                        session_id = %params.session_id,
                        "retention compressNow: session already compressed (idempotent ok)"
                    );
                } else {
                    // Restored between the click and the lock: the
                    // precondition (archived) no longer holds — a
                    // business error, never a fake success (M1).
                    return Err(invalid_params(format!(
                        "session {} is not archived; only archived sessions can be \
                         compressed (restore-first sessions must be re-archived)",
                        params.session_id
                    )));
                }
            }
            // A held directory lock is a busy/retryable business error on
            // an EXPLICIT user action (M1): the compression did not
            // happen and "ok" would lie about it.
            CompressOutcome::Skipped(SkipReason::Locked) => {
                return Err(invalid_params(format!(
                    "session {} is busy: another action (compression / restore / \
                     delete) holds its directory lock; retry shortly",
                    params.session_id
                )));
            }
        }
        // Task 10: the listing's compression state rides the index row
        // (the W5 listing source), so the just-written state.json stamps
        // (compressed/compressedAt) refresh the row immediately — the
        // same `index_refresh_from_disk` write-through the archive and
        // restore dispatches perform.
        self.index_refresh_from_disk(&session_id).await;
        to_value(RetentionCompressNowResult { ok: true })
    }

    /// `retention.delete` — single-session delete through the unified
    /// executor.
    pub(super) async fn dispatch_retention_delete(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let params: RetentionDeleteParams =
            serde_json::from_value(normalize(params)).map_err(|e| invalid_params(e.to_string()))?;
        let (persistence, session_id) = self.retention_session(&params.session_id).await?;
        let index = self.session_index();
        let outcome = self
            .delete_session_with_reclaim(&persistence, index.as_ref(), &session_id)
            .await?;
        let reclaimed_bytes = match outcome {
            DeleteOutcome::Deleted { reclaimed_bytes } => reclaimed_bytes,
            DeleteOutcome::AlreadyGone => 0,
        };
        to_value(RetentionDeleteResult {
            ok: true,
            reclaimed_bytes,
        })
    }

    /// `retention.deleteTiered` — the tier-button batch: every compressed
    /// session already compressed for ≥ `older_than_days` days, deleted
    /// through the same unified executor, with per-row outcomes (US3.4's
    /// honest partial accounting).
    pub(super) async fn dispatch_retention_delete_tiered(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        let params: RetentionDeleteTieredParams =
            serde_json::from_value(normalize(params)).map_err(|e| invalid_params(e.to_string()))?;
        if !RETENTION_DELETE_TIER_DAYS.contains(&params.older_than_days) {
            return Err(invalid_params(format!(
                "older_than_days must be one of {RETENTION_DELETE_TIER_DAYS:?} (the product's \
                 fixed tier set), got {}",
                params.older_than_days
            )));
        }
        let rows = self.scan_retention_rows().await?;
        let index = self.session_index();
        let mut deleted: Vec<String> = Vec::new();
        let mut failed: Vec<RetentionDeleteTieredFailure> = Vec::new();
        let mut reclaimed_bytes: u64 = 0;
        for row in rows {
            if !row.compressed_shape {
                continue; // the tier set addresses compressed sessions only
            }
            let Some(days) = row.compressed_days() else {
                continue; // no compressed_at anchor — not tier-addressable
            };
            if days < params.older_than_days {
                continue; // below the tier
            }
            let session_id_str = row.session_id.clone();
            let outcome = async {
                let (persistence, session_id) = self.retention_session(&row.session_id).await?;
                let index = index.clone();
                self.delete_session_with_reclaim(&persistence, index.as_ref(), &session_id)
                    .await
            }
            .await;
            match outcome {
                Ok(DeleteOutcome::Deleted {
                    reclaimed_bytes: bytes,
                }) => {
                    reclaimed_bytes += bytes;
                    deleted.push(session_id_str);
                }
                Ok(DeleteOutcome::AlreadyGone) => {
                    // The idempotent success — the row was already gone;
                    // report it deleted (its tier membership was real).
                    deleted.push(session_id_str);
                }
                Err(error) => {
                    failed.push(RetentionDeleteTieredFailure {
                        session_id: session_id_str,
                        error: error.to_string(),
                    });
                }
            }
        }
        to_value(RetentionDeleteTieredResult {
            deleted,
            failed,
            reclaimed_bytes,
        })
    }

    /// `retention.getPolicy` — unchanged since Task 7 (the config
    /// snapshot read).
    pub(super) async fn dispatch_retention_get_policy(
        &self,
        params: Value,
    ) -> Result<Value, EngineError> {
        if !params.is_null() && !params.as_object().is_some_and(|object| object.is_empty()) {
            return Err(invalid_params("retention.getPolicy takes no params"));
        }
        let retention = self
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .resolve_retention();
        let result = mirri_proto::RetentionPolicyResult {
            enabled: retention.enabled,
            archive_grace_days: retention.archive_grace_days,
            interval_hours: retention.interval_hours,
        };
        serde_json::to_value(result).map_err(|error| {
            EngineError::Internal(format!("retention.getPolicy serialization failed: {error}"))
        })
    }

    // ------------------------------------------------------------------
    // Shared scan plumbing
    // ------------------------------------------------------------------

    /// The owning workspace's persistence + the parsed id for one
    /// retention-addressed session (the lifecycle not-found mapping).
    async fn retention_session(
        &self,
        session_id: &str,
    ) -> Result<(SessionPersistence, SessionId), EngineError> {
        let (owner_manager, _workspace) = self
            .resolve_session_owner(session_id)
            .map_err(|_| super::not_found(format!("session not found: {session_id}")))?;
        Ok((
            owner_manager.persistence().clone(),
            SessionId::new(session_id),
        ))
    }

    /// The retention scan (the `retention.list` / `stats` /
    /// `deleteTiered` source): the index's archived rows, each with its
    /// disk facts re-verified — the shape verdict (ADR-007), the
    /// occupancy (the directory's whole tree), and the stamps.
    ///
    /// The no-index degrade: the disk walk over the default root (the
    /// same listing the persistence layer performs).
    async fn scan_retention_rows(&self) -> Result<Vec<ScanRow>, EngineError> {
        let sessions_root = self.manager.persistence().sessions_dir().to_path_buf();
        let mut raw: Vec<RawScanEntry> = Vec::new();
        if let Some(index) = self.session_index() {
            let rows = index
                .list_sessions(ListOpts {
                    include_archived: true,
                    ..ListOpts::default()
                })
                .map_err(|error| {
                    EngineError::Internal(format!(
                        "scanning the session index for archived rows failed: {error}"
                    ))
                })?;
            for row in rows {
                if !row.archived {
                    continue;
                }
                raw.push(RawScanEntry {
                    session_id: row.id,
                    workspace: row.workspace_id,
                    archived_at: row.archived_at.map(|at| stamp(&at)),
                    compressed_at: row.compressed_at.map(|at| stamp(&at)),
                    // pre-size is not an index column — disk-read below
                    pre_compressed_size: None,
                });
            }
        } else {
            let persistence =
                SessionPersistence::new(sessions_root.clone(), Arc::new(mirri_os::LocalFs::new()));
            for info in persistence.list_persisted_sessions(true).await {
                if !info.archived {
                    continue;
                }
                raw.push(RawScanEntry {
                    session_id: info.session_id,
                    workspace: String::new(),
                    archived_at: info.archived_at.map(|at| stamp(&at)),
                    compressed_at: info.compressed_at.map(|at| stamp(&at)),
                    pre_compressed_size: info.pre_compressed_size,
                });
            }
        }

        let mut rows = Vec::with_capacity(raw.len());
        for RawScanEntry {
            session_id,
            workspace,
            archived_at,
            compressed_at,
            pre_compressed_size: pre_size,
        } in raw
        {
            // The workspace-scoped persistence: the bucket key matches the
            // index row's workspace_id (the registry's identity rule).
            let persistence = if workspace.is_empty() {
                SessionPersistence::new(sessions_root.clone(), Arc::new(mirri_os::LocalFs::new()))
            } else {
                SessionPersistence::new(sessions_root.clone(), Arc::new(mirri_os::LocalFs::new()))
                    .with_workspace_key(workspace.clone())
            };
            let id = SessionId::new(&session_id);
            let dir = persistence.session_dir(&id);
            // The disk facts: shape, occupancy, the title (from the
            // state.json read when available).
            let compressed_shape = matches!(
                mirri_persistence::probe_compressed(&dir),
                mirri_persistence::CompressedShape::Compressed
            );
            // The current .gz size — the savings subtrahend (M2). Read
            // from disk NOW (the scan's own fact-gathering pass) so the
            // row's `saved_bytes` reflects the file as it stands, not a
            // stale or absent recording.
            let gz_size = std::fs::metadata(dir.join(mirri_persistence::WIRE_LOG_GZ_FILE))
                .map(|meta| meta.len())
                .unwrap_or(0); // zero-io-allow: retention scan — a stat of the persistence-owned .gz (the same class probe_compressed's is_file carries)
            let current_size_bytes = dir_size(&dir).unwrap_or(0);
            let title = persistence
                .read_persisted_info(&id)
                .await
                .map(|info| info.metadata.title)
                .unwrap_or_default();
            // The pre-size when not carried by the index scan: read from
            // the state.json (the compressed batch's recorded basis).
            let pre_compressed_size = match pre_size {
                Some(size) => Some(size),
                None => persistence
                    .read_persisted_info(&id)
                    .await
                    .and_then(|info| info.pre_compressed_size),
            };
            rows.push(ScanRow {
                session_id,
                workspace,
                title,
                archived_at,
                compressed_at,
                pre_compressed_size,
                current_size_bytes,
                compressed_shape,
                gz_size,
            });
        }
        Ok(rows)
    }
}

impl ScanRow {
    /// The compression savings (§3.1, M2): `preCompressedSize` minus the
    /// CURRENT `.gz` size — only meaningful on the compressed view (rows
    /// never compressed answer `None`). The subtrahend is the live disk
    /// fact gathered during the scan, so a re-compressed or externally
    /// rewritten `.gz` reports honest savings, never the raw pre-size.
    fn saved_bytes(&self) -> Option<u64> {
        if !self.compressed_shape {
            return None;
        }
        Some(
            self.pre_compressed_size
                .unwrap_or(0)
                .saturating_sub(self.gz_size),
        )
    }

    /// How many whole days the wire log has been compressed (the server
    /// clock, never the client's — US3.1).
    fn compressed_days(&self) -> Option<u32> {
        if !self.compressed_shape {
            return None;
        }
        let at = self.compressed_at.as_deref().and_then(|stamp| {
            chrono::DateTime::parse_from_rfc3339(stamp)
                .ok()
                .map(|at| at.with_timezone(&chrono::Utc))
        })?;
        let elapsed = chrono::Utc::now().signed_duration_since(at);
        Some(u32::try_from(elapsed.num_days().max(0)).unwrap_or(u32::MAX))
    }
}

/// Normalize null params to `{}` (the dispatch convention).
fn normalize(params: Value) -> Value {
    if params.is_null() {
        serde_json::json!({})
    } else {
        params
    }
}

/// Serialize a typed result (the shared error mapping).
fn to_value<T: serde::Serialize>(value: T) -> Result<Value, EngineError> {
    serde_json::to_value(value)
        .map_err(|error| EngineError::Internal(format!("retention serialization failed: {error}")))
}

/// RFC 3339 with second precision (the stamps' wire form).
fn stamp(at: &chrono::DateTime<chrono::Utc>) -> String {
    at.to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}
