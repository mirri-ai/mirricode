//! Phase 3 — the SQLite session/workspace index integration for the
//! embedded [`EmbeddedServer`](super::EmbeddedServer).
//!
//! The engine keeps a `session-index.db` read model at
//! `<injected home>/session-index.db` (see [`mirri_session_index`]), mirroring
//! the TS engine's session index design:
//!
//! - **Open** — lazy, on the first RPC ([`EmbeddedServer::session_index`]):
//!   construction may run before the home is attached
//!   ([`EmbeddedServer::from_parts`] / [`EmbeddedServer::with_home_dir`] set
//!   it after the fact), and an engine with no injected home never touches a
//!   database at all — every consumer degrades to today's plain disk scan.
//! - **Converge** — once per process, before the first dispatch reads the
//!   sessions root ([`EmbeddedServer::reconcile_session_index_once`]): rows
//!   that are missing or whose `state.json` mtime differs are rebuilt from
//!   the same validated disk read the listing performs, and rows whose
//!   session directory no longer exists are deleted (the orphan pass).
//! - **Write-through** — every mutation that changes what a listing would
//!   show (first-send materialization — SDR-004's lazy create defers the
//!   row to the first user input — archive, restore, close, rename,
//!   metadata update) upserts the row best-effort, guarded on the session
//!   having materialized (`state.json` exists): a failure logs and never
//!   fails the RPC, and a never-conversed session never leaves an orphan
//!   row.
//! - **Read** — `session.list`'s merged listing source (W5): the index's
//!   multi-workspace rows (via [`ListOpts`]'s `workspace_ids` /
//!   `include_archived` filters and keyset cursors) union the live-session
//!   union of every built runtime (live wins by id — it also covers a
//!   lazy-created session that has not materialized yet), merged and sorted
//!   by `(updated_at DESC, id DESC)`. Each merged entry carries the row's
//!   recorded `cwd` (never a bound-root backfill) and `last_prompt` (the
//!   SDR-004 `exclude_empty` source, surfaced as the view's `lastPrompt`).
//!   The no-index degrade keeps the pre-W5 live-union-plus-default-bucket
//!   scan (no `last_prompt` source — `exclude_empty` degrades off).
//!
//! Like the TS converge, the reconcile is registry-driven (W3): every
//! registered workspace's bucket converges — not just the default's — so a
//! persisted session of a non-default workspace enters the index (the
//! routing seam's fallback reads it; without the row, a resume or a
//! paginated read would not find it).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use chrono::Utc;

use mirri_core::{SessionId, SessionInfo, SessionMetadata, SessionScope, SessionStatus};
use mirri_session_index::{ListOpts, SessionIndex, SessionRow, WorkspaceRow};

use super::EmbeddedServer;
use super::session_routing::workspace_owner_via;
use crate::workspace_registry::WorkspaceRegistry;

