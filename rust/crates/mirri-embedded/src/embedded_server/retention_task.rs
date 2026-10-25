//! The retention compression task (architecture §2.1 / §8, plan Task 8) —
//! the periodic background pass the embedded layer assembles when
//! `[retention] enabled = true`.
//!
//! Same assembly shape as the orphan governance task
//! ([`orphan_governance`](super::orphan_governance)): the spawn rides the
//! first `call_method`'s lazy seam (construction may precede the Tokio
//! runtime; the first RPC never does), the task itself holds no
//! `&EmbeddedServer` — every input rides along as captured values.
//!
//! ## Assembly (§1 principle 6: 默认即无操作)
//!
//! The task is **not** spawned when the resolved `[retention]` config has
//! `enabled = false` (the default) — "不开启 = 行为与现状一致" is the
//! assembly-time gate, not a spawn-then-idle loop.
//!
//! ## Scheduling (§8)
//!
//! - The startup delay is a minute-scale constant
//!   ([`RETENTION_STARTUP_DELAY_SECS`]) **plus** a random jitter of up to
//!   [`RETENTION_STARTUP_JITTER_SECS`] (60 s), so multiple processes
//!   sharing a home stagger their first pass.
//! - Afterwards one round every `interval_hours` (re-read from the config
//!   file each round — the task never snapshots the config, US6.1).
//!
//! ## The round body — [`run_retention_pass`]
//!
//! One pass is a plain `async fn` (the test seam: the task only times it,
//! tests drive it directly):
//!
//! 1. Re-read the config from disk ([`mirri_core::load_retention`]). If
//!    `enabled` turned false, the round ends immediately (the task itself
//!    stays alive and re-checks on the next tick — unloading requires a
//!    restart; simple and safe, per the task ruling).
//! 2. Scan candidates: the session index's `archived = 1` rows
//!    (ADR-008: the index belongs to embedded), each disk-verified through
//!    `read_persisted_info` (the index is a cache, the disk is the source
//!    of truth), minus the live-session tables' ids (§5 tier 1: a running
//!    session never compresses).
//! 3. Per candidate: [`heal_compression_state`] (ADR-007 self-heal) →
//!    [`backfill_archived_at`] (SDR-008 legacy stamp) →
//!    [`should_compress`] → [`compress_session`] (directory lock +
//!    in-lock revalidation inside). Every action logs an info-level
//!    structured line (session id, action, outcome).
//! 4. Tail segment — the blob-ledger recheck (§3.4 复核宿主): resolve
//!    the SERVER's shared ledger (the same instance the delete executor
//!    decrements — SDR-004's "same instance" ruling; the double-instance
//!    gap fix) through its lazy slot and run
//!    `reclaim_aged(BLOB_AGING_WINDOW)`.
//!
//! ## Priority (US4.1)
//!
//! The repo has no low-priority-thread convention (the orphan sweeper and
//! the index-refresh driver are plain Tokio tasks); this task follows the
//! same shape — a spawned task whose per-round work rides
//! `spawn_blocking`, which the cooperative scheduler already treats as
//! best-effort background work.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use mirri_core::SessionId;
use mirri_core::blob_ledger::{BLOB_AGING_WINDOW, BlobLedger};
use mirri_core::session::retention::{
    CompressOutcome, backfill_archived_at, compress_session, heal_compression_state,
    should_compress,
};
use mirri_os::LocalSyncFs;

use super::EmbeddedServer;

/// The startup delay's constant base (§8: 分钟级) — the first pass waits at
/// least this long after the engine starts.
pub(super) const RETENTION_STARTUP_DELAY_SECS: u64 = 120;

/// The startup delay's random jitter ceiling (0~60 s): multiple processes
/// sharing a home stagger their first pass so they never open the scan in
/// lockstep (§5 多进程同时压缩).
pub(super) const RETENTION_STARTUP_JITTER_SECS: u64 = 60;

impl EmbeddedServer {
    /// The shared ledger slot handle: the SAME `Arc`-wrapped mutex the
    /// `blob_ledger` field is — handed to the retention task's spawn so its
    /// tail recheck and the delete executor's decrement address ONE slot
    /// (SDR-004's "same instance" ruling; the double-instance gap fix).
    pub(super) fn blob_ledger_slot(&self) -> Arc<tokio::sync::Mutex<Option<Arc<BlobLedger>>>> {
        Arc::clone(&self.blob_ledger)
    }

