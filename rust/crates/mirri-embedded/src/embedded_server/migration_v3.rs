//! ADR-006 Task 9 — the one-shot metadata migration (裁决 5 核心) with the
//! exclusive hold and the version floor lock.
//!
//! A pre-ADR-006 home carries materialized sessions as directories with a
//! `state.json` but **no metadata row** (the old write-through's normal
//! shape). Task 6 shipped `mirri migrate` as the manual command; this module
//! is the automatic counterpart that runs **inside the engine's startup
//! sequence** — the user opens the new version and the migration completes
//! by itself, exactly once, guarded by a library-internal format stamp.
//!
//! ## The sequence (strict order)
//!
//! 1. `open_with_check` already ran at the wiring point (the bootstrap); the
//!    stamp check below re-reads the `meta` table through the shared
//!    connection — a stamped library answers `FreshOk` immediately (the
//!    stamp IS the completion marker; the seed never re-runs).
//! 2. **Exclusive hold, two layers** (裁决 5: 迁移独占持库):
//!    - process level: the `<home>/session-index.migration.lock` file lock
//!      (`create_new` + stale-takeover, the `session-index.reconcile.lock`
//!      pattern), HELD for the whole migration — another live entry's
//!      migration holds it, and the loser answers
//!      [`MigrationOutcome::BlockedByLiveInstance`] with the "please exit
//!      the other entry first" guidance;
//!    - library level: a `BEGIN IMMEDIATE` take-and-release probe with a
//!      500 ms busy timeout on a dedicated connection — the belt-and-braces
//!      against a live foreign writer the file lock cannot see (a writer
//!      already inside the database when the migration enters). The probe
//!      cannot be HELD while the migration writes (the writes ride the
//!      shared connection, and SQLite hands out one writer slot — a held
//!      fence would deadlock the migration against itself); it proves the
//!      slot was free at entry and defers to a writer that answers within
//!      the budget. Failing to take it is also
//!      [`MigrationOutcome::BlockedByLiveInstance`].
//! 3. `backup_library` — the rolling two-generation snapshot BEFORE any
//!    destructive step (裁决 2's migration-point backup; best-effort like
//!    every other housekeeping seam, never a startup blocker).
//! 4. **Seed convergence**: every registered workspace's bucket enumerates
//!    its `state.json`-carrying session directories; a session whose row
//!    is absent is read through the validated `read_persisted_info` and
//!    seeded with its **historical `created_at`** (the row's birthday is
//!    the state document's, not the migration moment). Seeded rows land
//!    through `bulk_upsert_sessions` (one transaction, all-or-nothing per
//!    bucket batch). This is the last legal `state.json` read (the reconcile
//!    retiree — Task 10 removes the standing pass that still reads it).
//! 5. **Death-list consumption**: every bucket on `workspaces.json`'s
//!    `deleted_workspace_ids` has its rows batch-deleted and tombstoned —
//!    the registry's tombstone semantics win over any residue a foreign
//!    engine left in the library.
//! 6. **The format stamp**: `meta.schema_format = 2` — the
//!    "metadata-source-of-truth era" chapter. `open_with_check`'s floor
//!    compares against it; a future build's floor recognizes the era.
//!
//! Idempotence: the stamp at step 1 short-circuits every re-entry, so the
//! seed's own row-existence skip is a second guard (a crash after seeding
//! but before stamping re-runs the seed over rows that already exist —
//! skipped one by one).
//!
//! ## Wiring
//!
//! [`EmbeddedServer::run_one_shot_migration_once`] rides the census gate's
//! settlement body ([`super::orphan_governance`]'s `OnceCell` initializer):
//! the seed runs INSIDE the census settlement (off the first request's
//! inline path — the e956f5e8a principle) but BEFORE any census waiter is
//! released, so the first dispatch that reads the sessions surface sees a
//! fully seeded library. A `BlockedByLiveInstance` verdict degrades loudly
//! but non-fatally: the engine serves with whatever rows the winning
//! instance's migration produced.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::Utc;
use mirri_core::SessionId;
use mirri_session_index::{SessionIndex, SessionRow};

use super::EmbeddedServer;
use crate::workspace_registry::WorkspaceRegistry;

/// The migration's exclusive file lock body — `<home>/session-index.migration`
/// (the `.lock` sibling the `RegistryLock` pattern appends).
const MIGRATION_LOCK_STEM: &str = "session-index.migration";

/// The busy timeout the migration connection holds while taking the
/// exclusive transaction — deliberately far below the engine's 5 s: a live
/// writer answers within 500 ms or the migration defers to it (裁决 5's
/// "migration never fights a live entry").
const MIGRATION_BUSY_TIMEOUT_MS: u64 = 500;