impl EmbeddedServer {
    /// The session/workspace index, opened lazily at
    /// `<injected home>/session-index.db` — the same lazy-once pattern as
    /// the auth registry: `from_parts`/`with_home_dir` attach the home after
    /// construction, so the first RPC (which always runs after the home is
    /// known) is the safe open point.
    ///
    /// Assembled engines share the registry's connection (W3 unification):
    /// the composition root installs the home on the registry and both the
    /// routing fallback's reads and this server's own index reads go
    /// through one lazily opened handle, never two. Direct-constructed
    /// servers (no registry home installed) keep their own lazy open.
    ///
    /// `None` for the process lifetime when no home is injected (tests,
    /// workspace-local engines) or the open failed — either way every
    /// consumer falls back to today's plain disk scan, exactly the TS
    /// degrade-to-`FileSessionIndex` behaviour.
    pub(super) fn session_index(&self) -> Option<Arc<SessionIndex>> {
        if let Some(shared) = self.registry.shared_session_index() {
            return Some(shared);
        }
        self.session_index
            .get_or_init(|| {
                let home = self.home_dir.as_deref()?;
                match SessionIndex::open(&home.join("session-index.db")) {
                    Ok(index) => Some(Arc::new(index)),
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            home = %home.display(),
                            "opening session-index.db failed; session reads fall \
                             back to the disk scan for this process"
                        );
                        None
                    }
                }
            })
            .clone()
    }

    /// The `state.json` mtime in epoch milliseconds — the stamp the
    /// incremental reconcile compares against (`std::fs`, full precision;
    /// the injected `DynFileSystem` stat only carries seconds).
    fn state_mtime_ms(path: &Path) -> Option<i64> {
        std::fs::metadata(path)
            .ok()?
            .modified()
            .ok()?
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .map(|elapsed| elapsed.as_millis() as i64)
    }

    /// Enumerate the session directories of one workspace bucket — the
    /// cheap readdir of the TS listing pattern (no `state.json` content
    /// reads): every child directory carrying a `state.json`, with its
    /// mtime, sorted by session id ascending (the order the plain scan's
    /// listing produces).
    fn enumerate_bucket_sessions(bucket_dir: &Path) -> Vec<(String, i64)> {
        let Ok(entries) = std::fs::read_dir(bucket_dir) else {
            return Vec::new(); // no bucket yet → no sessions
        };
        let mut sessions = Vec::new();
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let state_path = path.join("state.json");
            if !state_path.is_file() {
                continue;
            }
            let Some(id) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let Some(mtime) = Self::state_mtime_ms(&state_path) else {
                continue;
            };
            sessions.push((id, mtime));
        }
        sessions.sort_by(|a, b| a.0.cmp(&b.0));
        sessions
    }

    /// Build the index row for a session from its metadata. The `cwd`
    /// prefers the session's own recorded workDir and falls back to the
    /// owning workspace's root. `last_prompt` is passed in by the caller
    /// under the N1 new-value-first merge (the fresh disk/scope value wins,
    /// the row's existing value is the fallback; the send path sets it
    /// fresh — SDR-004: the most recent user input regardless of the turn's
    /// outcome).
    fn session_row_from_metadata(
        session_id: &str,
        workspace_key: &str,
        workspace_root: &Path,
        metadata: &SessionMetadata,
        last_prompt: Option<String>,
        state_mtime: Option<i64>,
    ) -> SessionRow {
        let cwd = metadata
            .work_dir
            .clone()
            .or_else(|| Some(workspace_root.to_string_lossy().into_owned()));
        SessionRow {
            id: session_id.to_string(),
            workspace_id: workspace_key.to_string(),
            cwd,
            title: Some(metadata.title.clone()),
            created_at: metadata.created_at,
            updated_at: metadata.updated_at,
            archived: metadata.archived,
            tags: metadata.tags.iter().cloned().collect(),
            custom: None,
            last_prompt,
            state_mtime,
        }
    }

    /// The one-time converge of the index with the on-disk sessions — the
    /// TS `sessionIndexService` startup reconcile. Runs inline in the first
    /// `call_method` (the [`Self::migrate_legacy_sessions_once`] seam: it
    /// must settle before the first dispatch reads the sessions root).
    ///
    /// Registry-driven (W3): **every registered workspace's bucket**
    /// converges — the default's and every lazily registered row's — so a
    /// persisted session of a non-default workspace enters the index before
    /// the routing seam's fallback needs it. Buckets are read through a
    /// lightweight per-bucket persistence reader over the shared sessions
    /// root (no workspace runtime is built by the converge; D5's lazy build
    /// stays untouched). Best-effort throughout (a failure logs and never
    /// aborts the call — the disk scan remains the source of truth and the
    /// next start retries).
    pub(super) async fn reconcile_session_index_once(&self) {
        use std::sync::atomic::Ordering;

        if self.session_index_reconciled.swap(true, Ordering::SeqCst) {
            return;
        }
        self.reconcile_session_index().await;
    }

    /// The re-runnable converge of the index with the on-disk sessions —
    /// the once-guarded body of
    /// [`Self::reconcile_session_index_once`], exposed without the
    /// per-process guard for the session-consistency index refresh driver
    /// (batch 1, US C1.1): a foreign process's mutation of the shared
    /// home (`state.json` rewrite) moves the file's mtime, and this pass
    /// rebuilds the stale rows from the same validated disk read — each
    /// pass serialized against every other process's pass through the
    /// `<home>/session-index.reconcile.lock` file (ADR-004's lock-file +
    /// stale-takeover pattern; a separate lock from workspaces.json.lock
    /// because the pass's own registry write-through takes THAT lock —
    /// nesting the same file would deadlock). A lock wait timeout skips
    /// the round — the next tick retries.
    ///
    /// No-ops without an index (no injected home) — the disk scan stays
    /// the source of truth.
    pub(super) async fn reconcile_session_index(&self) {
        let Some(index) = self.session_index() else {
            return;
        };
        // The shared sessions root every registered workspace's bucket lives
        // under (a home-injected engine's `<home>/v3_sessions`; a
        // workspace-local engine has exactly one registered workspace).
        let sessions_root = self.manager.persistence().sessions_dir().to_path_buf();
        reconcile_session_index_via(&self.registry, &sessions_root, &index).await;
    }

    /// Register (or touch) the bound workspace in the index — preserving an
    /// existing registration's `created_at` and `sort_order` so a restart
    /// does not reset either.
    fn register_workspace_in_index(
        index: &SessionIndex,
        workspace_root: &Path,
        workspace_key: &str,
    ) -> anyhow::Result<()> {
        let now = Utc::now();
        let mut row = WorkspaceRow {
            id: workspace_key.to_string(),
            root: Some(workspace_root.to_string_lossy().into_owned()),
            name: Some(
                workspace_root
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            ),
            created_at: now,
            last_opened_at: now,
            sort_order: 0,
        };
        if let Some(existing) = index.get_workspace(workspace_key) {
            row.created_at = existing.created_at;
            row.sort_order = existing.sort_order;
        }
        index.upsert_workspace(&row)
    }

    /// Write-through for a live session's mutation (rename, metadata
    /// update, restore, close): upsert the row from the live metadata plus
    /// the current `state.json` mtime, routed to the owning workspace's
    /// bucket (W3 — a non-default workspace's session must not land in the
    /// default's rows). The row's `last_prompt` is merged new-value-first
    /// (N1): the live scope metadata carries the send path's stamp
    /// (ADR-009), so it wins; the row's existing value is only the fallback
    /// for paths whose metadata lacks the key — the same direction as the
    /// startup backfill above. Best-effort — a failure logs and never
    /// fails the RPC that produced it.
    ///
    /// SDR-004 guard: a **lazily created session that has not been
    /// materialized** (no `state.json` — no user message yet) writes no
    /// row: an unmaterialized session must stay invisible to the
    /// persisted listing.
    pub(super) fn index_upsert_live(&self, session_id: &SessionId, metadata: &SessionMetadata) {
        let index = self.session_index();
        Self::index_upsert_live_at(index.as_ref(), &self.registry, session_id, metadata);
    }

    /// The core of [`Self::index_upsert_live`] as a free function — shared
    /// with the turn-settle write-through ([`note_turn_settled`]) so the
    /// spawned turn tasks of the cron/task drivers (which hold no
    /// `&EmbeddedServer`) write the exact same rows.
    pub(super) fn index_upsert_live_at(
        index: Option<&Arc<SessionIndex>>,
        registry: &WorkspaceRegistry,
        session_id: &SessionId,
        metadata: &SessionMetadata,
    ) {
        let Some(index) = index else {
            return;
        };
        let Some(owner) = workspace_owner_via(registry, session_id.as_str()) else {
            tracing::debug!(
                session_id = %session_id,
                "session-index write-through skipped: no workspace owns the session"
            );
            return;
        };
        let state_path = owner.manager.persistence().state_path(session_id);
        if !state_path.is_file() {
            tracing::debug!(
                session_id = %session_id,
                "session-index write-through skipped: the session is not materialized yet"
            );
            return;
        }
        let state_mtime = EmbeddedServer::state_mtime_ms(&state_path);
        // N1 new-value-first merge (same direction as the startup backfill
        // above): the live metadata carries the freshest last_prompt (the
        // send path stamps the scope, ADR-009); the row's value is the
        // fallback for metadata built without the key.
        let last_prompt = metadata.last_prompt.clone().or_else(|| {
            index
                .get_session(session_id.as_str())
                .and_then(|row| row.last_prompt)
        });
        let row = EmbeddedServer::session_row_from_metadata(
            session_id.as_str(),
            &owner.workspace_id,
            &owner.root,
            metadata,
            last_prompt,
            state_mtime,
        );
        if let Err(error) = index.upsert_session(&row) {
            tracing::warn!(
                session_id = %session_id,
                %error,
                "session-index write-through failed; the listing falls back to the disk scan"
            );
        }
    }

    /// The send path's write-through (SDR-004): the session's `last_prompt`
    /// is the most recent user input — set fresh on every user message,
    /// regardless of the turn's outcome ("the conversation failed" is not
    /// empty). Best-effort like [`Self::index_upsert_live`]; the materialized
    /// state file is guaranteed by the caller (the send path materializes
    /// before this runs).
    pub(super) fn index_note_last_prompt(
        &self,
        session_id: &SessionId,
        metadata: &SessionMetadata,
        prompt: &str,
    ) {
        let Some(index) = self.session_index() else {
            return;
        };
        let Some(owner) = workspace_owner_via(&self.registry, session_id.as_str()) else {
            tracing::debug!(
                session_id = %session_id,
                "session-index last-prompt write skipped: no workspace owns the session"
            );
            return;
        };
        let state_path = owner.manager.persistence().state_path(session_id);
        if !state_path.is_file() {
            tracing::debug!(
                session_id = %session_id,
                "session-index last-prompt write skipped: the session is not materialized yet"
            );
            return;
        }
        let state_mtime = Self::state_mtime_ms(&state_path);
        let row = Self::session_row_from_metadata(
            session_id.as_str(),
            &owner.workspace_id,
            &owner.root,
            metadata,
            Some(prompt.to_string()),
            state_mtime,
        );
        if let Err(error) = index.upsert_session(&row) {
            tracing::warn!(
                session_id = %session_id,
                %error,
                "session-index last-prompt write-through failed"
            );
        }
    }

    /// Write-through for a persisted-only session's mutation (archive of a
    /// session that is not live): re-read the (just-written) `state.json`
    /// through the validated single-session read and upsert the row, routed
    /// to the owning workspace's bucket like [`Self::index_upsert_live`].
    pub(super) async fn index_refresh_from_disk(&self, session_id: &SessionId) {
        let Some(index) = self.session_index() else {
            return;
        };
        let Some(owner) = workspace_owner_via(&self.registry, session_id.as_str()) else {
            tracing::debug!(
                session_id = %session_id,
                "session-index write-through skipped: no workspace owns the session"
            );
            return;
        };
        let Some(metadata) = owner
            .manager
            .persistence()
            .read_persisted_summary(session_id)
            .await
        else {
            return;
        };
        // The re-read state carries the session's last_prompt (both
        // engines persist it, ADR-009 — `read_persisted_summary` round-trips
        // it) — N1 new-value-first: the fresh disk value wins, the row's is
        // the fallback for old state.json files that lack the key (same
        // direction as the startup backfill and `index_upsert_live_at`).
        let state_mtime = Self::state_mtime_ms(&owner.manager.persistence().state_path(session_id));
        let last_prompt = metadata.last_prompt.clone().or_else(|| {
            self.session_index()
                .and_then(|index| index.get_session(session_id.as_str()))
                .and_then(|row| row.last_prompt)
        });
        let row = Self::session_row_from_metadata(
            session_id.as_str(),
            &owner.workspace_id,
            &owner.root,
            &metadata,
            last_prompt,
            state_mtime,
        );
        if let Err(error) = index.upsert_session(&row) {
            tracing::warn!(
                session_id = %session_id,
                %error,
                "session-index write-through failed; the listing falls back to the disk scan"
            );
        }
    }

    /// The live-session union over every BUILT workspace runtime (W4): each
    /// built runtime's live sessions report their live `SessionInfo` (live
    /// metadata + agent count, always listed regardless of the archived
    /// flag). The persisted-bucket scan stays default-only until W5 replaces
    /// the whole listing source. Runtimes iterate default first then by id
    /// (deterministic).
    pub(super) async fn live_session_infos_all_workspaces(&self) -> Vec<SessionInfo> {
        let mut infos: Vec<SessionInfo> = Vec::new();
        for runtime in self.registry.built_runtimes() {
            infos.extend(runtime.manager.list_sessions().await);
        }
        infos
    }

    /// The index-resolved W5 listing source: [`SessionIndex::list_sessions`]
    /// (a **multi-workspace** query — the persisted rows of every workspace in
    /// scope, not just the bound bucket) merged with the live union over every
    /// built runtime. A live session REPLACES its index row — live metadata
    /// is fresher, and an unmaterialized lazily created session (SDR-004)
    /// exists only in the live set. The merged sequence sorts by
    /// `(updated_at DESC, id DESC)`, the global order the keyset cursors page
    /// over (kap-server's list route order).
    ///
    /// `workspace_ids` scopes both arms (`None`/empty = every workspace —
    /// the TS `workspaceIds` semantics); a live session whose owner falls
    /// outside the scope is dropped, exactly like the index rows outside it
    /// were never fetched. `include_archived` is the raw archived opt-in;
    /// the `archived_only` narrow filter applies at the dispatch site (after
    /// the merge — a live session of an archived-and-restored state stays
    /// visible to the narrow filter, which forces inclusion here).
    ///
    /// No cwd is backfilled: a persisted row reports its own recorded cwd
    /// (true attribution, W5), a live row's view computes it from live
    /// metadata (W4).
    pub(super) async fn merged_session_entries(
        &self,
        workspace_ids: Option<&[String]>,
        include_archived: bool,
    ) -> Vec<MergedSessionEntry> {
        let in_scope = |workspace_id: &str| -> bool {
            match workspace_ids.filter(|ids| !ids.is_empty()) {
                Some(ids) => ids.iter().any(|id| id == workspace_id),
                None => true,
            }
        };
        let mut by_id: HashMap<String, MergedSessionEntry> = HashMap::new();
        if let Some(index) = self.session_index()
            && let Ok(rows) = index.list_sessions(mirri_session_index::ListOpts {
                workspace_ids,
                include_archived,
                ..ListOpts::default()
            })
        {
            for row in rows {
                by_id.insert(
                    row.id.clone(),
                    MergedSessionEntry {
                        info: SessionInfo {
                            id: row.id.clone(),
                            title: row.title.clone().unwrap_or_default(),
                            created_at: row.created_at,
                            updated_at: row.updated_at,
                            agent_count: 0,
                            tags: row.tags.to_vec(),
                            archived: row.archived,
                            status: SessionStatus::Closed,
                        },
                        cwd: row.cwd,
                        last_prompt: row.last_prompt,
                    },
                );
            }
        }
        for live in self.live_session_infos_all_workspaces().await {
            let Some(owner) = workspace_owner_via(&self.registry, &live.id) else {
                continue;
            };
            if !in_scope(&owner.workspace_id) {
                continue;
            }
            // The live row's last_prompt comes from the index — the live
            // `SessionInfo` carries no such state, and the send path's
            // write-through keeps the row warm (the scope's own metadata
            // stamps the same value, ADR-009).
            let last_prompt = self
                .session_index()
                .and_then(|index| index.get_session(&live.id))
                .and_then(|row| row.last_prompt);
            by_id.insert(
                live.id.clone(),
                MergedSessionEntry {
                    info: live,
                    cwd: None,
                    last_prompt,
                },
            );
        }
        let mut entries: Vec<MergedSessionEntry> = by_id.into_values().collect();
        entries.sort_by(|a, b| {
            b.info
                .updated_at
                .cmp(&a.info.updated_at)
                .then_with(|| b.info.id.cmp(&a.info.id))
        });
        entries
    }
}