    /// The assembly seam (Task 8): spawn the retention compression task
    /// when the resolved `[retention]` config is enabled.
    ///
    /// Called from the first `call_method` (the same lazy seam every
    /// background driver uses). The config read is the in-memory snapshot
    /// here — the gate only decides *whether a task exists at all*; every
    /// round re-reads the file, so a config edit that flips `enabled`
    /// takes effect on the next round without a restart (and a config
    /// saved BEFORE the first RPC is honored by the same snapshot the
    /// first round re-reads).
    pub(super) fn maybe_spawn_retention_task(&self) {
        use std::sync::atomic::Ordering;

        // The once-guard (the `maybe_spawn_index_refresh_driver` pattern):
        // exactly one periodic task per server, regardless of how many
        // `call_method` invocations race past the enabled gate. The guard
        // is checked BEFORE the config read — a config flip between two
        // first-round RPCs must never mint a second task (every round
        // re-reads the config, so the single task honors the flip).
        if self.retention_task_spawned.swap(true, Ordering::SeqCst) {
            return;
        }
        if !self
            .config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .resolve_retention()
            .enabled
        {
            return; // 默认即无操作: no task, no loop, no tick (§1.6)
        }
        let sessions_root = self.manager.persistence().sessions_dir().to_path_buf();
        let config_path = self.config_path();
        let index = self.session_index();
        let registry = Arc::clone(&self.registry);
        // The shared ledger slot (the SDR-004 "same instance" ruling):
        // the delete executor (`shared_blob_ledger`) and this task's tail
        // recheck must address ONE ledger — a pass-held second instance
        // would never see the executor's zero crossings, and its
        // reclaim_aged could leave aged zeroed attachments unreclaimed
        // forever. The task captures the SERVER's slot (the same
        // `Arc`-cloned `tokio::sync::Mutex`), never a ledger of its own.
        let ledger_slot = self.blob_ledger_slot();
        // The new-reference hook (B2 / ADR-003's reuse rule): every blob
        // write the engine performs resets the shared ledger's zero
        // anchor through the process-wide hook, so a hash that left zero
        // via a new session's offload is never reclaimed by a recheck
        // that still holds the stale anchor. The ledger resolves lazily
        // inside the hook (the same slot the delete executor and the
        // recheck address — one instance); an unbuilt ledger is built on
        // the first reference, exactly as the first delete would.
        {
            let ledger_slot = Arc::clone(&ledger_slot);
            mirri_core::blob_ledger::install_blob_reference_hook(Arc::new(move |hash| {
                if let Ok(ledger) = ledger_slot.try_lock()
                    && let Some(ledger) = ledger.as_ref()
                {
                    ledger.note_reference(hash);
                }
            }));
        }
        // The blobs dir derived EXACTLY the way `shared_blob_ledger`
        // derives it (the injected home's `blobs`, else the sibling of
        // the sessions root): whichever of the two builds the shared
        // slot's ledger first, they agree on the store's location.
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
        tokio::spawn(async move {
            // The startup delay + jitter (§8): the first pass never runs
            // at engine start (a booting host is busy), and the random
            // jitter staggers multi-process deployments.
            let jitter = rand_jitter_secs(RETENTION_STARTUP_JITTER_SECS);
            tokio::time::sleep(std::time::Duration::from_secs(
                RETENTION_STARTUP_DELAY_SECS + jitter,
            ))
            .await;
            loop {
                // Each round re-reads the config file itself (the pass's
                // own step 1); `enabled = false` makes the round a no-op
                // and the loop sleeps the configured interval before the
                // next check (unloading still requires a restart — the
                // task ruling).
                let interval_hours = read_round_config(config_path.as_deref()).interval_hours;
                if let Err(error) = run_retention_pass(
                    &sessions_root,
                    index.as_ref(),
                    &registry,
                    &blobs_dir,
                    &ledger_slot,
                    config_path.as_deref(),
                )
                .await
                {
                    tracing::warn!(%error, "retention compression pass failed");
                }
                tokio::time::sleep(std::time::Duration::from_secs(
                    u64::from(interval_hours) * 3600,
                ))
                .await;
            }
        });
    }
}