/// The outcome of [`run_one_shot_migration`].
#[derive(Debug)]
pub(super) enum MigrationOutcome {
    /// The library is already at the metadata-source-of-truth era (the
    /// `meta.schema_format` stamp is present) — nothing ran, nothing was
    /// written.
    FreshOk,
    /// The migration ran: `rows` metadata rows were seeded from `state.json`
    /// documents whose row was absent.
    Migrated { rows: usize },
    /// Another live entry holds the library (its migration lock, or an
    /// exclusive write lock the 500 ms budget could not out-wait) — the
    /// engine should serve with the rows the winner's migration produced
    /// and surface the "exit the other entry" guidance.
    BlockedByLiveInstance,
}

/// Run the one-shot metadata migration for the home behind `registry`'s
/// shared session index — see the module doc for the sequence.
///
/// `None` (no shared index — no home injected) never happens in production
/// wiring but keeps the direct-constructor test shapes sound: the caller
/// treats it as `FreshOk` (nothing to migrate over).
pub(super) async fn run_one_shot_migration(
    registry: &WorkspaceRegistry,
) -> Option<MigrationOutcome> {
    let index = registry.shared_session_index()?;
    let home = registry.session_index_home()?;
    let sessions_root = home.join("v3_sessions");
    let registry_path = home.join(mirri_session_index::WORKSPACE_REGISTRY_FILENAME);
    // The in-memory registration face (the bound workspace included — a
    // fresh home's workspaces.json may not list it yet): the registration
    // write-through runs over THIS list so the row lands on every boot
    // (the retired reconcile pass carried it; ADR-006).
    let live_registrations: Vec<(String, PathBuf)> = registry
        .registrations()
        .into_iter()
        .map(|r| (r.id, r.root))
        .collect();

    // The blocking body (file lock + state.json reads + SQLite writes) rides
    // the blocking pool — the census initializer awaits it off the async
    // workers.
    let index = Arc::clone(&index);
    let outcome = tokio::task::spawn_blocking(move || {
        run_one_shot_migration_blocking(
            &home,
            &sessions_root,
            &registry_path,
            &index,
            &live_registrations,
        )
    })
    .await;
    match outcome {
        Ok(outcome) => Some(outcome),
        Err(join_error) => {
            tracing::warn!(
                %join_error,
                "the one-shot metadata migration task failed to join; the next start retries"
            );
            // A failed join means the migration state is unknown (a stamp may
            // or may not have landed) — treat it as blocked, not as fresh.
            Some(MigrationOutcome::BlockedByLiveInstance)
        }
    }
}