/// One full index-converge pass over every registered workspace's bucket —
/// the shared body of [`EmbeddedServer::reconcile_session_index`] and the
/// spawned index-refresh driver (batch 1, US C1.1; a spawned task holds no
/// `&EmbeddedServer`, so both paths funnel through this free function).
///
/// The pass is incremental per session (an mtime-equal row is skipped) and
/// idempotent overall (full-snapshot row upserts), so the driver may run it
/// every tick and the startup converge may race a driver's first immediate
/// tick without harm. Best-effort throughout: every failure logs and the
/// caller (the next tick, the next start) retries.
///
/// Cross-process serialization (ADR-004, batch 2): the whole pass holds the
/// `<home>/session-index.reconcile.lock` file lock — the same
/// create_new + stale-takeover + bounded-wait PATTERN the workspace
/// registry's merged writes use, but a distinct lock FILE: the pass's own
/// workspace registration writes through to `workspaces.json`, whose write
/// takes the registry's `workspaces.json.lock` — nesting the same file
/// would deadlock. Acquired through `spawn_blocking` (the `create_new` +
/// 50 ms polling loop blocks). Two simultaneous passes (two engines ticking
/// at once) therefore run one after the other; a lock held longer than
/// the wait budget SKIPS this round (the degrade ADR-004 prescribes: the
/// reconcile is a fallback converger, the next tick retries — never
/// block or fail the caller).
pub(super) async fn reconcile_session_index_via(
    registry: &WorkspaceRegistry,
    sessions_root: &Path,
    index: &SessionIndex,
) {
    // The reconcile lock — a distinct file from workspaces.json.lock (see
    // the function doc: the pass's own registry write-through takes THAT
    // lock; the same file would nest into a deadlock). No home installed
    // (workspace-local engine, tests) → no cross-process surface to lock
    // over → the pass runs unlocked exactly as before.
    //
    // Staleness assumption: the takeover window (LOCK_STALE_TIMEOUT, 10 s)
    // presumes one pass finishes well inside 10 s. On a home with a very
    // large session count a pass may exceed it — a second process then
    // takes the "stale" lock over and the two passes interleave: the
    // posture degrades to the pre-batch-2 lock-free one, which the
    // idempotent full-snapshot row writes already tolerate (last writer
    // wins with converging content, the orphan pass re-deletes; see the
    // roadmap's batch 2 scope-cut entry).
    let Some(lock_path) = registry
        .session_index_home()
        .map(|home| mirri_session_index::lock_file_path(&home.join("session-index.reconcile")))
    else {
        reconcile_session_index_pass(registry, sessions_root, index).await;
        return;
    };
    let lock = tokio::task::spawn_blocking(move || {
        mirri_session_index::RegistryLock::new(lock_path.clone()).acquire()
    })
    .await;
    match lock {
        Ok(Ok(_guard)) => {
            reconcile_session_index_pass(registry, sessions_root, index).await;
        }
        Ok(Err(error)) => {
            // The lock file could not be created at all (permissions, a
            // stale residue that failed to be removed) — the pass's own
            // best-effort posture applies: skip and let the next tick retry.
            tracing::warn!(
                %error,
                "acquiring the reconcile lock failed; skipping this index \
                 reconcile round"
            );
        }
        Err(error) => {
            // The spawn_blocking join failed (panic/cancel) — never fail the
            // caller; the next tick retries.
            tracing::warn!(
                %error,
                "the reconcile lock task failed; skipping this index reconcile round"
            );
        }
    }
}