/// One retention round (§2.1): the pure pass the periodic task times and
/// the tests drive directly.
///
/// The round starts by re-reading the `[retention]` config from
/// `config_path` (`None` degrades to the disabled defaults — the ambient
/// no-file posture). `enabled = false` ends the round immediately: no
/// scan, no compress, and — the one-way boundary's allowed side — no
/// blob recheck either (already-zeroed attachments stay put; §3.4).
///
/// `ledger_slot` is the SERVER's shared ledger slot (the same
/// `Arc<tokio::sync::Mutex<..>>` `EmbeddedServer::shared_blob_ledger`
/// resolves through — SDR-004's "same instance" ruling): the delete
/// executor's decrements and this pass's `reclaim_aged` must address one
/// ledger, or a zeroed-by-delete hash never reclaims. The ledger builds
/// lazily inside the slot on the first round that reaches the tail
/// segment and stays warm; the blob recheck host is this tail segment
/// (§3.4 复核宿主). `blobs_dir` is the store location the server itself
/// would build over (the home's `blobs`, else the sessions root's
/// sibling) — passed in because the free function has no server handle.
pub(super) async fn run_retention_pass(
    sessions_root: &std::path::Path,
    index: Option<&Arc<mirri_session_index::SessionIndex>>,
    registry: &crate::workspace_registry::WorkspaceRegistry,
    blobs_dir: &std::path::Path,
    ledger_slot: &Arc<tokio::sync::Mutex<Option<Arc<BlobLedger>>>>,
    config_path: Option<&std::path::Path>,
) -> Result<(), String> {
    // 1. The round's config — straight from the file (US6.1: the task
    //    never snapshots; a change saved between rounds is honored by
    //    the next one). A disabled round is a no-op: the already-
    //    compressed sessions keep their shape (US1.2 第 4 条), the task
    //    stays resident until restart.
    let config = read_round_config(config_path);
    if !config.enabled {
        tracing::debug!(
            "retention pass skipped: [retention] enabled is false \
             (already-compressed sessions keep their shape)"
        );
        return Ok(());
    }

    // 2. Candidate scan (§2.1 扫描): archived=1 rows from the index,
    //    disk-verified one by one, minus every live session.
    let candidates = scan_candidates(sessions_root, index, registry).await?;

    // 3. Per candidate: heal → backfill → verdict → compress. Every
    //    action is per-candidate best-effort (a failure logs and the
    //    batch continues — §2.1 "单条失败记日志跳过，不中断批次").
    for candidate in candidates {
        let persistence = &candidate.persistence;
        let session_id = &candidate.session_id;
        let dir = &candidate.dir;

        if let Err(error) = heal_compression_state(persistence, session_id, dir).await {
            tracing::warn!(
                session_id = %session_id,
                %error,
                "retention pass: compression-state heal failed; the candidate is skipped this round"
            );
            continue;
        }
        if let Err(error) = backfill_archived_at(persistence, session_id).await {
            tracing::warn!(
                session_id = %session_id,
                %error,
                "retention pass: archivedAt backfill failed; the candidate is skipped this round"
            );
            continue;
        }
        // Re-read the (possibly just-backfilled) stamp for the verdict.
        let archived_at = persistence
            .read_persisted_info(session_id)
            .await
            .and_then(|info| info.archived_at)
            .map(|at| at.to_rfc3339_opts(chrono::SecondsFormat::Secs, false));
        if !should_compress(
            archived_at.as_deref(),
            chrono::Utc::now(),
            config.archive_grace_days,
        ) {
            tracing::debug!(
                session_id = %session_id,
                "retention pass: not due yet (archivedAt + grace not reached)"
            );
            continue;
        }
        match compress_session(persistence, session_id, dir).await {
            Ok(CompressOutcome::Compressed { pre_size }) => {
                tracing::info!(
                    session_id = %session_id,
                    action = "compress",
                    pre_size,
                    "retention pass compressed an archived session"
                );
                // Task 10: the listing's compression state rides the index
                // row (the W5 source), so the just-written stamps
                // (compressed/compressedAt) refresh the row — the same
                // write-through compressNow performs (the shape-first
                // disk facts come through read_persisted_info).
                if let Some(index) = index
                    && let Some(info) = persistence.read_persisted_info(session_id).await
                    && let Some(bucket_key) = persistence.workspace_key()
                    && let Some(owner) = registry.registration(bucket_key)
                {
                    // The refreshed row: the same shape the index
                    // write-throughs build (session_row_with_retention
                    // over the just-read disk facts). The compression
                    // verdict is the SHAPE's (ADR-007 effective_compressed
                    // — shape first, the state.json marker only as the
                    // Missing fallback), never the raw marker: a crash
                    // between the atomic replacement and the stamp must
                    // not regress the row to the pre-compression view.
                    let effective = mirri_core::session::persistence::effective_compressed(
                        dir,
                        info.compressed,
                    );
                    let row = super::EmbeddedServer::session_row_with_retention(
                        session_id.as_str(),
                        &owner.id,
                        &owner.root,
                        &info.metadata,
                        None,
                        None,
                        info.archived_at,
                        effective,
                        info.compressed_at,
                    );
                    if let Err(error) = index.upsert_session(&row) {
                        tracing::warn!(
                            session_id = %session_id,
                            %error,
                            "retention pass: the compressed row's index \
                             write-through failed; the next reconcile rebuilds it"
                        );
                    }
                }
            }
            Ok(CompressOutcome::Skipped(reason)) => {
                // §5: 跳过不是终局 — a benign skip (lock held elsewhere,
                // or the in-lock revalidation found the state moved on).
                tracing::info!(
                    session_id = %session_id,
                    action = "skip",
                    reason = ?reason,
                    "retention pass skipped a candidate (not terminal; the next round retries)"
                );
            }
            Err(error) => {
                tracing::warn!(
                    session_id = %session_id,
                    action = "compress",
                    %error,
                    "retention pass: compression failed for one candidate; the batch continues"
                );
            }
        }
    }

    // 4. The tail segment — the blob recheck (§3.4 复核宿主): resolve
    //    the shared ledger through the server's slot (the SDR-004 "same
    //    instance" ruling — the zero crossings the delete executor stamps
    //    are visible HERE), building it lazily on the first round that
    //    reaches this point; every round reclaims the aged zeroed
    //    hashes. A failed build (an unreadable stream — the loud-failure
    //    stance) logs and the round still succeeds: the attachments
    //    stay, the one-way boundary's allowed side.
    match resolve_shared_ledger(blobs_dir, sessions_root, ledger_slot).await {
        Ok(Some(ledger)) => {
            let reclaimed = ledger.reclaim_aged(BLOB_AGING_WINDOW);
            if !reclaimed.is_empty() {
                tracing::info!(
                    count = reclaimed.len(),
                    "retention pass reclaimed aged zero-referenced attachments"
                );
            }
        }
        Ok(None) => {
            tracing::warn!(
                "retention pass: the shared blob ledger could not be built; the \
                 aged recheck is skipped this round (attachments stay)"
            );
        }
        Err(error) => {
            tracing::warn!(
                %error,
                "retention pass: the shared blob ledger build failed; the aged \
                 recheck is skipped this round (attachments stay)"
            );
        }
    }
    Ok(())
}