/// The migration body — synchronous (the file lock's polling loop and the
/// SQLite writes are blocking by nature; the caller offloaded them).
fn run_one_shot_migration_blocking(
    home: &Path,
    sessions_root: &Path,
    registry_path: &Path,
    index: &SessionIndex,
    live_registrations: &[(String, PathBuf)],
) -> MigrationOutcome {
    // ① The stamp check — a stamped library is done for the SEED, full stop
    // (the stamp is the completion marker; the seed never re-runs over it).
    // The registration write-through still runs on every boot (the retired
    // reconcile pass carried it; a fresh home's workspaces.json may not yet
    // know the bound workspace, and the upsert preserves created_at /
    // sort_order — idempotent, one row per workspace).
    if library_is_stamped(index) {
        register_live_workspaces(index, live_registrations);
        return MigrationOutcome::FreshOk;
    }

    // ② The process-level exclusive hold: the migration lock file. A live
    // holder outlasting the wait budget means another entry is migrating
    // (or serving with the file held) — defer to it.
    let lock_path = mirri_session_index::lock_file_path(&home.join(MIGRATION_LOCK_STEM));
    let lock = mirri_session_index::RegistryLock::new(lock_path).acquire();
    let guard = match lock {
        Ok(guard) => guard,
        Err(error) => {
            tracing::warn!(
                %error,
                "the one-shot metadata migration is blocked by another live entry; \
                 exit the other engine entry and restart to finish the migration"
            );
            return MigrationOutcome::BlockedByLiveInstance;
        }
    };

    // ③ The library-level exclusive probe — a dedicated connection takes
    // `BEGIN IMMEDIATE` with the 500 ms busy budget and commits it right
    // away. The probe cannot be HELD while the migration writes (the writes
    // ride the shared `SessionIndex` connection — a second connection —
    // and SQLite hands out exactly one writer slot, so holding the fence
    // would deadlock the migration against itself). Instead the probe
    // answers "is a live foreign writer inside?" — one that answers within
    // the budget gets to finish, and the migration defers (`Blocked`);
    // a clean take proves the writer slot was free at entry, and the
    // process-level file lock keeps every other entry out from here on.
    let db_path = home.join("session-index.db");
    if probe_library_free(&db_path).is_err() {
        tracing::warn!(
            "the one-shot metadata migration found a live writer on {} within {} ms; \
             exit the other engine entry and restart to finish the migration",
            db_path.display(),
            MIGRATION_BUSY_TIMEOUT_MS
        );
        return MigrationOutcome::BlockedByLiveInstance;
    }

    // Re-check the stamp after the locks: the winner between the first
    // check and the lock acquisition may have stamped the library already
    // (the file lock serializes, but the first check ran before it).
    if library_is_stamped(index) {
        drop(guard);
        return MigrationOutcome::FreshOk;
    }

    // ④ The pre-migration backup (裁决 2): the rolling two-generation
    // snapshot a destructive step must be able to roll back to. Best-effort —
    // a failed backup logs (a fresh home has no database file to back up
    // until the first row lands; `backup_library`'s NotFound is the expected
    // first-boot shape) and the migration proceeds.
    if let Err(error) = mirri_session_index::backup::backup_library(&db_path) {
        tracing::debug!(
            %error,
            "the pre-migration backup did not run (a fresh home has nothing to back \
             up yet); the migration proceeds"
        );
    }

    // ⑤ Seed convergence + ⑥ the death list — the stamp lands only after
    // both settled (a crash before it re-runs the seed over rows that
    // already exist — the per-row skip).
    register_live_workspaces(index, live_registrations);
    let registrations = registry::registrations_on_disk(registry_path);
    let mut rows: Vec<SessionRow> = Vec::new();
    for registration in &registrations {
        collect_seed_rows(
            index,
            sessions_root,
            &registration.id,
            &registration.root,
            &mut rows,
        );
    }
    let seeded = rows.len();
    if let Err(error) = index.bulk_upsert_sessions(&rows) {
        tracing::warn!(
            %error,
            count = seeded,
            "the one-shot metadata migration's seed batch failed; the next start retries"
        );
        // A failed seed never stamps: the next start re-runs it.
        drop(guard);
        return MigrationOutcome::BlockedByLiveInstance;
    }

    // ⑥ The death list — a failed consumption BLOCKS the stamp exactly like
    // a failed seed: the completion marker must never paper over residue
    // the list was supposed to remove (the next start re-runs the pass
    // over the already-seeded rows, each skipped).
    if let Err(error) = consume_death_list(index, registry_path) {
        tracing::warn!(
            %error,
            "the one-shot metadata migration's death-list consumption failed; the format \
             stamp is withheld and the next start retries the pass"
        );
        drop(guard);
        return MigrationOutcome::BlockedByLiveInstance;
    }
    // ⑦ The format stamp — the metadata-source-of-truth era chapter. The last
    // write of the migration; its commit is what makes every later start a
    // `FreshOk`.
    if let Err(error) = stamp_format(index) {
        tracing::warn!(
            %error,
            "stamping the metadata era failed; the next start re-runs the migration \
             over the already-seeded rows (each is skipped)"
        );
        drop(guard);
        return MigrationOutcome::BlockedByLiveInstance;
    }

    drop(guard);
    if seeded > 0 {
        tracing::info!(
            rows = seeded,
            "one-shot metadata migration settled: rows seeded from state.json, era stamped"
        );
    } else {
        tracing::info!("one-shot metadata migration settled: no missing rows, era stamped");
    }
    MigrationOutcome::Migrated { rows: seeded }
}

/// Whether the library carries the metadata-era stamp (`meta.schema_format`
/// present and readable) — the completion marker the idempotent re-entry
/// checks.
fn library_is_stamped(index: &SessionIndex) -> bool {
    read_format_stamp(index).is_some()
}

/// The library's `meta.schema_format` value, when the `meta` table carries
/// a parseable one.
fn read_format_stamp(index: &SessionIndex) -> Option<i64> {
    index
        .read_meta_value("schema_format")
        .and_then(|value| value.parse().ok())
}

/// Stamp the metadata-source-of-truth era into the `meta` table — the
/// chapter every later `open_with_check` floor check reads.
fn stamp_format(index: &SessionIndex) -> anyhow::Result<()> {
    index.write_meta_value("schema_format", &KNOWN_SCHEMA_FORMAT.to_string())
}

/// The schema-format era this build stamps — the same constant
/// `open_with_check` judges the floor against (mirrored here because the
/// original is private to `mirri-session-index`; a future era bump changes
/// both in lockstep through the crate's own tests).
const KNOWN_SCHEMA_FORMAT: i64 = 2;