/// The lock-free body of [`reconcile_session_index_via`] — one converge
/// pass over every registered workspace's bucket (the caller holds the
/// reconcile lock, or decided there is no cross-process surface to lock
/// over).
async fn reconcile_session_index_pass(
    registry: &WorkspaceRegistry,
    sessions_root: &Path,
    index: &SessionIndex,
) {
    let io: Arc<dyn mirri_os::DynFileSystem> = Arc::new(mirri_os::LocalFs::new());

    let registrations = registry.registrations();
    for registration in &registrations {
        // Register (or touch) the workspace in the index — preserving an
        // existing registration's `created_at` and `sort_order` so a
        // restart does not reset either. The upsert writes through to
        // `workspaces.json`, so a TS engine sharing the home sees the
        // rust-registered workspaces too.
        if let Err(error) =
            EmbeddedServer::register_workspace_in_index(index, &registration.root, &registration.id)
        {
            tracing::warn!(
                %error,
                workspace = %registration.id,
                "registering the workspace in the session index failed"
            );
        }

        // Backfill / refresh: rows missing from the index or whose
        // `state.json` mtime moved are rebuilt from the validated disk
        // read — through a per-bucket reader (no runtime build).
        let bucket_persistence =
            mirri_core::SessionPersistence::new(sessions_root.to_path_buf(), Arc::clone(&io))
                .with_workspace_key(registration.id.clone());
        let bucket_dir = sessions_root.join(&registration.id);
        for (session_id, mtime) in EmbeddedServer::enumerate_bucket_sessions(&bucket_dir) {
            let existing = index.get_session(&session_id);
            let fresh = existing
                .as_ref()
                .is_some_and(|row| row.state_mtime == Some(mtime));
            if fresh {
                continue;
            }
            let Some(metadata) = bucket_persistence
                .read_persisted_summary(&SessionId::new(&session_id))
                .await
            else {
                continue;
            };
            // The unified last_prompt direction (N1): the NEW value wins —
            // here the freshly-read state.json (both engines persist it,
            // TS as `lastPrompt`, rust via the scope-layer round-trip) —
            // and the row's existing value is only the fallback for old
            // files that lack the key. Mirrors the live-path upsert
            // ([`EmbeddedServer::index_upsert_live_at`]), which prefers
            // the live scope metadata the same way.
            let last_prompt = metadata
                .last_prompt
                .clone()
                .or_else(|| existing.and_then(|row| row.last_prompt));
            let row = EmbeddedServer::session_row_from_metadata(
                &session_id,
                &registration.id,
                &registration.root,
                &metadata,
                last_prompt,
                Some(mtime),
            );
            if let Err(error) = index.upsert_session(&row) {
                tracing::warn!(
                    session_id = %session_id,
                    %error,
                    "backfilling the session index from disk failed for one session"
                );
            }
        }
    }

    // Orphan pass: rows of a workspace THIS registry knows whose session
    // directory no longer exists are deleted (the TS converge's sweep —
    // a session removed on disk must not linger in the listing). Rows of
    // workspaces this process does not know stay in place: their owning
    // engines (other homes' registries, the TS engine) sweep them, and
    // deleting another owner's row here would 404 its sessions.
    let known: HashSet<&str> = registrations
        .iter()
        .map(|registration| registration.id.as_str())
        .collect();
    let refs = match index.list_all_rows() {
        Ok(refs) => refs,
        Err(error) => {
            tracing::warn!(%error, "listing the session index rows for the orphan pass failed");
            return;
        }
    };
    for row_ref in refs {
        if !known.contains(row_ref.workspace_id.as_str()) {
            continue; // a foreign owner's row — its engines sweep it
        }
        let bucket_dir = sessions_root.join(&row_ref.workspace_id);
        if bucket_dir.join(&row_ref.id).exists() {
            continue;
        }
        if let Err(error) = index.delete_session(&row_ref.id) {
            tracing::warn!(
                session_id = %row_ref.id,
                %error,
                "deleting an orphaned session index row failed"
            );
        }
    }
}

