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
//!   sessions root (the retired startup converge — ADR-006): rows
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

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use mirri_core::{SessionId, SessionInfo, SessionMetadata, SessionScope, SessionStatus};
use chrono::Utc;
use mirri_session_index::{ListOpts, SessionIndex, WorkspaceRow};

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

    /// The shared cron-index connection (ADR-020), delegated to the
    /// registry's lazy-opened `<home>/cron-index.db` — the same delegation
    /// precedent as [`Self::session_index`]: assembled engines share the
    /// registry's connection so every runtime's cron manager, the delete
    /// cascade, and the `cron.listAll` face read and write ONE database.
    /// `None` for the process lifetime when no home is injected — cron
    /// persistence stays off (tests, workspace-local engines).
    pub(super) fn shared_cron_index(&self) -> Option<Arc<mirri_cron_index::CronIndex>> {
        self.registry.shared_cron_index()
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
    pub(super) fn enumerate_bucket_sessions(bucket_dir: &Path) -> Vec<(String, i64)> {
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

    /// The retention-stamps row builder: the disk-derived paths (the
    /// converge and the one-time backfill) build their `SessionRow` from a
    /// scope-layer metadata plus the retention-domain stamps (§3.1 /
    /// ADR-006) — `archived_at` / `compressed` / `compressed_at` ride along
    /// the same row shape.
    #[allow(clippy::too_many_arguments)]
    /// Register (or touch) the bound workspace in the index — preserving an
    /// existing registration's `created_at` and `sort_order` so a restart
    /// does not reset either.
    pub(super) fn register_workspace_in_index(
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
    /// update, restore, close): write ONLY the fields the live metadata
    /// actually owns — `updated_at`, `title`, `tags`, `cwd`,
    /// `last_prompt`, `last_used_at`, and the row-existence upsert —
    /// through the owning workspace's metadata store port (ADR-006 裁决 4:
    /// field-level narrowing; the retention/pin columns are never named, so
    /// a concurrent writer's disjoint field survives). Routed to the owning
    /// workspace's store (W3 — a non-default workspace's session must not
    /// land in the default's rows). Best-effort — a failure logs and never
    /// fails the RPC that produced it.
    ///
    /// SDR-004 guard: a **lazily created session that has not been
    /// materialized** (no wire log — no user message yet) writes no row:
    /// an unmaterialized session must stay invisible to the persisted
    /// listing. The test is the WIRE LOG's presence (either shape), not
    /// state.json — a materialized row-only session (post-ADR-006's
    /// normal shape) carries no state.json.
    pub(super) fn index_upsert_live(&self, session_id: &SessionId, metadata: &SessionMetadata) {
        let index = self.session_index();
        Self::index_upsert_live_at(index.as_ref(), &self.registry, session_id, metadata);
    }

    /// The core of [`Self::index_upsert_live`] as a free function — shared
    /// with the turn-settle write-through ([`note_turn_settled`]) so the
    /// spawned turn tasks of the cron/task drivers (which hold no
    /// `&EmbeddedServer`) write the exact same columns.
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
        // The materialization guard (SDR-004): the wire log's presence —
        // either shape (plain or .gz) — is "the session conversed". A
        // state.json probe would miss a row-only session (post-ADR-006's
        // normal materialized shape: a row plus a wire log, no state.json)
        // and silently drop every write-through; the wire log is the
        // closest true test of the original "worth indexing" semantics.
        let persistence = owner.manager.persistence();
        let materialized = persistence.wire_log_path(session_id).is_file()
            || persistence.wire_log_path(session_id).with_extension("jsonl.gz").is_file();
        if !materialized {
            tracing::debug!(
                session_id = %session_id,
                "session-index write-through skipped: the session is not materialized yet"
            );
            return;
        }
        // ADR-006 裁决 4 — field-level narrowing: the write names ONLY the
        // scope-owned columns (title, tags, recency, last_prompt, cwd). The
        // row's retention stamps and pin columns are never named, so a
        // concurrent writer's disjoint field survives; an absent row is
        // INSERTed with the untouched columns defaulted (the same upsert
        // statement `SessionIndexPort::apply` compiles).
        let patch = mirri_core::session::metadata_port::MetadataPatch {
            title: Some(metadata.title.clone()),
            tags: Some(metadata.tags.iter().cloned().collect()),
            updated_at: Some(metadata.updated_at),
            last_used_at: Some(metadata.last_used_at),
            // N1 new-value-first (same direction as the startup backfill):
            // the live metadata carries the freshest last_prompt (the send
            // path stamps the scope, ADR-009); the row's value is the
            // fallback for metadata built without the key.
            last_prompt: Some(metadata.last_prompt.clone().or_else(|| {
                index
                    .get_session(session_id.as_str())
                    .and_then(|row| row.last_prompt)
            })),
            cwd: Some(metadata.work_dir.clone().or_else(|| {
                Some(owner.root.to_string_lossy().into_owned())
            })),
            ..Default::default()
        };        if let Err(error) =
            index.apply_session_patch(session_id.as_str(), &owner.workspace_id, &patch)
        {
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
    /// empty). Best-effort like [`Self::index_upsert_live`]; the
    /// materialized wire log is guaranteed by the caller (the send path
    /// materializes before this runs).
    ///
    /// ADR-006 裁决 4: the write names ONLY `last_prompt` — the row's every
    /// other column (a concurrent writer's retention stamp, pin, title,
    /// …) survives untouched.
    ///
    /// The row write routes through the OWNING MANAGER's metadata store
    /// (the source of truth — the production wiring's `SessionIndexPort`
    /// over the shared `<home>/session-index.db`), NOT the server's
    /// optional index handle: a no-index engine (no injected home) still
    /// owns its rows through the manager's store, and the send path must
    /// stamp them regardless.
    pub(super) async fn index_note_last_prompt(&self, session_id: &SessionId, prompt: &str) {
        let Some(owner) = workspace_owner_via(&self.registry, session_id.as_str()) else {
            tracing::debug!(
                session_id = %session_id,
                "session-index last-prompt write skipped: no workspace owns the session"
            );
            return;
        };
        // The materialization guard: the wire log's presence (either
        // shape) — NOT state.json, which a materialized row-only session
        // (post-ADR-006's normal shape) does not carry.
        let persistence = owner.manager.persistence();
        let materialized = persistence.wire_log_path(session_id).is_file()
            || persistence
                .wire_log_path(session_id)
                .with_extension("jsonl.gz")
                .is_file();
        if !materialized {
            tracing::debug!(
                session_id = %session_id,
                "session-index last-prompt write skipped: the session is not materialized yet"
            );
            return;
        }
        // 裁决 4 narrowing: only `last_prompt` is written. An existing row's
        // other columns keep whatever the last writer left there; an absent
        // row inserts with defaults.
        let patch = mirri_core::session::metadata_port::MetadataPatch {
            last_prompt: Some(Some(prompt.to_string())),
            ..Default::default()
        };
        if let Err(error) = owner
            .manager
            .metadata_store()
            .apply(session_id, &owner.manager.metadata_workspace_id(), patch)
            .await
        {
            tracing::warn!(
                session_id = %session_id,
                %error,
                "session-index last-prompt write-through failed"
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
                // ADR-008 (the row-side twin of the live filter below): a
                // scheduled-task execution session's row is ROUTABLE but
                // stays out of the listing until the user opens it —
                // under ADR-006 the row exists from creation (no wire log
                // yet), so the row itself must carry the exclusion. The
                // wire-log probe mirrors the live filter's materialization
                // check: an OPENED execution session (wire log installed)
                // lists normally.
                if row.tags.iter().any(|tag| tag.starts_with("cron-task:")) {
                    let materialized = match self.registry.manager_for(&row.id) {
                        crate::workspace_registry::ManagerResolution::Active { manager, .. }
                        | crate::workspace_registry::ManagerResolution::Restored { manager, .. } => {
                            let dir = manager
                                .persistence()
                                .session_dir(&mirri_core::SessionId::new(&row.id));
                            dir.join(mirri_persistence::compress::WIRE_LOG_FILE).exists()
                        }
                        _ => false,
                    };
                    if !materialized {
                        continue;
                    }
                }
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
                            archived_at: row.archived_at,
                            compressed: row.compressed,
                            compressed_at: row.compressed_at,
                            pre_compressed_size: None,
                            status: SessionStatus::Closed,
                            // The closed-row fallback (SDR-014): the SQLite
                            // index has no last_used_at column, so a closed
                            // session's sort key degrades to `updated_at` —
                            // a closed session has no touch entry point, so
                            // the exact last_used_at is of low value there.
                            last_used_at: None,
                        },
                        cwd: row.cwd,
                        last_prompt: row.last_prompt,
                        pinned: row.pinned,
                        pinned_at: row.pinned_at,
                        pin_color: row.pin_color,
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
            // ADR-008: a scheduled-task EXECUTION session is routable (so
            // its parked approval can be decided) but stays out of the
            // listing until the user explicitly opens it (materialization).
            // A user-created lazy session (SDR-004) carries no such tag and
            // is still listed.
            if live.tags.iter().any(|tag| tag.starts_with("cron-task:"))
                && let Some(session) = owner
                    .manager
                    .get_session(&mirri_core::SessionId::new(&live.id))
                    .await
                && !session.wire_log_installed().await
            {
                continue;
            }
            // The live row's last_prompt and pin trio come from the index —
            // the live `SessionInfo` carries no such state, and the send
            // path's write-through keeps the row warm (the scope's own
            // metadata stamps the same value, ADR-009; pins ride the store
            // port, SDR-016).
            let row = self.session_index().and_then(|index| index.get_session(&live.id));
            let last_prompt = row.as_ref().and_then(|row| row.last_prompt.clone());
            let (pinned, pinned_at, pin_color) = match row.as_ref() {
                Some(row) => (
                    row.pinned,
                    row.pinned_at,
                    row.pin_color.clone(),
                ),
                None => (false, None, None),
            };
            let info = live;
            // The merged view's carrier is SessionInfo's own fields — pin
            // rides the MergedSessionEntry instead (the listing view
            // composes both).
            by_id.insert(
                info.id.clone(),
                MergedSessionEntry {
                    last_prompt,
                    pinned,
                    pinned_at,
                    pin_color,
                    info,
                    cwd: None,
                },
            );
        }
        let mut entries: Vec<MergedSessionEntry> = by_id.into_values().collect();
        // The pinned group rides the top (SDR-016): within it, the latest
        // pin moment wins; everything else keeps the recency order.
        entries.sort_by(|a, b| {
            if a.pinned != b.pinned {
                return if a.pinned {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Greater
                };
            }
            if a.pinned && b.pinned {
                return b.pinned_at.cmp(&a.pinned_at);
            }
            b.info
                .updated_at
                .cmp(&a.info.updated_at)
                .then_with(|| b.info.id.cmp(&a.info.id))
        });
        entries
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
    /// The pin trio (SDR-016/017): overlaid from the index row for live
    /// entries (the static arm carries the row's own values).
    pub pinned: bool,
    pub pinned_at: Option<chrono::DateTime<chrono::Utc>>,
    pub pin_color: Option<String>,
}

/// ADR-009 turn-settle write-through: a turn just ended (completed or
/// failed). Refresh the session's recency and persist it — the same
/// `updatedAt: Date.now()` + write-through the TS kernel's
/// `SessionMetadataService.applyUpdate` performs — in three steps:
///
/// 1. bump the scope-layer `updated_at` (`SessionScope::update_metadata`
///    stamps it at the turn-END moment; the send path's own stamp predates
///    the turn, this one lands after it);
/// 2. write the metadata ROW through the owning workspace's metadata
///    store port (ADR-006: the row is the source of truth — the full field
///    set rides the patch, narrowing is Task 7's);
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
///
/// note: this bumps `updated_at` only — the sort key `last_used_at` is
/// reserved for user-interaction events (event-delivery ADR-003).
pub(super) async fn note_turn_settled(
    index: Option<Arc<SessionIndex>>,
    registry: &WorkspaceRegistry,
    session: &SessionScope,
) {
    // 1. In-memory recency bump.
    session.update_metadata(|_| {}).await;
    // ADR-008 / ADR-006 Task 13: an UN-MATERIALIZED session never writes
    // its row at settle — the row lands at materialization (the explicit
    // open), so a cron execution session's settled turn leaves the
    // row-exempt contract intact (the wire-log probe is the
    // materialization truth; a settled turn on an un-opened lazy session
    // has no disk presence to settle into).
    if !session.wire_log_installed().await {
        return;
    }
    let metadata = session.metadata().await;
    let session_id = session.id().clone();

    // 2. Metadata row write-through (ADR-006 裁决 4: the row is the source
    //    of truth and the write is field-level NARROWED — the settle names
    //    ONLY `updated_at`, so a concurrent writer's disjoint field (a
    //    retention stamp, a pin, a rename) survives the settle).
    let Some(owner) = workspace_owner_via(registry, session_id.as_str()) else {
        tracing::debug!(
            session_id = %session_id,
            "turn-settle metadata write skipped: no workspace owns the session"
        );
        return;
    };
    let patch = mirri_core::session::metadata_port::MetadataPatch {
        updated_at: Some(metadata.updated_at),
        ..Default::default()
    };
    if let Err(error) = owner
        .manager
        .metadata_store()
        .apply(
            &session_id,
            &owner.manager.metadata_workspace_id(),
            patch,
        )
        .await
    {
        tracing::warn!(
            session_id = %session_id,
            %error,
            "turn-settle metadata write failed; the session recency stays in memory"
        );
    }

    // 3. Index write-through (the row's `last_prompt` and the fresh
    //    `state_mtime` keep the listing and the reconcile honest).
    EmbeddedServer::index_upsert_live_at(index.as_ref(), registry, &session_id, &metadata);
}