/// Probe the library-level writer slot: a dedicated connection takes
/// `BEGIN IMMEDIATE` with the degraded 500 ms busy timeout and commits
/// immediately. A live FOREIGN writer holds the slot → the probe fails
/// within the budget → the migration defers; a clean take proves the slot
/// was free at entry (the process-level file lock then keeps every other
/// entry out for the rest of the migration).
///
/// `IMMEDIATE` (not `EXCLUSIVE`) is deliberate: in WAL mode `EXCLUSIVE`
/// blocks even readers, while `IMMEDIATE` claims exactly the single writer
/// slot — the "no other writer commits during the migration" check 裁决 5
/// asks for, without freezing the readers of a home a live entry is
/// serving.
fn probe_library_free(db_path: &Path) -> rusqlite::Result<()> {
    let conn = rusqlite::Connection::open(db_path)?;
    conn.busy_timeout(std::time::Duration::from_millis(MIGRATION_BUSY_TIMEOUT_MS))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    let guard =
        rusqlite::Transaction::new_unchecked(&conn, rusqlite::TransactionBehavior::Immediate)?;
    // Take-and-release: the probe's answer is the TAKE itself. Committing
    // the empty transaction releases the slot immediately — the migration's
    // own writes then ride the shared connection without self-deadlock.
    guard.commit()
}

/// Enumerate the bucket's `state.json` sessions whose row is ABSENT and mint
/// their [`SessionRow`]s into `rows` — the seed convergence of one workspace
/// bucket. The historical `created_at` (the state document's) rides the row
/// verbatim: the row's birthday is the session's, never the migration's.
///
/// The state read is the bootstrap bridge's shape (a dedicated current-thread
/// runtime — `read_persisted_info` is async and this body is sync).
fn collect_seed_rows(
    index: &SessionIndex,
    sessions_root: &Path,
    bucket_key: &str,
    workspace_root: &Path,
    rows: &mut Vec<SessionRow>,
) {
    let io: Arc<dyn mirri_os::DynFileSystem> = Arc::new(mirri_os::LocalFs::new());
    let bucket_persistence =
        mirri_core::SessionPersistence::new(sessions_root.to_path_buf(), Arc::clone(&io))
            .with_workspace_key(bucket_key.to_string());
    let bucket_dir = sessions_root.join(bucket_key);
    let sessions = EmbeddedServer::enumerate_bucket_sessions(&bucket_dir);
    if sessions.is_empty() {
        return; // no bucket yet → nothing to seed (the common fresh-home case)
    }
    for (session_id, mtime) in sessions {
        // Idempotence, per row: an existing row is never re-seeded (the
        // row's view is the truth — the same predicate `mirri migrate`'s
        // seed applies).
        if index.get_session(&session_id).is_some() {
            continue;
        }
        let row =
            {
                let persistence = bucket_persistence.clone();
                let session_id_for_thread = session_id.clone();
                let bucket_key = bucket_key.to_string();
                let workspace_root = workspace_root.to_path_buf();
                std::thread::spawn(move || {
                    tokio::runtime::Builder::new_current_thread()
                        .build()
                        .expect("one-shot metadata migration runtime")
                        .block_on(async move {
                            let sid = SessionId::new(&session_id_for_thread);
                            let info = persistence.read_persisted_info(&sid).await?;
                            Some(mirri_session_index::SessionRow {
                                id: session_id_for_thread.clone(),
                                workspace_id: bucket_key,
                                cwd: info.metadata.work_dir.clone().or_else(|| {
                                    Some(workspace_root.to_string_lossy().into_owned())
                                }),
                                title: Some(info.metadata.title.clone()),
                                created_at: info.metadata.created_at,
                                updated_at: info.metadata.updated_at,
                                archived: info.metadata.archived,
                                tags: info.metadata.tags.iter().cloned().collect(),
                                custom: None,
                                last_prompt: info.metadata.last_prompt.clone(),
                                state_mtime: Some(mtime),
                                archived_at: info.archived_at,
                                compressed: info.compressed,
                                compressed_at: info.compressed_at,
                                pinned: false,
                                pinned_at: None,
                                pin_color: None,
                                last_used_at: info.metadata.last_used_at,
                                pre_compressed_size: info
                                    .pre_compressed_size
                                    .and_then(|size| i64::try_from(size).ok()),
                            })
                        })
                })
                .join()
                .unwrap_or_else(|panic| {
                    tracing::warn!(
                        panic = ?panic,
                        session_id = %session_id,
                        "one-shot metadata migration: the state.json reader panicked"
                    );
                    None
                })
            };
        match row {
            Some(row) => rows.push(row),
            None => tracing::warn!(
                session_id = %session_id,
                "one-shot metadata migration: state.json could not be read; no row \
                 seeded and the session stays skipped permanently — it is invisible to \
                 row-backed listings until recovered (mirri migrate, or restore the \
                 file and restart before the format stamp lands)"
            ),
        }
    }
}