/// One merged listing entry (W5): the session's summary (`SessionInfo`)
/// plus the fields the multi-workspace listing contributes — the recorded
/// cwd (persisted rows; live views compute their own) and the
/// `last_prompt` (SDR-004 — `None` for a never-conversed session, the
/// `exclude_empty` filter's input).
pub(super) struct MergedSessionEntry {
    pub info: SessionInfo,
    pub cwd: Option<String>,
    pub last_prompt: Option<String>,
}

/// ADR-009 turn-settle write-through: a turn just ended (completed or
/// failed). Refresh the session's recency and persist it — the same
/// `updatedAt: Date.now()` + write-through the TS kernel's
/// `SessionMetadataService.applyUpdate` performs — in three steps:
///
/// 1. bump the scope-layer `updated_at` (`SessionScope::update_metadata`
///    stamps it at the turn-END moment; the send path's own stamp predates
///    the turn, this one lands after it);
/// 2. write `state.json` through the owning workspace's persistence (the
///    same `save_state` the send path uses — the metadata round-trip keeps
///    `last_prompt`, ADR-009 / P1-2);
/// 3. upsert the SQLite index row (the `last_prompt` merges
///    new-value-first — the metadata's value wins, the row's is the
///    fallback, N1 — and `state_mtime` advances so the startup reconcile
///    does not roll the row back).
///
/// Best-effort throughout (US7.2): a persistence or index failure logs a
/// warning and never interrupts the turn result.
///
/// Shared by every turn driver — `message.send` (wait/fire/steer/abort),
/// the cron injector, and the background-task notification driver — so the
/// listing's "recently active" signal advances with turns (P1-1). The
/// subagent host (mirri-core) bumps only the in-memory recency: a subagent
/// turn always runs inside one of these outer drives, whose settle covers
/// the durable write-through.
pub(super) async fn note_turn_settled(
    index: Option<Arc<SessionIndex>>,
    registry: &WorkspaceRegistry,
    session: &SessionScope,
) {
    // 1. In-memory recency bump.
    session.update_metadata(|_| {}).await;
    let metadata = session.metadata().await;
    let session_id = session.id().clone();

    // 2. `state.json` write-through.
    let Some(owner) = workspace_owner_via(registry, session_id.as_str()) else {
        tracing::debug!(
            session_id = %session_id,
            "turn-settle state save skipped: no workspace owns the session"
        );
        return;
    };
    if let Err(error) = owner
        .manager
        .persistence()
        .save_state(&session_id, &metadata)
        .await
    {
        tracing::warn!(
            session_id = %session_id,
            %error,
            "turn-settle state save failed; the session recency stays in memory"
        );
    }

    // 3. Index write-through (the row's `last_prompt` and the fresh
    //    `state_mtime` keep the listing and the reconcile honest).
    EmbeddedServer::index_upsert_live_at(index.as_ref(), registry, &session_id, &metadata);
}