/// Resolve the shared ledger through the server's slot: return the warm
/// instance when one exists, else build (off the async workers, through
/// `spawn_blocking`) and park it in the slot — the exact resolution
/// [`EmbeddedServer::shared_blob_ledger`](super::retention_delete) performs,
/// shared here so the pass's `reclaim_aged` sees every zero crossing the
/// delete executor stamps.
async fn resolve_shared_ledger(
    blobs_dir: &std::path::Path,
    sessions_root: &std::path::Path,
    slot: &Arc<tokio::sync::Mutex<Option<Arc<BlobLedger>>>>,
) -> Result<Option<Arc<BlobLedger>>, String> {
    let mut slot = slot.lock().await;
    if let Some(ledger) = slot.as_ref() {
        return Ok(Some(Arc::clone(ledger)));
    }
    let built = tokio::task::spawn_blocking({
        let blobs_dir = blobs_dir.to_path_buf();
        let sessions_root = sessions_root.to_path_buf();
        move || BlobLedger::build(&blobs_dir, &[sessions_root])
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;
    let ledger = Arc::new(built);
    *slot = Some(Arc::clone(&ledger));
    Ok(Some(ledger))
}

/// One compressed-candidate candidate: the verified disk row plus the
/// bucket persistence handle the actions address.
struct RetentionCandidate {
    session_id: SessionId,
    dir: PathBuf,
    persistence: mirri_core::SessionPersistence,
}

/// The candidate scan (§2.1): `archived = 1` rows from the index (or the
/// disk walk when no index exists), each disk-verified through
/// `read_persisted_info`, minus the live-session tables' ids.
///
/// The per-candidate persistence handle is the owning workspace's (W3:
/// the state-path resolution must match the rest of the lifecycle), and
/// the directory comes from that same handle — never a hand-built path.
async fn scan_candidates(
    sessions_root: &std::path::Path,
    index: Option<&Arc<mirri_session_index::SessionIndex>>,
    registry: &crate::workspace_registry::WorkspaceRegistry,
) -> Result<Vec<RetentionCandidate>, String> {
    // §5 tier 1 (候选排除): every live session of every BUILT runtime is
    // excluded — a running session never compresses.
    let mut live_ids: HashSet<String> = HashSet::new();
    for runtime in registry.built_runtimes() {
        for id in runtime.manager.workspace().list_sessions().await {
            live_ids.insert(id.as_str().to_string());
        }
    }

    // The archived rows: from the index when it exists (the cheap
    // indexed source), else the default manager's persisted listing.
    let mut rows: Vec<(String, String)> = Vec::new(); // (session_id, workspace_id)
    if let Some(index) = index {
        let listed = index
            .list_sessions(mirri_session_index::ListOpts {
                include_archived: true,
                ..mirri_session_index::ListOpts::default()
            })
            .map_err(|e| e.to_string())?;
        for row in listed {
            if row.archived {
                rows.push((row.id, row.workspace_id));
            }
        }
    } else {
        for persisted in list_persisted_archived(sessions_root).await {
            rows.push((persisted, String::new()));
        }
    }

    let mut candidates = Vec::new();
    for (session_id, workspace_id) in rows {
        if live_ids.contains(&session_id) {
            continue; // a running session is never a candidate
        }
        // The owning workspace's persistence (W3): route through the
        // registry when it knows the owner; the no-index degrade uses a
        // root-level handle over the same sessions root. The workspace
        // key IS the bucket name (the registry's `id` — the identity the
        // index row's `workspace_id` carries).
        let persistence = if workspace_id.is_empty() {
            mirri_core::SessionPersistence::new(
                sessions_root.to_path_buf(),
                Arc::new(mirri_os::LocalFs::new()),
            )
        } else if registry.registration(&workspace_id).is_some() {
            mirri_core::SessionPersistence::new(
                sessions_root.to_path_buf(),
                Arc::new(mirri_os::LocalFs::new()),
            )
            .with_workspace_key(workspace_id)
        } else {
            // A row of a workspace this registry does not know: its
            // owning engine sweeps it; skip (the same posture as the
            // index orphan pass).
            continue;
        };
        let id = SessionId::new(&session_id);
        let dir = persistence.session_dir(&id);
        // The disk verification (§2.1: 索引是缓存、磁盘是事实源): the row
        // must still read as archived on disk; a lock-held directory is
        // not excluded here — the compress action's own lock attempt
        // handles contention (§5 tier 2).
        let Some(info) = persistence.read_persisted_info(&id).await else {
            continue; // unreadable state.json — the index row is stale
        };
        if !info.metadata.archived {
            continue; // restored since the row was written
        }
        candidates.push(RetentionCandidate {
            session_id: id,
            dir,
            persistence,
        });
    }
    Ok(candidates)
}

/// The no-index degrade: every archived persisted session under the root
/// (both layouts), through the default bucket-scanning persistence.
async fn list_persisted_archived(sessions_root: &std::path::Path) -> Vec<String> {
    let persistence = mirri_core::SessionPersistence::new(
        sessions_root.to_path_buf(),
        Arc::new(mirri_os::LocalFs::new()),
    );
    persistence
        .list_persisted_sessions(true)
        .await
        .into_iter()
        .filter(|info| info.archived)
        .map(|info| info.session_id)
        .collect()
}

/// The round config: re-read from `path` when given, else the ambient
/// in-file resolution degrade (no path → disabled defaults; tests that
/// drive [`run_retention_pass`] directly pass their own config file).
fn read_round_config(path: Option<&std::path::Path>) -> mirri_core::RetentionConfig {
    match path {
        Some(path) => mirri_core::load_retention(path, &LocalSyncFs::new()),
        None => mirri_core::RetentionConfig::disabled_defaults(),
    }
}

/// A uniformly-random `0..=ceiling` seconds value.
///
/// `rand` is deliberately not pulled in for one draw: the std hash of a
/// nanosecond timestamp plus the thread id spreads well enough for a
/// stagger whose only requirement is "not the same value every process".
fn rand_jitter_secs(ceiling: u64) -> u64 {
    use std::hash::{DefaultHasher as SipHasher, Hash, Hasher};
    let mut hasher = SipHasher::new();
    std::time::SystemTime::now().hash(&mut hasher);
    std::thread::current().id().hash(&mut hasher);
    let drawn = hasher.finish();
    if ceiling == 0 {
        return 0;
    }
    drawn % (ceiling + 1)
}