/// Consume `workspaces.json`'s death list: every tombstoned workspace id's
/// rows are batch-deleted and recorded in the tombstone ledger, so a
/// workspace the user deleted is never resurrected by residue a foreign
/// engine left in the library (the registry's tombstone semantics win — the
/// same precedence `read_workspace_registry` applies to registrations).
///
/// **Failures block the stamp** (the caller withholds the format chapter
/// and retries on the next start): a half-consumed death list that the
/// completion marker papered over would leave deleted-workspace residue
/// visible forever — the stamp means "done", and a failed consumption is
/// anything but. A partial batch (some rows deleted, the ledger write
/// failed) is retried harmlessly: the already-deleted rows are simply no
/// longer in `list_all_rows` next time.
fn consume_death_list(index: &SessionIndex, registry_path: &Path) -> anyhow::Result<()> {
    let dead = registry::deleted_workspace_ids(registry_path);
    if dead.is_empty() {
        return Ok(());
    }
    let rows = index
        .list_all_rows()
        .map_err(|error| anyhow::anyhow!("listing the rows for the death-list consumption: {error}"))?;
    let now = Utc::now();
    let mut tombstones: Vec<(String, chrono::DateTime<Utc>)> = Vec::new();
    for row in rows {
        if dead.contains(&row.workspace_id) {
            index.delete_session(&row.id).map_err(|error| {
                anyhow::anyhow!("deleting the death-listed row {}: {error}", row.id)
            })?;
            tombstones.push((row.id, now));
        }
    }
    if tombstones.is_empty() {
        // The death list named workspaces with no rows in the library —
        // nothing to consume, nothing to record.
        return Ok(());
    }
    index
        .record_tombstones(&tombstones)
        .map_err(|error| anyhow::anyhow!("recording the death-list tombstones: {error}"))?;
    tracing::info!(
        workspaces = dead.len(),
        rows = tombstones.len(),
        "one-shot metadata migration consumed the deleted-workspace death list"
    );
    Ok(())
}

/// The disk-side registration facts the migration seeds from: the rows
/// `workspaces.json` carries on disk (the registry file is the cross-engine
/// source the bootstrap restores from), independent of the in-memory
/// registry's runtime state.
mod registry {
    use std::path::{Path, PathBuf};

    /// One on-disk workspace registration the migration seeds a bucket for.
    pub(super) struct DiskRegistration {
        pub(super) id: String,
        pub(super) root: PathBuf,
    }

    /// The registrations `workspaces.json` carries (a missing or corrupt
    /// file reads as empty — the same tolerance the bootstrap's restore
    /// applies).
    pub(super) fn registrations_on_disk(registry_path: &Path) -> Vec<DiskRegistration> {
        mirri_session_index::read_workspace_registry(registry_path)
            .into_iter()
            .filter_map(|row| {
                let root = row.root?;
                Some(DiskRegistration {
                    id: row.id,
                    root: PathBuf::from(root),
                })
            })
            .collect()
    }

    /// The `deleted_workspace_ids` of `workspaces.json` (the death list).
    pub(super) fn deleted_workspace_ids(registry_path: &Path) -> std::collections::HashSet<String> {
        std::fs::read_to_string(registry_path)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .and_then(|document| {
                document
                    .get("deleted_workspace_ids")
                    .and_then(serde_json::Value::as_array)
                    .map(|ids| {
                        ids.iter()
                            .filter_map(serde_json::Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
            })
            .unwrap_or_default()
    }
}

/// The registration write-through the retired reconcile pass carried
/// (ADR-006): every workspace of the in-memory face (the bound workspace
/// included) is registered in the index, preserving an existing row's
/// `created_at` / `sort_order`. The upsert writes through to
/// `workspaces.json`, so a foreign engine sharing the home sees this
/// entry's workspaces too. Failures only skip that workspace's row — the
/// next boot retries.
fn register_live_workspaces(index: &SessionIndex, live: &[(String, PathBuf)]) {
    for (id, root) in live {
        if let Err(error) = crate::embedded_server::EmbeddedServer::register_workspace_in_index(
            index, root, id,
        ) {
            tracing::warn!(
                %error,
                workspace = %id,
                "registering the workspace in the session index failed during the migration"
            );
        }
    }
}
